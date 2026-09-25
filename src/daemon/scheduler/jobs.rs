//! Scheduled jobs (DMN-114): the daemon's own list of "do X on schedule Y",
//! beyond the per-app backup policies the first scheduler increment ran.
//!
//! A job is a trigger ([`super::Schedule`] syntax, plus `hourly`) and one
//! action: reboot the node, start/stop/restart/update an app, back an app
//! up, run a shell command or send an HTTP request. Jobs live in
//! `<data_dir>/schedules.json` (0600 — an HTTP job may carry an
//! `Authorization` header), their run history in
//! `<data_dir>/schedule-runs.json` (the last [`MAX_RUNS_PER_JOB`] per job,
//! output capped at [`MAX_OUTPUT`]). Both files are only ever rewritten
//! whole, under an exclusive `flock` on `schedules.lock`, so the CLI editing
//! the list and the running daemon recording a run never interleave.
//!
//! Jobs come from two places: an operator (`asc schedule add`) and the
//! platform, which pushes its "run on the machine" schedules as
//! `managed_by = "platform"` with [`JobStore::replace_managed`] — a full
//! replace of its own jobs that never touches the operator's.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::Schedule;
use crate::daemon::apps::{AppManager, RuntimeState, UserContext};
use crate::daemon::backup;
use crate::daemon::backup::storage::{self, StorageList};
use crate::daemon::config::Config;
use crate::daemon::pkg;

/// How many runs are kept per job — enough for "what happened this week"
/// on an hourly job, small enough that the file stays a few hundred KiB.
pub const MAX_RUNS_PER_JOB: usize = 50;
/// Cap on captured output (shell stdout+stderr, HTTP response body) — the
/// same 64 KiB the platform keeps for its own HTTP schedule runs.
pub const MAX_OUTPUT: usize = 64 * 1024;
/// Default and ceiling for shell/HTTP job timeouts.
pub const DEFAULT_TIMEOUT_SECS: u32 = 600;
pub const MAX_TIMEOUT_SECS: u32 = 3600;

const JOBS_FILE: &str = "schedules.json";
const RUNS_FILE: &str = "schedule-runs.json";
const LOCK_FILE: &str = "schedules.lock";

fn yes() -> bool {
    true
}

fn default_method() -> String {
    "GET".to_string()
}

/// One scheduled job.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Job {
    /// `[A-Za-z0-9._-]{1,64}`; the platform uses its own schedule UUID.
    pub id: String,
    /// `hourly`, `daily@HH:MM`, `HH:MM` or a five-field cron expression.
    pub trigger: String,
    /// Evaluate the trigger in UTC instead of the node's local time — what
    /// the platform asks for, so its own `next_run_at` (UTC) matches.
    #[serde(default)]
    pub utc: bool,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub comment: String,
    /// `Some("platform")` for pushed jobs; `None` for the operator's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_by: Option<String>,
    pub action: JobAction,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub updated_at: i64,
}

/// What a job does when it fires. `app` is the daemon's app id (or custom
/// name — resolved the same way `asc app start <app>` resolves it).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JobAction {
    NodeReboot,
    AppStart {
        app: String,
    },
    AppStop {
        app: String,
    },
    AppRestart {
        app: String,
    },
    /// Upgrade to the latest version; a running app is stopped for the
    /// upgrade and started again afterwards.
    AppUpdate {
        app: String,
    },
    /// Back up to `storages` (empty = the app's own policy, else `local`),
    /// rotating each down to `keep` copies.
    Backup {
        app: String,
        #[serde(default)]
        storages: Vec<String>,
        #[serde(default)]
        keep: Option<u32>,
    },
    /// `/bin/sh -c <command>`; with `app`, in the app directory as the app's
    /// owner, otherwise in `/` as the daemon's own user.
    Shell {
        command: String,
        #[serde(default)]
        app: Option<String>,
        #[serde(default)]
        timeout_secs: Option<u32>,
    },
    Http {
        #[serde(default = "default_method")]
        method: String,
        url: String,
        #[serde(default)]
        headers: BTreeMap<String, String>,
        #[serde(default)]
        body: String,
        #[serde(default)]
        timeout_secs: Option<u32>,
    },
}

impl JobAction {
    /// Short machine label (`app_start`, `backup`, …) for tables and logs.
    pub fn label(&self) -> &'static str {
        match self {
            JobAction::NodeReboot => "node_reboot",
            JobAction::AppStart { .. } => "app_start",
            JobAction::AppStop { .. } => "app_stop",
            JobAction::AppRestart { .. } => "app_restart",
            JobAction::AppUpdate { .. } => "app_update",
            JobAction::Backup { .. } => "backup",
            JobAction::Shell { .. } => "shell",
            JobAction::Http { .. } => "http",
        }
    }

    /// The app this action targets, if any.
    pub fn app(&self) -> Option<&str> {
        match self {
            JobAction::AppStart { app }
            | JobAction::AppStop { app }
            | JobAction::AppRestart { app }
            | JobAction::AppUpdate { app }
            | JobAction::Backup { app, .. } => Some(app),
            JobAction::Shell { app, .. } => app.as_deref(),
            JobAction::NodeReboot | JobAction::Http { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunTrigger {
    Schedule,
    Manual,
}

/// One execution of a job.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunRecord {
    pub id: String,
    pub trigger: RunTrigger,
    pub status: RunStatus,
    pub started_at: i64,
    #[serde(default)]
    pub finished_at: Option<i64>,
    #[serde(default)]
    pub error: String,
    #[serde(default)]
    pub output: String,
    /// HTTP jobs: the response status (absent when no response came back).
    #[serde(default)]
    pub http_status: Option<u16>,
}

/// What executing a job produced.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Outcome {
    pub ok: bool,
    pub error: String,
    pub output: String,
    pub http_status: Option<u16>,
}

impl Outcome {
    fn failed(error: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: error.into(),
            ..Self::default()
        }
    }
}

pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// A fresh short id: 16 hex chars from the kernel's CSPRNG, falling back to
/// time + pid if `/dev/urandom` is somehow unreadable.
pub fn new_id() -> String {
    let mut bytes = [0u8; 8];
    let random = fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes));
    if random.is_err() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        bytes = (nanos ^ ((std::process::id() as u64) << 32)).to_le_bytes();
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Reject a job the scheduler could not run: a bad id or trigger, an empty
/// app/command/URL, an unsupported HTTP method, an absurd timeout.
pub fn validate(job: &Job) -> Result<()> {
    let id_ok = !job.id.is_empty()
        && job.id.len() <= 64
        && job
            .id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if !id_ok {
        bail!(
            "schedule id '{}' must be 1-64 characters of [A-Za-z0-9._-]",
            job.id
        );
    }
    Schedule::parse(&job.trigger)?;
    if let Some(app) = job.action.app()
        && app.trim().is_empty()
    {
        bail!(
            "schedule '{}': app is required for {}",
            job.id,
            job.action.label()
        );
    }
    let check_timeout = |t: &Option<u32>| -> Result<()> {
        if let Some(t) = t
            && (*t == 0 || *t > MAX_TIMEOUT_SECS)
        {
            bail!(
                "schedule '{}': timeout must be 1-{MAX_TIMEOUT_SECS} seconds",
                job.id
            );
        }
        Ok(())
    };
    match &job.action {
        JobAction::Shell {
            command,
            timeout_secs,
            ..
        } => {
            if command.trim().is_empty() {
                bail!("schedule '{}': shell command is empty", job.id);
            }
            check_timeout(timeout_secs)?;
        }
        JobAction::Http {
            method,
            url,
            timeout_secs,
            ..
        } => {
            if !matches!(
                method.to_ascii_uppercase().as_str(),
                "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD"
            ) {
                bail!("schedule '{}': unsupported HTTP method '{method}'", job.id);
            }
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                bail!(
                    "schedule '{}': URL must start with http:// or https://",
                    job.id
                );
            }
            check_timeout(timeout_secs)?;
        }
        JobAction::Backup { keep: Some(0), .. } => {
            bail!("schedule '{}': keep must be at least 1", job.id);
        }
        _ => {}
    }
    Ok(())
}

/// The next time `job` fires after `now` (unix seconds), `None` when it is
/// disabled or does not fire within a year.
pub fn next_run(job: &Job, now: i64) -> Option<i64> {
    if !job.enabled {
        return None;
    }
    Schedule::parse(&job.trigger).ok()?.next_after(now, job.utc)
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct JobsFile {
    #[serde(default)]
    jobs: Vec<Job>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct RunsFile {
    #[serde(default)]
    runs: HashMap<String, Vec<RunRecord>>,
}

/// Exclusive advisory lock over the job and run files; released on drop.
struct LockGuard(fs::File);

impl Drop for LockGuard {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        // SAFETY: the fd is owned by self.0 and still open.
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// The on-disk job list and run history of one daemon (`data_dir`).
#[derive(Debug, Clone)]
pub struct JobStore {
    dir: PathBuf,
}

impl JobStore {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            dir: data_dir.to_path_buf(),
        }
    }

    pub fn for_config(config: &Config) -> Self {
        Self::new(&config.daemon.data_dir)
    }

    fn lock(&self) -> Result<LockGuard> {
        use std::os::fd::AsRawFd;
        fs::create_dir_all(&self.dir)
            .with_context(|| format!("cannot create {}", self.dir.display()))?;
        let path = self.dir.join(LOCK_FILE);
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("cannot open {}", path.display()))?;
        // SAFETY: flock on an fd we own; blocks until the lock is ours.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(io::Error::last_os_error())
                .with_context(|| format!("cannot lock {}", path.display()));
        }
        Ok(LockGuard(file))
    }

    fn read<T: for<'de> Deserialize<'de> + Default>(&self, name: &str) -> Result<T> {
        let path = self.dir.join(name);
        match fs::read_to_string(&path) {
            Ok(raw) if raw.trim().is_empty() => Ok(T::default()),
            Ok(raw) => serde_json::from_str(&raw)
                .with_context(|| format!("invalid schedule file {}", path.display())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(T::default()),
            Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
        }
    }

    /// Write via a temp file + rename, so a crash mid-write never leaves a
    /// truncated list that would silently drop every job.
    fn write<T: Serialize>(&self, name: &str, value: &T) -> Result<()> {
        let path = self.dir.join(name);
        let tmp = self.dir.join(format!(".{name}.tmp"));
        let raw = serde_json::to_vec_pretty(value).context("cannot serialize schedules")?;
        {
            use std::io::Write;
            let mut file = fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .mode(0o600)
                .open(&tmp)
                .with_context(|| format!("cannot write {}", tmp.display()))?;
            file.write_all(&raw)
                .and_then(|_| file.sync_all())
                .with_context(|| format!("cannot write {}", tmp.display()))?;
        }
        fs::rename(&tmp, &path).with_context(|| format!("cannot replace {}", path.display()))
    }

    fn load_jobs(&self) -> Result<Vec<Job>> {
        Ok(self.read::<JobsFile>(JOBS_FILE)?.jobs)
    }

    fn save_jobs(&self, jobs: Vec<Job>) -> Result<()> {
        self.write(JOBS_FILE, &JobsFile { jobs })
    }

    pub fn list(&self) -> Result<Vec<Job>> {
        let _lock = self.lock()?;
        self.load_jobs()
    }

    pub fn get(&self, id: &str) -> Result<Option<Job>> {
        Ok(self.list()?.into_iter().find(|j| j.id == id))
    }

    /// Add or replace a job by id. Replacing keeps `created_at`; a job
    /// managed by someone else (or by nobody, when `job` is managed) is not
    /// taken over.
    pub fn upsert(&self, mut job: Job) -> Result<Job> {
        validate(&job)?;
        let _lock = self.lock()?;
        let mut jobs = self.load_jobs()?;
        let now = unix_now();
        job.updated_at = now;
        if let Some(existing) = jobs.iter_mut().find(|j| j.id == job.id) {
            if existing.managed_by != job.managed_by {
                bail!(
                    "schedule '{}' is managed by {} — refusing to replace it",
                    job.id,
                    existing.managed_by.as_deref().unwrap_or("the operator")
                );
            }
            job.created_at = existing.created_at;
            *existing = job.clone();
        } else {
            job.created_at = now;
            jobs.push(job.clone());
        }
        self.save_jobs(jobs)?;
        Ok(job)
    }

    /// Remove a job and its history; `false` when there was no such job.
    pub fn remove(&self, id: &str) -> Result<bool> {
        let _lock = self.lock()?;
        let mut jobs = self.load_jobs()?;
        let before = jobs.len();
        jobs.retain(|j| j.id != id);
        if jobs.len() == before {
            return Ok(false);
        }
        self.save_jobs(jobs)?;
        let mut runs: RunsFile = self.read(RUNS_FILE)?;
        if runs.runs.remove(id).is_some() {
            self.write(RUNS_FILE, &runs)?;
        }
        Ok(true)
    }

    pub fn set_enabled(&self, id: &str, enabled: bool) -> Result<Job> {
        let _lock = self.lock()?;
        let mut jobs = self.load_jobs()?;
        let job = jobs
            .iter_mut()
            .find(|j| j.id == id)
            .with_context(|| format!("schedule '{id}' not found"))?;
        job.enabled = enabled;
        job.updated_at = unix_now();
        let updated = job.clone();
        self.save_jobs(jobs)?;
        Ok(updated)
    }

    /// Replace every job managed by `managed_by` with `incoming` (whose
    /// `managed_by` is forced to the same value). Operator jobs stay; an
    /// incoming id that collides with an operator job is refused as a whole
    /// — nothing is written. History of dropped jobs is dropped too.
    pub fn replace_managed(&self, managed_by: &str, incoming: Vec<Job>) -> Result<Vec<Job>> {
        if managed_by.trim().is_empty() {
            bail!("managed_by must not be empty");
        }
        let mut incoming = incoming;
        for job in &mut incoming {
            job.managed_by = Some(managed_by.to_string());
            validate(job)?;
        }
        let _lock = self.lock()?;
        let jobs = self.load_jobs()?;
        let now = unix_now();
        let (managed, mut kept): (Vec<Job>, Vec<Job>) = jobs
            .into_iter()
            .partition(|j| j.managed_by.as_deref() == Some(managed_by));
        for job in &mut incoming {
            if kept.iter().any(|k| k.id == job.id) {
                bail!(
                    "schedule id '{}' is already used by a job not managed by {managed_by}",
                    job.id
                );
            }
            match managed.iter().find(|m| m.id == job.id) {
                Some(previous) => {
                    job.created_at = previous.created_at;
                    job.updated_at = if previous == job {
                        previous.updated_at
                    } else {
                        now
                    };
                }
                None => {
                    job.created_at = now;
                    job.updated_at = now;
                }
            }
        }
        let mut seen = std::collections::HashSet::new();
        if let Some(dup) = incoming.iter().find(|j| !seen.insert(j.id.clone())) {
            bail!("duplicate schedule id '{}'", dup.id);
        }
        let dropped: Vec<String> = managed
            .iter()
            .filter(|m| !incoming.iter().any(|j| j.id == m.id))
            .map(|m| m.id.clone())
            .collect();
        kept.extend(incoming.iter().cloned());
        self.save_jobs(kept)?;
        if !dropped.is_empty() {
            let mut runs: RunsFile = self.read(RUNS_FILE)?;
            for id in &dropped {
                runs.runs.remove(id);
            }
            self.write(RUNS_FILE, &runs)?;
        }
        Ok(incoming)
    }

    /// Runs of one job, newest first.
    pub fn runs(&self, id: &str, limit: usize) -> Result<Vec<RunRecord>> {
        let _lock = self.lock()?;
        let runs: RunsFile = self.read(RUNS_FILE)?;
        let mut list = runs.runs.get(id).cloned().unwrap_or_default();
        list.reverse();
        list.truncate(limit.clamp(1, MAX_RUNS_PER_JOB));
        Ok(list)
    }

    /// The latest run of every job, by job id.
    pub fn last_runs(&self) -> Result<HashMap<String, RunRecord>> {
        let _lock = self.lock()?;
        let runs: RunsFile = self.read(RUNS_FILE)?;
        Ok(runs
            .runs
            .into_iter()
            .filter_map(|(id, list)| list.last().cloned().map(|r| (id, r)))
            .collect())
    }

    /// Record a new run as `running`.
    pub fn begin_run(&self, id: &str, trigger: RunTrigger) -> Result<RunRecord> {
        let run = RunRecord {
            id: new_id(),
            trigger,
            status: RunStatus::Running,
            started_at: unix_now(),
            finished_at: None,
            error: String::new(),
            output: String::new(),
            http_status: None,
        };
        let _lock = self.lock()?;
        let mut runs: RunsFile = self.read(RUNS_FILE)?;
        let list = runs.runs.entry(id.to_string()).or_default();
        list.push(run.clone());
        if list.len() > MAX_RUNS_PER_JOB {
            let excess = list.len() - MAX_RUNS_PER_JOB;
            list.drain(..excess);
        }
        self.write(RUNS_FILE, &runs)?;
        Ok(run)
    }

    /// Close a run [`Self::begin_run`] opened.
    pub fn finish_run(&self, id: &str, run_id: &str, outcome: &Outcome) -> Result<RunRecord> {
        let _lock = self.lock()?;
        let mut runs: RunsFile = self.read(RUNS_FILE)?;
        let run = runs
            .runs
            .get_mut(id)
            .and_then(|list| list.iter_mut().find(|r| r.id == run_id))
            .with_context(|| format!("run '{run_id}' of schedule '{id}' not found"))?;
        run.status = if outcome.ok {
            RunStatus::Succeeded
        } else {
            RunStatus::Failed
        };
        run.finished_at = Some(unix_now());
        run.error = truncate(&outcome.error, MAX_OUTPUT);
        run.output = truncate(&outcome.output, MAX_OUTPUT);
        run.http_status = outcome.http_status;
        let finished = run.clone();
        self.write(RUNS_FILE, &runs)?;
        Ok(finished)
    }
}

/// Cut `s` to at most `max` bytes on a char boundary.
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Run one job now and return what happened. Never panics on a failing
/// action — every failure is an [`Outcome`] with `ok: false`.
pub fn execute(config: &Config, job: &Job) -> Outcome {
    let ctx = UserContext::current();
    let manager = AppManager::new(config);
    let result: Result<Outcome> = (|| match &job.action {
        JobAction::NodeReboot => {
            // Leave time for the run to be recorded (and an API response to
            // leave the machine) before systemd starts tearing things down.
            std::thread::spawn(|| {
                std::thread::sleep(Duration::from_secs(3));
                if cfg!(test) {
                    return;
                }
                match Command::new("systemctl")
                    .args(["reboot", "--no-wall"])
                    .status()
                {
                    Ok(status) if status.success() => {}
                    Ok(status) => tracing::warn!(%status, "scheduled reboot was rejected"),
                    Err(error) => tracing::warn!(%error, "could not start scheduled reboot"),
                }
            });
            Ok(ok_output("reboot requested"))
        }
        JobAction::AppStart { app } => {
            let outcome = manager.start(&ctx, app)?;
            Ok(ok_output(format!("{outcome:?}")))
        }
        JobAction::AppStop { app } => {
            let outcome = manager.stop(&ctx, app)?;
            Ok(ok_output(format!("{outcome:?}")))
        }
        JobAction::AppRestart { app } => {
            manager.restart(&ctx, app)?;
            Ok(ok_output("restarted"))
        }
        JobAction::AppUpdate { app } => {
            let status = manager.status(&ctx, app)?;
            let was_running = status.state == RuntimeState::Running;
            if was_running {
                manager.stop(&ctx, &status.meta.id)?;
            }
            let upgraded = pkg::upgrade(config, &ctx, &status.meta.id, None);
            // Bring the app back whatever the upgrade did: a failed upgrade
            // leaves the previous version in place, which should keep serving.
            let restarted = if was_running {
                manager.start(&ctx, &status.meta.id).map(|_| ())
            } else {
                Ok(())
            };
            let upgraded = upgraded?;
            restarted?;
            Ok(ok_output(format!("{upgraded:?}")))
        }
        JobAction::Backup {
            app,
            storages,
            keep,
        } => run_backup(config, &manager, &ctx, app, storages, *keep),
        JobAction::Shell {
            command,
            app,
            timeout_secs,
        } => {
            let target = match app {
                Some(app) => {
                    let meta = manager.get_authorized(&ctx, app)?;
                    let dir = manager.store().app_dir(&meta.id)?;
                    Some((meta.id.clone(), dir, meta.owner.uid))
                }
                None => None,
            };
            run_shell(
                command,
                target
                    .as_ref()
                    .map(|(id, dir, uid)| (id.as_str(), dir.as_path(), *uid)),
                timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS),
            )
        }
        JobAction::Http {
            method,
            url,
            headers,
            body,
            timeout_secs,
        } => run_http(
            method,
            url,
            headers,
            body,
            timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS),
        ),
    })();
    result.unwrap_or_else(|err| Outcome::failed(format!("{err:#}")))
}

fn ok_output(output: impl Into<String>) -> Outcome {
    Outcome {
        ok: true,
        output: output.into(),
        ..Outcome::default()
    }
}

fn run_backup(
    config: &Config,
    manager: &AppManager,
    ctx: &UserContext,
    app: &str,
    storages: &[String],
    keep: Option<u32>,
) -> Result<Outcome> {
    let meta = manager.get_authorized(ctx, app)?;
    let config_dir = manager.store().app_dir(&meta.id)?.join("config");
    let policy = crate::daemon::pkg::settings::SettingValues::load(&config_dir)?
        .backup_policy()?
        .unwrap_or_default();
    let targets = if !storages.is_empty() {
        storages.to_vec()
    } else if !policy.storages.is_empty() {
        policy.storages.clone()
    } else {
        vec![storage::LOCAL_NAME.to_string()]
    };
    let keep = keep.or(policy.keep);
    let list = StorageList::load()?;
    let results =
        backup::create_backup_multi(config, manager.store(), &meta, &list, &targets, keep);
    let mut lines = Vec::new();
    let mut errors = Vec::new();
    for (name, result) in results {
        match result {
            Ok(info) => lines.push(format!("{name}: {} ({} bytes)", info.name, info.bytes)),
            Err(err) => errors.push(format!("{name}: {err:#}")),
        }
    }
    Ok(Outcome {
        ok: errors.is_empty(),
        error: errors.join("\n"),
        output: lines.join("\n"),
        http_status: None,
    })
}

/// Primary gid of a uid from the passwd database.
fn primary_gid(uid: u32) -> Option<u32> {
    let mut buf = vec![0 as libc::c_char; 4096];
    // SAFETY: zeroed passwd is a valid out-parameter for getpwuid_r.
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: every pointer references a live buffer of the stated size.
    let rc = unsafe { libc::getpwuid_r(uid, &mut pwd, buf.as_mut_ptr(), buf.len(), &mut result) };
    (rc == 0 && !result.is_null()).then_some(pwd.pw_gid)
}

/// `/bin/sh -c`, stdout and stderr merged in order, killed (whole process
/// group) on timeout. With an app target the command runs in the app
/// directory as the app's owner — a platform user with rights on one app
/// must not get a root shell out of its schedule.
fn run_shell(command: &str, app: Option<(&str, &Path, u32)>, timeout_secs: u32) -> Result<Outcome> {
    let mut cmd = Command::new("/bin/sh");
    // `exec 2>&1` inside the shell merges the streams in write order,
    // which two separate pipes cannot reconstruct.
    cmd.arg("-c")
        .arg(format!("exec 2>&1\n{command}"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    match app {
        Some((id, dir, uid)) => {
            cmd.current_dir(dir)
                .env("ASC_APP_ID", id)
                .env("ASC_APP_DIR", dir);
            // SAFETY: geteuid has no preconditions.
            if unsafe { libc::geteuid() } == 0 && uid != 0 {
                let gid = primary_gid(uid)
                    .with_context(|| format!("app owner uid {uid} has no passwd entry"))?;
                cmd.uid(uid).gid(gid);
                if let Some(home) = crate::daemon::apps::home_for_uid(uid) {
                    cmd.env("HOME", home);
                }
            }
        }
        None => {
            cmd.current_dir("/");
        }
    }
    let mut child = cmd.spawn().context("cannot start /bin/sh")?;
    let mut stdout = child.stdout.take().context("no stdout pipe")?;
    let reader = std::thread::spawn(move || {
        let mut captured = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            match stdout.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                // Keep draining past the cap so the child never blocks on a
                // full pipe; only the first MAX_OUTPUT bytes are kept.
                Ok(n) => {
                    let room = MAX_OUTPUT.saturating_sub(captured.len());
                    captured.extend_from_slice(&chunk[..n.min(room)]);
                }
            }
        }
        captured
    });
    let deadline = Instant::now() + Duration::from_secs(timeout_secs as u64);
    let status = loop {
        if let Some(status) = child.try_wait().context("cannot wait for command")? {
            break Some(status);
        }
        if Instant::now() >= deadline {
            // SAFETY: negative pid = the process group we created above.
            unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let output = String::from_utf8_lossy(&reader.join().unwrap_or_default()).into_owned();
    Ok(match status {
        None => Outcome {
            ok: false,
            error: format!("timed out after {timeout_secs}s"),
            output,
            http_status: None,
        },
        Some(status) if status.success() => Outcome {
            ok: true,
            output,
            ..Outcome::default()
        },
        Some(status) => Outcome {
            ok: false,
            error: match status.code() {
                Some(code) => format!("exit code {code}"),
                None => format!("terminated: {status}"),
            },
            output,
            http_status: None,
        },
    })
}

fn run_http(
    method: &str,
    url: &str,
    headers: &BTreeMap<String, String>,
    body: &str,
    timeout_secs: u32,
) -> Result<Outcome> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(timeout_secs as u64)))
        .build()
        .into();
    let mut builder = ureq::http::Request::builder()
        .method(method.to_ascii_uppercase().as_str())
        .uri(url);
    for (key, value) in headers {
        builder = builder.header(key.as_str(), value.as_str());
    }
    let request = builder
        .body(body.as_bytes().to_vec())
        .context("invalid HTTP request")?;
    let mut response = match agent.run(request) {
        Ok(response) => response,
        Err(err) => return Ok(Outcome::failed(format!("request failed: {err}"))),
    };
    let status = response.status();
    let mut captured = Vec::new();
    let _ = response
        .body_mut()
        .with_config()
        .limit(u64::MAX)
        .reader()
        .take(MAX_OUTPUT as u64)
        .read_to_end(&mut captured);
    let output = String::from_utf8_lossy(&captured).into_owned();
    Ok(Outcome {
        ok: status.as_u16() < 400,
        error: if status.as_u16() < 400 {
            String::new()
        } else {
            format!("request returned {status}")
        },
        output,
        http_status: Some(status.as_u16()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(id: &str, action: JobAction) -> Job {
        Job {
            id: id.into(),
            trigger: "hourly".into(),
            utc: false,
            enabled: true,
            comment: String::new(),
            managed_by: None,
            action,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn validation_rejects_what_cannot_run() {
        assert!(validate(&job("ok-1", JobAction::NodeReboot)).is_ok());
        assert!(validate(&job("bad id", JobAction::NodeReboot)).is_err());
        assert!(validate(&job("", JobAction::NodeReboot)).is_err());
        assert!(validate(&job("a", JobAction::AppStart { app: " ".into() })).is_err());
        let mut bad_trigger = job("a", JobAction::NodeReboot);
        bad_trigger.trigger = "weekly".into();
        assert!(validate(&bad_trigger).is_err());
        let http = |method: &str, url: &str| {
            job(
                "h",
                JobAction::Http {
                    method: method.into(),
                    url: url.into(),
                    headers: BTreeMap::new(),
                    body: String::new(),
                    timeout_secs: None,
                },
            )
        };
        assert!(validate(&http("get", "https://example.com")).is_ok());
        assert!(validate(&http("TRACE", "https://example.com")).is_err());
        assert!(validate(&http("GET", "file:///etc/passwd")).is_err());
        let shell = |command: &str, timeout: Option<u32>| {
            job(
                "s",
                JobAction::Shell {
                    command: command.into(),
                    app: None,
                    timeout_secs: timeout,
                },
            )
        };
        assert!(validate(&shell("true", Some(10))).is_ok());
        assert!(validate(&shell("  ", None)).is_err());
        assert!(validate(&shell("true", Some(0))).is_err());
        assert!(validate(&shell("true", Some(MAX_TIMEOUT_SECS + 1))).is_err());
    }

    #[test]
    fn store_roundtrip_runs_and_managed_replace() {
        let dir = tempfile::tempdir().unwrap();
        let store = JobStore::new(dir.path());
        assert!(store.list().unwrap().is_empty());

        store.upsert(job("manual", JobAction::NodeReboot)).unwrap();
        let mut pushed = job("p1", JobAction::AppRestart { app: "web".into() });
        pushed.utc = true;
        store
            .replace_managed(
                "platform",
                vec![pushed.clone(), job("p2", JobAction::NodeReboot)],
            )
            .unwrap();
        assert_eq!(store.list().unwrap().len(), 3);

        // The operator's job cannot be taken over by a push…
        let err = store
            .replace_managed("platform", vec![job("manual", JobAction::NodeReboot)])
            .unwrap_err();
        assert!(err.to_string().contains("not managed"), "{err}");
        assert_eq!(
            store.list().unwrap().len(),
            3,
            "a refused replace writes nothing"
        );
        // …nor can a manual upsert replace a managed job.
        assert!(store.upsert(job("p1", JobAction::NodeReboot)).is_err());

        // Runs: begin/finish, newest first, dropped with the job.
        let run = store.begin_run("p2", RunTrigger::Manual).unwrap();
        assert_eq!(run.status, RunStatus::Running);
        let done = store
            .finish_run(
                "p2",
                &run.id,
                &Outcome {
                    ok: false,
                    error: "boom".into(),
                    ..Outcome::default()
                },
            )
            .unwrap();
        assert_eq!(done.status, RunStatus::Failed);
        assert_eq!(store.runs("p2", 10).unwrap().len(), 1);
        assert_eq!(store.last_runs().unwrap()["p2"].error, "boom");

        // A push without p2 drops p2 and its history; p1 keeps created_at.
        let created = store.get("p1").unwrap().unwrap().created_at;
        store.replace_managed("platform", vec![pushed]).unwrap();
        let jobs = store.list().unwrap();
        assert_eq!(jobs.len(), 2);
        assert_eq!(store.get("p1").unwrap().unwrap().created_at, created);
        assert!(store.runs("p2", 10).unwrap().is_empty());
        assert_eq!(
            store.get("p1").unwrap().unwrap().managed_by.as_deref(),
            Some("platform")
        );

        let disabled = store.set_enabled("manual", false).unwrap();
        assert!(!disabled.enabled);
        assert_eq!(next_run(&disabled, 0), None);
        assert!(store.remove("manual").unwrap());
        assert!(!store.remove("manual").unwrap());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.path().join(JOBS_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn run_history_is_capped() {
        let dir = tempfile::tempdir().unwrap();
        let store = JobStore::new(dir.path());
        for _ in 0..(MAX_RUNS_PER_JOB + 5) {
            store.begin_run("j", RunTrigger::Schedule).unwrap();
        }
        assert_eq!(store.runs("j", 1000).unwrap().len(), MAX_RUNS_PER_JOB);
    }

    #[test]
    fn shell_jobs_capture_output_exit_codes_and_timeouts() {
        let ok = run_shell("echo out; echo err >&2", None, 10).unwrap();
        assert!(ok.ok, "{ok:?}");
        assert!(ok.output.contains("out") && ok.output.contains("err"));

        let failed = run_shell("exit 3", None, 10).unwrap();
        assert!(!failed.ok);
        assert_eq!(failed.error, "exit code 3");

        let started = Instant::now();
        let slow = run_shell("sleep 30", None, 1).unwrap();
        assert!(!slow.ok);
        assert!(slow.error.contains("timed out"), "{slow:?}");
        assert!(started.elapsed() < Duration::from_secs(10));

        let big = run_shell("head -c 200000 /dev/zero | tr '\\0' 'x'", None, 10).unwrap();
        assert!(big.ok);
        assert_eq!(big.output.len(), MAX_OUTPUT);
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        let s = "яяя"; // 2 bytes per char
        assert_eq!(truncate(s, 3), "я");
        assert_eq!(truncate(s, 100), s);
    }
}
