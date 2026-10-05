//! The node's firewall (DMN-148, DMN-149): nftables through one table the
//! daemon owns, `inet asc`, built from a small model (settings, rules, IP
//! sets), applied with an automatic rollback, plus an expert RAW mode where
//! the operator's text is the whole ruleset. See docs/english/firewall.md.
//!
//! State lives in two files under `/var/lib/asc/firewall`: `state.json` (the
//! model, the mode and what was last confirmed) and `pending.json` (a change
//! that is applied but not yet confirmed, together with the script that
//! undoes it). The confirmed ruleset is `/etc/asc/firewall/ruleset.nft`,
//! loaded at boot by `asc-firewall.service`, so a reboot always returns to the
//! last confirmed state. Only the root daemon manages the firewall.

pub mod conflicts;
pub mod host;
pub mod model;
pub mod nft;
pub mod persist;
pub mod render;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::daemon::exec::{self, Progress, invalid, not_found, precondition};
use crate::daemon::webserver::{unix_now, write_atomic};
use conflicts::Conflict;
use host::Facts;
use model::{
    ALL_PRESETS, CRITICAL_PRESETS, MAX_RULES, MAX_SET_ENTRIES, Mode, Model, Rule, SetEntry,
    SetName, Settings,
};
use nft::{Counter, MAX_TABLE_TEXT, Nft, SystemNft};
pub use persist::Paths;

/// How long a change waits for confirmation before it is rolled back.
pub const MIN_TIMEOUT: u64 = 15;
pub const MAX_TIMEOUT: u64 = 600;
pub const DEFAULT_TIMEOUT: u64 = 60;

/// What the daemon keeps between calls.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    /// The confirmed mode — what is actually in force.
    pub mode: Mode,
    pub model: Model,
    /// The operator's text for RAW mode (the draft until it is confirmed).
    pub raw: String,
    /// The table text as it was applied and confirmed, for the diff.
    pub applied_table: String,
    /// Fingerprint of what was confirmed; compared with the model to tell
    /// whether there are unapplied changes.
    pub applied_fp: String,
    pub applied_unix: i64,
    pub last_error: String,
}

/// A change that is in the kernel but not confirmed yet.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pending {
    pub id: String,
    pub started_unix: i64,
    pub deadline_unix: i64,
    pub target_mode: Mode,
    /// The script that was applied; it becomes the boot ruleset on confirm.
    pub script: String,
    pub table: String,
    pub fingerprint: String,
    /// The script that puts the previous state back.
    pub rollback: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PendingView {
    pub id: String,
    pub deadline_unix: i64,
    pub seconds_left: i64,
    pub target_mode: Mode,
}

/// Everything `GetFirewall` reports.
#[derive(Debug, Clone, Serialize)]
pub struct Overview {
    pub nft_installed: bool,
    pub nft_version: String,
    pub mode: Mode,
    pub settings: Settings,
    pub rules: Vec<Rule>,
    /// The rules the daemon derives from the host; read-only.
    pub presets: Vec<Rule>,
    pub sets: model::IpSets,
    pub pending: Option<PendingView>,
    pub conflicts: Vec<Conflict>,
    /// The model differs from what is in force.
    pub changed: bool,
    /// Matched packets and bytes per rule id.
    pub counters: BTreeMap<String, Counter>,
    pub last_error: String,
    pub applied_unix: i64,
}

/// What `RenderFirewall` returns: the table that would be applied next to
/// the one in force.
#[derive(Debug, Clone, Serialize)]
pub struct Rendered {
    pub proposed: String,
    pub applied: String,
    pub changed: bool,
}

/// `ListTables`: one table of the host, read-only.
#[derive(Debug, Clone, Serialize)]
pub struct TableView {
    pub family: String,
    pub name: String,
    /// `asc`, `fail2ban`, `iptables` (iptables-nft's tables, Docker's among
    /// them) or `other`.
    pub owner: String,
    pub text: String,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct RulesetView {
    /// `nft list ruleset` as the kernel has it.
    pub live: String,
    /// The operator's RAW text, if any.
    pub raw: String,
}

/// The parts that touch the host and are replaced in tests.
#[derive(Clone, Copy)]
pub struct Hooks {
    pub detect_conflicts: fn() -> Vec<Conflict>,
    pub disable_conflicts: fn(&[Conflict]) -> Result<()>,
}

impl Default for Hooks {
    fn default() -> Self {
        Self {
            detect_conflicts: conflicts::detect,
            disable_conflicts: conflicts::disable,
        }
    }
}

pub struct Firewall {
    paths: Paths,
    nft: Box<dyn Nft>,
    hooks: Hooks,
    lock: Mutex<()>,
}

/// One change ready to go to the kernel.
struct Change {
    target_mode: Mode,
    /// What `applied_table` becomes.
    table: String,
    script: String,
    fingerprint: String,
    rollback: String,
}

impl Firewall {
    pub fn new() -> Self {
        Self::with(Paths::system(), Box::new(SystemNft), Hooks::default())
    }

    pub fn with(paths: Paths, nft: Box<dyn Nft>, hooks: Hooks) -> Self {
        Self {
            paths,
            nft,
            hooks,
            lock: Mutex::new(()),
        }
    }

    fn guard(&self) -> MutexGuard<'_, ()> {
        self.lock
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    // ── State files ─────────────────────────────────────────────────────────

    fn load_state(&self) -> State {
        let path = self.paths.state_file();
        let Ok(text) = std::fs::read_to_string(&path) else {
            return State::default();
        };
        match serde_json::from_str(&text) {
            Ok(state) => state,
            Err(err) => {
                // Keep the broken file for the operator instead of silently
                // starting over and losing their rules.
                let aside = path.with_extension("json.corrupt");
                warn!(error = %err, "firewall state is unreadable; moved to {}", aside.display());
                let _ = std::fs::rename(&path, &aside);
                State::default()
            }
        }
    }

    fn save_state(&self, state: &State) -> Result<()> {
        let text =
            serde_json::to_string_pretty(state).context("cannot encode the firewall state")?;
        write_atomic(&self.paths.state_file(), text.as_bytes(), 0o600)
            .context("cannot save the firewall state")
    }

    fn load_pending(&self) -> Option<Pending> {
        let text = std::fs::read_to_string(self.paths.pending_file()).ok()?;
        match serde_json::from_str(&text) {
            Ok(pending) => Some(pending),
            Err(err) => {
                warn!(error = %err, "pending firewall change is unreadable");
                None
            }
        }
    }

    fn save_pending(&self, pending: &Pending) -> Result<()> {
        let text = serde_json::to_string_pretty(pending).context("cannot encode the change")?;
        write_atomic(&self.paths.pending_file(), text.as_bytes(), 0o600)
            .context("cannot save the pending firewall change")
    }

    fn clear_pending(&self) {
        let _ = std::fs::remove_file(self.paths.pending_file());
    }

    // ── Reading ─────────────────────────────────────────────────────────────

    pub fn overview(&self, facts: &Facts) -> Result<Overview> {
        let _guard = self.guard();
        self.overview_locked(facts)
    }

    fn overview_locked(&self, facts: &Facts) -> Result<Overview> {
        let state = self.load_state();
        let now = unix_now();
        let installed = self.nft.installed();
        let counters = if installed && state.mode == Mode::Managed {
            self.nft
                .table_json(render::TABLE_FAMILY, render::TABLE_NAME)
                .ok()
                .flatten()
                .map(|json| nft::parse_counters(&json))
                .unwrap_or_default()
        } else {
            BTreeMap::new()
        };
        let changed = match state.mode {
            Mode::Managed => render::render(&state.model, facts, 0) != state.applied_fp,
            Mode::Raw => state.raw != state.applied_fp,
            Mode::Disabled => false,
        };
        Ok(Overview {
            nft_installed: installed,
            nft_version: if installed {
                self.nft.version()
            } else {
                String::new()
            },
            mode: state.mode,
            presets: host::presets(facts, &state.model.settings),
            settings: state.model.settings.clone(),
            rules: state.model.rules.clone(),
            sets: state.model.sets.clone(),
            pending: self.load_pending().map(|p| PendingView {
                seconds_left: (p.deadline_unix - now).max(0),
                id: p.id,
                deadline_unix: p.deadline_unix,
                target_mode: p.target_mode,
            }),
            conflicts: (self.hooks.detect_conflicts)(),
            changed,
            counters,
            last_error: state.last_error,
            applied_unix: state.applied_unix,
        })
    }

    pub fn render(&self, facts: &Facts) -> Result<Rendered> {
        let _guard = self.guard();
        let state = self.load_state();
        let proposed = render::render(&state.model, facts, unix_now());
        Ok(Rendered {
            changed: render::render(&state.model, facts, 0) != state.applied_fp,
            proposed,
            applied: state.applied_table,
        })
    }

    pub fn ruleset(&self) -> Result<RulesetView> {
        let _guard = self.guard();
        self.require_nft()?;
        Ok(RulesetView {
            live: self.nft.list_ruleset()?,
            raw: self.load_state().raw,
        })
    }

    pub fn tables(&self) -> Result<Vec<TableView>> {
        let _guard = self.guard();
        self.require_nft()?;
        let mut out = Vec::new();
        for (family, name) in nft::parse_tables(&self.nft.tables_json()?) {
            let text = self
                .nft
                .list_table(&family, &name)
                .ok()
                .flatten()
                .unwrap_or_default();
            let truncated = text.len() > MAX_TABLE_TEXT;
            let text = if truncated {
                text.chars().take(MAX_TABLE_TEXT).collect()
            } else {
                text
            };
            out.push(TableView {
                owner: table_owner(&family, &name).into(),
                family,
                name,
                text,
                truncated,
            });
        }
        Ok(out)
    }

    fn require_nft(&self) -> Result<()> {
        if self.nft.installed() {
            Ok(())
        } else {
            Err(precondition(
                "nftables is not installed on this node (run `asc firewall install`)",
            ))
        }
    }

    // ── Editing the model ───────────────────────────────────────────────────

    pub fn update_settings(&self, settings: Settings, force: bool) -> Result<Settings> {
        settings.validate().map_err(invalid)?;
        let _guard = self.guard();
        let mut state = self.load_state();
        let newly_off: Vec<&String> = settings
            .disabled_presets
            .iter()
            .filter(|p| !state.model.settings.disabled_presets.contains(p))
            .filter(|p| CRITICAL_PRESETS.contains(&p.as_str()))
            .collect();
        if !newly_off.is_empty() && !force {
            return Err(precondition(format!(
                "switching off the {} preset can lock you out of the node; repeat with force to confirm",
                newly_off
                    .iter()
                    .map(|p| p.as_str())
                    .collect::<Vec<_>>()
                    .join(" and ")
            )));
        }
        state.model.settings = settings;
        self.save_state(&state)?;
        Ok(state.model.settings)
    }

    pub fn upsert_rule(&self, mut rule: Rule) -> Result<Rule> {
        if rule.is_preset() {
            return Err(invalid(
                "preset rules are derived from the host and cannot be edited",
            ));
        }
        rule.normalize().map_err(invalid)?;
        let _guard = self.guard();
        let mut state = self.load_state();
        if rule.id.is_empty() {
            rule.id = new_id("r");
        }
        match state.model.rules.iter_mut().find(|r| r.id == rule.id) {
            Some(existing) => *existing = rule.clone(),
            None => {
                if state.model.rules.len() >= MAX_RULES {
                    return Err(invalid(format!("at most {MAX_RULES} rules are allowed")));
                }
                state.model.rules.push(rule.clone());
            }
        }
        self.save_state(&state)?;
        Ok(rule)
    }

    pub fn remove_rule(&self, id: &str) -> Result<bool> {
        let _guard = self.guard();
        let mut state = self.load_state();
        let before = state.model.rules.len();
        state.model.rules.retain(|r| r.id != id);
        let removed = state.model.rules.len() != before;
        if removed {
            self.save_state(&state)?;
        }
        Ok(removed)
    }

    /// Replaces one set with `entries`.
    pub fn set_entries(&self, name: SetName, entries: Vec<SetEntry>) -> Result<Vec<SetEntry>> {
        let entries = normalize_entries(entries)?;
        let _guard = self.guard();
        let mut state = self.load_state();
        *state.model.sets.get_mut(name) = entries.clone();
        self.save_state(&state)?;
        Ok(entries)
    }

    /// Adds (or refreshes) one address in a set.
    pub fn add_entry(&self, name: SetName, mut entry: SetEntry) -> Result<Vec<SetEntry>> {
        entry.normalize().map_err(invalid)?;
        let _guard = self.guard();
        let mut state = self.load_state();
        let list = state.model.sets.get_mut(name);
        match list.iter_mut().find(|e| e.value == entry.value) {
            Some(existing) => *existing = entry,
            None => list.push(entry),
        }
        if list.len() > MAX_SET_ENTRIES {
            return Err(invalid(format!(
                "a set holds at most {MAX_SET_ENTRIES} entries"
            )));
        }
        let list = list.clone();
        self.save_state(&state)?;
        Ok(list)
    }

    pub fn remove_entry(&self, name: SetName, value: &str) -> Result<bool> {
        let value = model::parse_net(value).map_err(invalid)?.to_string();
        let _guard = self.guard();
        let mut state = self.load_state();
        let list = state.model.sets.get_mut(name);
        let before = list.len();
        list.retain(|e| e.value != value);
        let removed = list.len() != before;
        if removed {
            self.save_state(&state)?;
        }
        Ok(removed)
    }

    // ── Applying ────────────────────────────────────────────────────────────

    /// Installs the `nftables` package.
    pub fn install_nft(&self, progress: Progress<'_>) -> Result<()> {
        let _guard = self.guard();
        if self.nft.installed() {
            progress("nftables is already installed");
            return Ok(());
        }
        exec::install_package("nftables", progress)
    }

    /// Applies the managed model. `timeout_secs` is the confirmation window
    /// (`0` commits at once); `force` switches off a competing ufw/firewalld.
    pub fn apply(
        self: &Arc<Self>,
        facts: &Facts,
        timeout_secs: u64,
        force: bool,
    ) -> Result<Overview> {
        check_timeout(timeout_secs)?;
        let _guard = self.guard();
        self.require_nft()?;
        self.require_no_pending()?;
        let mut state = self.load_state();

        // A second firewall owner with a drop policy would override ours, so
        // enabling next to one is refused unless the operator says to switch
        // it off. It is switched off only after the ruleset passed `nft -c`.
        let competing = if state.mode == Mode::Managed {
            Vec::new()
        } else {
            (self.hooks.detect_conflicts)()
        };
        if !competing.is_empty() && !force {
            return Err(precondition(format!(
                "{} is active on this node and would override the firewall; stop it first or repeat with force",
                competing
                    .iter()
                    .map(|c| c.kind.as_str())
                    .collect::<Vec<_>>()
                    .join(" and ")
            )));
        }

        let now = unix_now();
        for name in [SetName::Allowlist, SetName::Blocklist] {
            state.model.sets.get_mut(name).retain(|e| !e.expired(now));
        }
        let table = render::render(&state.model, facts, now);
        let change = Change {
            target_mode: Mode::Managed,
            script: render::replace_script(&table),
            fingerprint: render::render(&state.model, facts, 0),
            rollback: self.rollback_for(&state, Mode::Managed)?,
            table,
        };
        self.nft
            .check(&change.script)
            .map_err(|err| precondition(format!("{err:#}")))?;
        if !competing.is_empty() {
            (self.hooks.disable_conflicts)(&competing)?;
        }
        self.save_state(&state)?;
        self.submit(&mut state, change, timeout_secs)?;
        self.overview_locked(facts)
    }

    /// Replaces the whole ruleset with the operator's text (RAW mode).
    pub fn apply_raw(
        self: &Arc<Self>,
        facts: &Facts,
        text: &str,
        ack_risk: bool,
        timeout_secs: u64,
    ) -> Result<Overview> {
        if !ack_risk {
            return Err(precondition(
                "RAW mode replaces the whole nftables ruleset, Docker's and fail2ban's tables included; acknowledge the risk to continue",
            ));
        }
        if text.trim().is_empty() {
            return Err(invalid(
                "the ruleset is empty; use disable to remove the firewall",
            ));
        }
        if text.len() > 1024 * 1024 {
            return Err(invalid("the ruleset is larger than 1 MiB"));
        }
        check_timeout(timeout_secs)?;
        let _guard = self.guard();
        self.require_nft()?;
        self.require_no_pending()?;
        let mut state = self.load_state();
        let text = text.replace("\r\n", "\n");
        let change = Change {
            target_mode: Mode::Raw,
            script: format!("flush ruleset\n{text}\n"),
            fingerprint: text.clone(),
            rollback: self.rollback_for(&state, Mode::Raw)?,
            table: text.clone(),
        };
        state.raw = text;
        self.save_state(&state)?;
        self.submit(&mut state, change, timeout_secs)?;
        self.overview_locked(facts)
    }

    /// The script that undoes a change to `target`. A change to the managed
    /// table only needs that table back; anything touching RAW needs the
    /// whole ruleset, because RAW flushes it.
    fn rollback_for(&self, state: &State, target: Mode) -> Result<String> {
        if target == Mode::Raw || state.mode == Mode::Raw {
            let dump = self.nft.list_ruleset()?;
            return Ok(format!("flush ruleset\n{dump}\n"));
        }
        Ok(
            match self
                .nft
                .list_table(render::TABLE_FAMILY, render::TABLE_NAME)?
            {
                Some(table) => render::replace_script(&table),
                None => render::remove_script(),
            },
        )
    }

    fn require_no_pending(&self) -> Result<()> {
        if self.load_pending().is_some() {
            return Err(precondition(
                "a firewall change is waiting for confirmation; confirm it or roll it back first",
            ));
        }
        Ok(())
    }

    /// Checks, applies and either commits (no window) or leaves the change
    /// pending with its rollback armed.
    fn submit(
        self: &Arc<Self>,
        state: &mut State,
        change: Change,
        timeout_secs: u64,
    ) -> Result<()> {
        self.nft
            .check(&change.script)
            .map_err(|err| precondition(format!("{err:#}")))?;
        if timeout_secs == 0 {
            self.nft.apply(&change.script)?;
            return self.commit(state, &change);
        }
        let now = unix_now();
        let pending = Pending {
            id: new_id("c"),
            started_unix: now,
            deadline_unix: now + timeout_secs as i64,
            target_mode: change.target_mode,
            script: change.script.clone(),
            table: change.table,
            fingerprint: change.fingerprint,
            rollback: change.rollback,
        };
        // The way back is on disk before the change is in the kernel.
        self.save_pending(&pending)?;
        if let Err(err) = self.nft.apply(&change.script) {
            self.clear_pending();
            return Err(err);
        }
        info!(id = %pending.id, seconds = timeout_secs, "firewall change applied; waiting for confirmation");
        self.arm(pending.id, pending.deadline_unix);
        Ok(())
    }

    fn commit(&self, state: &mut State, change: &Change) -> Result<()> {
        self.commit_parts(
            state,
            change.target_mode,
            &change.table,
            &change.fingerprint,
            &change.script,
        )
    }

    fn commit_parts(
        &self,
        state: &mut State,
        mode: Mode,
        table: &str,
        fingerprint: &str,
        script: &str,
    ) -> Result<()> {
        state.mode = mode;
        state.applied_table = table.to_string();
        state.applied_fp = fingerprint.to_string();
        state.applied_unix = unix_now();
        state.last_error.clear();
        self.save_state(state)?;
        persist::save_ruleset(&self.paths, &self.nft.path(), script)
    }

    /// Confirms the pending change (`id` may be empty for "the current one").
    pub fn confirm(&self, id: &str, facts: &Facts) -> Result<Overview> {
        let _guard = self.guard();
        let Some(pending) = self.load_pending() else {
            return Err(precondition(
                "no firewall change is waiting for confirmation",
            ));
        };
        if !id.is_empty() && id != pending.id {
            return Err(precondition(
                "that change is no longer pending (it was replaced, confirmed or rolled back)",
            ));
        }
        if unix_now() > pending.deadline_unix {
            self.rollback_locked(&pending, "was not confirmed in time")?;
            return Err(precondition(
                "the confirmation window passed; the change was rolled back",
            ));
        }
        let mut state = self.load_state();
        self.commit_parts(
            &mut state,
            pending.target_mode,
            &pending.table,
            &pending.fingerprint,
            &pending.script,
        )?;
        self.clear_pending();
        info!(id = %pending.id, "firewall change confirmed");
        self.overview_locked(facts)
    }

    /// Rolls the pending change back now.
    pub fn rollback(&self, facts: &Facts) -> Result<Overview> {
        let _guard = self.guard();
        let Some(pending) = self.load_pending() else {
            return Err(precondition(
                "no firewall change is waiting for confirmation",
            ));
        };
        self.rollback_locked(&pending, "was rolled back by request")?;
        self.overview_locked(facts)
    }

    fn rollback_locked(&self, pending: &Pending, why: &str) -> Result<()> {
        self.nft
            .apply(&pending.rollback)
            .context("cannot restore the previous ruleset")?;
        self.clear_pending();
        let mut state = self.load_state();
        state.last_error = format!("the firewall change {} {why}", pending.id);
        let _ = self.save_state(&state);
        warn!(id = %pending.id, "{why}; the previous ruleset is back");
        Ok(())
    }

    /// Rolls the pending change back if its window is over at `now`. Returns
    /// whether it did.
    pub fn expire(&self, id: &str, now: i64) -> Result<bool> {
        let _guard = self.guard();
        let Some(pending) = self.load_pending() else {
            return Ok(false);
        };
        if pending.id != id || now < pending.deadline_unix {
            return Ok(false);
        }
        self.rollback_locked(&pending, "was not confirmed in time")?;
        Ok(true)
    }

    /// Starts the timer that rolls the change back when nobody confirms.
    fn arm(self: &Arc<Self>, id: String, deadline: i64) {
        let this = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("firewall-rollback".into())
            .spawn(move || {
                loop {
                    let now = unix_now();
                    if now >= deadline {
                        break;
                    }
                    if this.load_pending().is_none_or(|p| p.id != id) {
                        return;
                    }
                    let left = Duration::from_secs((deadline - now) as u64);
                    std::thread::sleep(left.min(Duration::from_millis(500)));
                }
                for attempt in 1..=12 {
                    match this.expire(&id, unix_now()) {
                        Ok(_) => return,
                        Err(err) => {
                            warn!(attempt, error = %format!("{err:#}"), "firewall rollback failed; retrying");
                            std::thread::sleep(Duration::from_secs(5));
                        }
                    }
                }
            });
        if let Err(err) = spawned {
            // Without the timer the change stays pending until `recover` or
            // an explicit rollback; say so loudly.
            warn!(error = %err, "cannot start the firewall rollback timer");
        }
    }

    /// Switches the managed firewall off: the table is removed and nothing is
    /// loaded at boot. Leaving RAW mode keeps whatever the text loaded.
    pub fn disable(&self, facts: &Facts) -> Result<Overview> {
        let _guard = self.guard();
        self.require_no_pending()?;
        let mut state = self.load_state();
        if state.mode == Mode::Managed && self.nft.installed() {
            self.nft.apply(&render::remove_script())?;
        }
        persist::clear_ruleset(&self.paths)?;
        state.mode = Mode::Disabled;
        state.applied_table.clear();
        state.applied_fp.clear();
        state.last_error.clear();
        self.save_state(&state)?;
        self.overview_locked(facts)
    }

    /// Start-up: finish what a restart interrupted. An expired pending change
    /// is rolled back; a live one gets its timer back; a confirmed managed
    /// table that is missing from the kernel is loaded again.
    pub fn recover(self: &Arc<Self>) {
        let _guard = self.guard();
        if let Some(pending) = self.load_pending() {
            if unix_now() >= pending.deadline_unix {
                if let Err(err) =
                    self.rollback_locked(&pending, "was not confirmed before a restart")
                {
                    warn!(error = %format!("{err:#}"), "cannot roll back the interrupted firewall change");
                }
            } else {
                self.arm(pending.id, pending.deadline_unix);
            }
            return;
        }
        let state = self.load_state();
        if state.mode != Mode::Managed || !self.nft.installed() {
            return;
        }
        let present = self
            .nft
            .list_table(render::TABLE_FAMILY, render::TABLE_NAME)
            .ok()
            .flatten()
            .is_some();
        if present {
            return;
        }
        match std::fs::read_to_string(self.paths.ruleset()) {
            Ok(script) => match self.nft.apply(&script) {
                Ok(()) => info!("firewall table restored at start-up"),
                Err(err) => warn!(error = %format!("{err:#}"), "cannot restore the firewall table"),
            },
            Err(err) => {
                warn!(error = %err, "the firewall is enabled but its ruleset file is missing")
            }
        }
    }
}

/// Start-up hook: finishes what a restart interrupted, off the async
/// runtime. Only the root daemon has a firewall to recover.
pub fn start(firewall: Arc<Firewall>) {
    if !crate::daemon::config::is_root() {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("firewall-recover".into())
        .spawn(move || firewall.recover());
    if let Err(err) = spawned {
        warn!(error = %err, "cannot start the firewall recovery");
    }
}

impl Default for Firewall {
    fn default() -> Self {
        Self::new()
    }
}

fn check_timeout(seconds: u64) -> Result<()> {
    if seconds != 0 && !(MIN_TIMEOUT..=MAX_TIMEOUT).contains(&seconds) {
        return Err(invalid(format!(
            "the confirmation window must be {MIN_TIMEOUT}-{MAX_TIMEOUT} seconds (or 0 to apply without rollback)"
        )));
    }
    Ok(())
}

fn normalize_entries(entries: Vec<SetEntry>) -> Result<Vec<SetEntry>> {
    if entries.len() > MAX_SET_ENTRIES {
        return Err(invalid(format!(
            "a set holds at most {MAX_SET_ENTRIES} entries"
        )));
    }
    let mut out: Vec<SetEntry> = Vec::with_capacity(entries.len());
    for mut entry in entries {
        entry.normalize().map_err(invalid)?;
        match out.iter_mut().find(|e| e.value == entry.value) {
            Some(existing) => *existing = entry,
            None => out.push(entry),
        }
    }
    Ok(out)
}

/// Who a table belongs to, judging by its name.
pub fn table_owner(family: &str, name: &str) -> &'static str {
    if family == render::TABLE_FAMILY && name == render::TABLE_NAME {
        "asc"
    } else if name.starts_with("f2b") {
        "fail2ban"
    } else if matches!(family, "ip" | "ip6" | "arp" | "bridge")
        && matches!(name, "filter" | "nat" | "mangle" | "raw" | "security")
    {
        "iptables"
    } else {
        "other"
    }
}

/// A short unique id: the prefix and eight hex digits.
fn new_id(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seed = format!(
        "{nanos}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let digest = Sha256::digest(seed.as_bytes());
    let hex: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
    format!("{prefix}-{hex}")
}

/// Names of the presets, for the API.
pub fn preset_names() -> &'static [&'static str] {
    ALL_PRESETS
}

/// The error for a rule id that does not exist.
pub fn rule_not_found(id: &str) -> anyhow::Error {
    not_found(format!("rule '{id}' not found"))
}

#[cfg(test)]
mod tests;
