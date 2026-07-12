//! `asc` — CLI and daemon entry point (`asc serve` runs the daemon itself,
//! everything else is management commands).

use clap::{Parser, Subcommand};

use asc_daemon::daemon::apps::meta::Runtime;
use asc_daemon::daemon::apps::{
    AppManager, AppStats, AppStatus, Outcome, RuntimeState, UserContext,
};
use asc_daemon::daemon::config::Config;
use asc_daemon::daemon::docker;
use asc_daemon::daemon::i18n::{self, Lang, Msg, t, tf, tf2, tf3};
use asc_daemon::daemon::monitor;
use asc_daemon::daemon::pkg::{self, RegistryClient, SourceList};
use asc_daemon::daemon::service::{self, ServiceState};
use asc_daemon::daemon::{logging, server};

#[derive(Parser)]
#[command(
    name = "asc",
    version,
    about = "AdminService.Cloud daemon & CLI — manage apps, backups, monitoring and more"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon in the foreground (used by the systemd service)
    Serve,
    /// Manage the daemon system service (systemd)
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// Show daemon version, service state and apps summary
    Status,
    /// Show CPU and memory usage per app, like `docker stats --no-stream`
    /// (your own apps; run under sudo to see everyone's, grouped by owner)
    Stats {
        /// Sort rows by consumption
        #[arg(long, value_enum, default_value_t = StatsSort::Cpu)]
        sort: StatsSort,
    },
    /// Manage apps (your own; run under sudo to manage everyone's)
    App {
        #[command(subcommand)]
        action: AppAction,
    },
    /// Install an app from a registry: <name> or <name>@<version>
    Install { spec: String },
    /// Attach to an app's console: live output + stdin (Docker apps)
    Attach { id: String },
    /// Upgrade an app to a new version: <name> (latest) or <name>@<version>
    Upgrade { spec: String },
    /// Search packages in the configured registries
    Search { query: String },
    /// Refresh registry indexes (bypass the cache)
    Update,
    /// Manage registry sources: your own list; under sudo — the system list
    /// shared by all users
    Source {
        #[command(subcommand)]
        action: SourceAction,
    },
    /// Manage git authorization for private package repositories
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },
    /// Manage daemon configuration
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Manage auto-updates (proxies to `asc-updater auto ...`)
    Autoupdate {
        /// enable | disable | status
        action: String,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum StatsSort {
    Cpu,
    Mem,
}

#[derive(Subcommand)]
enum SourceAction {
    /// Add a registry source (https:// or file://)
    Add {
        url: String,
        /// Source name (derived from the URL when omitted)
        #[arg(long)]
        name: Option<String>,
    },
    /// Remove a source by name
    Remove { name: String },
    /// List configured sources
    List,
}

#[derive(Subcommand)]
enum AuthAction {
    /// Save credentials for a git host or prefix (e.g. github.com/myorg)
    Add {
        /// Host, host/prefix or a repository URL
        target: String,
        /// Access token for https repositories
        #[arg(long)]
        token: Option<String>,
        /// SSH key for git@/ssh repositories; omit the path to pick
        /// interactively from ~/.ssh
        #[arg(long, num_args = 0..=1)]
        ssh_key: Option<Option<std::path::PathBuf>>,
    },
    /// List configured credentials (methods only, never secrets)
    List,
    /// Remove credentials for a host or prefix
    Remove { target: String },
}

#[derive(Subcommand)]
enum AppAction {
    /// List apps (root sees all users' apps, grouped by owner)
    List,
    /// Show one app's details
    Info {
        id: String,
    },
    /// Install an app from a registry (same as top-level `asc install`)
    Install {
        spec: String,
    },
    /// Attach to the app's console (same as top-level `asc attach`)
    Attach {
        id: String,
    },
    /// Upgrade the app to a new version (same as top-level `asc upgrade`)
    Upgrade {
        spec: String,
    },
    Start {
        id: String,
    },
    Stop {
        id: String,
    },
    Restart {
        id: String,
    },
    /// Show app logs
    Logs {
        id: String,
        /// Number of trailing lines
        #[arg(short = 'n', long, default_value_t = 100)]
        tail: usize,
    },
    /// Interactively edit app settings defined in asc.settings.yaml
    Settings {
        id: String,
    },
    /// Remove an app and all its data
    Remove {
        id: String,
        /// Confirm removal without prompting
        #[arg(short, long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum ServiceAction {
    /// Install the service unit and enable autostart
    Install,
    /// Stop, disable and remove the service unit
    Uninstall,
    Start,
    Stop,
    Restart,
    /// Show service state
    Status,
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Show or set the CLI output language (en|ru)
    Lang { lang: Option<Lang> },
}

fn main() {
    if let Err(err) = run() {
        eprintln!("asc: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let config = Config::load()?;
    i18n::set_lang(config.language);

    // CLI commands die quietly on a closed pipe (`asc status | head`), like
    // any Unix tool. The daemon keeps Rust's default (SIGPIPE ignored):
    // getting killed by a log-pipe hiccup is not acceptable for a service.
    if !matches!(cli.command, Command::Serve) {
        // SAFETY: resetting a signal disposition has no preconditions.
        unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    }

    match cli.command {
        Command::Serve => {
            logging::init(&config.log.level);
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(server::run(config))
        }
        Command::Service { action } => service_cmd(action),
        Command::Status => status_cmd(&config),
        Command::Stats { sort } => stats_cmd(sort, &config),
        Command::App { action } => app_cmd(action, &config),
        Command::Install { spec } => install_cmd(&spec, &config),
        Command::Attach { id } => attach_cmd(&id, &config),
        Command::Upgrade { spec } => upgrade_cmd(&spec, &config),
        Command::Search { query } => search_cmd(&query, &config),
        Command::Update => {
            let updated = RegistryClient::new(&config)?.update()?;
            let total: usize = updated.iter().map(|s| s.packages).sum();
            for s in &updated {
                println!("{}", tf2(Msg::UpdateSourceDone, &s.source_name, s.packages));
            }
            println!("{}", tf2(Msg::UpdateDone, updated.len(), total));
            Ok(())
        }
        Command::Source { action } => source_cmd(action),
        Command::Auth { action } => auth_cmd(action),
        Command::Config { action } => config_cmd(action, config),
        Command::Autoupdate { action } => autoupdate_cmd(&action),
    }
}

/// `asc autoupdate ...` → `asc-updater auto ...` (the updater owns updates;
/// it must work even when the daemon is broken, hence a separate binary).
fn autoupdate_cmd(action: &str) -> anyhow::Result<()> {
    let status = std::process::Command::new("asc-updater")
        .args(["auto", action])
        .status()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => anyhow::anyhow!(t(Msg::UpdNotInstalled)),
            _ => anyhow::Error::new(e).context("cannot run asc-updater"),
        })?;
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

fn install_cmd(spec: &str, config: &Config) -> anyhow::Result<()> {
    let ctx = UserContext::current();
    let report = match pkg::install(config, &ctx, spec) {
        Ok(report) => report,
        // Private repository: offer to set up auth right here, then retry.
        Err(err) if offer_auth_setup(&err) => pkg::install(config, &ctx, spec)?,
        Err(err) => return Err(err),
    };
    println!("{}", tf2(Msg::PkgInstalled, &report.id, &report.version));
    println!("{}", tf(Msg::PkgStartHint, &report.id));
    Ok(())
}

fn auth_cmd(action: AuthAction) -> anyhow::Result<()> {
    use asc_daemon::daemon::pkg::auth::{GitAuth, Method, normalize};
    let mut auth = GitAuth::load()?;
    match action {
        AuthAction::Add {
            target,
            token,
            ssh_key,
        } => {
            let method = match (token, ssh_key) {
                (Some(token), None) => Method::Token { token },
                (None, Some(Some(key))) => Method::SshKey { key },
                (None, Some(None)) => Method::SshKey {
                    key: pick_ssh_key(&target)?,
                },
                _ => anyhow::bail!(t(Msg::AuthNeedMethod)),
            };
            let saved = auth.add(&target, method)?;
            let (pattern, label) = (saved.pattern.clone(), saved.method.label());
            auth.save()?;
            println!("{}", tf2(Msg::AuthSaved, pattern, label));
        }
        AuthAction::List => {
            let all = auth.list();
            if all.is_empty() {
                println!("{}", t(Msg::AuthEmpty));
            } else {
                let name_w = all.iter().map(|(c, _)| c.pattern.len()).max().unwrap_or(4);
                for (cred, scope) in all {
                    println!(
                        "{:<name_w$}  {:<6}  {}",
                        cred.pattern,
                        scope.label(),
                        cred.method.label()
                    );
                }
            }
        }
        AuthAction::Remove { target } => {
            auth.remove(&target)?;
            auth.save()?;
            println!("{}", tf(Msg::AuthRemoved, normalize(&target)));
        }
    }
    Ok(())
}

/// Interactive SSH key selection from `~/.ssh` — the choice is saved by the
/// caller, so next time no prompt is needed.
fn pick_ssh_key(target: &str) -> anyhow::Result<std::path::PathBuf> {
    use anyhow::Context;
    use asc_daemon::daemon::pkg::auth;
    let home = std::env::var_os("HOME").context("cannot determine home directory ($HOME)")?;
    let keys = auth::list_ssh_keys(&std::path::PathBuf::from(home).join(".ssh"));
    if keys.is_empty() {
        anyhow::bail!(t(Msg::AuthNoKeys));
    }
    println!("{}", tf(Msg::AuthPromptKey, target));
    for (i, key) in keys.iter().enumerate() {
        println!("  {}) {}", i + 1, key.display());
    }
    let choice = read_line("> ")?;
    let index: usize = choice
        .parse()
        .ok()
        .filter(|n| (1..=keys.len()).contains(n))
        .ok_or_else(|| anyhow::anyhow!(t(Msg::AuthInvalidChoice)))?;
    Ok(keys[index - 1].clone())
}

fn read_line(prompt: &str) -> anyhow::Result<String> {
    use std::io::Write;
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

/// When `err` says the repository is private and stdin is a terminal, ask
/// for permission to configure authorization on the spot (token for https,
/// SSH key selection for git@/ssh) and save it. `true` = saved, retry.
fn offer_auth_setup(err: &anyhow::Error) -> bool {
    use asc_daemon::daemon::pkg::auth::{self, AuthRequired, GitAuth, Method};
    use asc_daemon::daemon::pkg::sources::Scope;

    let Some(required) = err.downcast_ref::<AuthRequired>() else {
        return false;
    };
    // Non-interactive callers get the structured error with the hint.
    // SAFETY: isatty() has no preconditions.
    if unsafe { libc::isatty(libc::STDIN_FILENO) } != 1 {
        return false;
    }
    let flow = || -> anyhow::Result<bool> {
        let url = &required.url;
        let host = auth::normalize(url)
            .split('/')
            .next()
            .unwrap_or_default()
            .to_string();
        if host.is_empty() {
            return Ok(false);
        }
        let answer = read_line(&tf2(Msg::AuthPromptConfigure, url, &host))?;
        if !matches!(answer.to_lowercase().as_str(), "y" | "yes" | "д" | "да") {
            return Ok(false);
        }
        let method = if auth::is_ssh_url(url) {
            Method::SshKey {
                key: pick_ssh_key(&host)?,
            }
        } else {
            let store_path = match Scope::current() {
                Scope::System => GitAuth::system_path(),
                Scope::User => GitAuth::user_path()?,
            };
            let token = read_line(&tf2(Msg::AuthPromptToken, &host, store_path.display()))?;
            if token.is_empty() {
                return Ok(false);
            }
            Method::Token { token }
        };
        let mut store = GitAuth::load()?;
        let saved = store.add(&host, method)?;
        let confirmation = tf2(Msg::AuthSaved, saved.pattern.clone(), saved.method.label());
        store.save()?;
        println!("{confirmation}");
        println!("{}", t(Msg::AuthRetrying));
        Ok(true)
    };
    match flow() {
        Ok(configured) => configured,
        Err(flow_err) => {
            eprintln!("asc: {flow_err:#}");
            false
        }
    }
}

fn upgrade_cmd(spec: &str, config: &Config) -> anyhow::Result<()> {
    let ctx = UserContext::current();
    let outcome = match pkg::upgrade(config, &ctx, spec) {
        Ok(outcome) => outcome,
        // Private repository: offer to set up auth right here, then retry.
        Err(err) if offer_auth_setup(&err) => pkg::upgrade(config, &ctx, spec)?,
        Err(err) => return Err(err),
    };
    match outcome {
        pkg::UpgradeOutcome::Upgraded { id, from, to } => {
            println!(
                "{}",
                tf3(Msg::PkgUpgraded, &id, from.as_deref().unwrap_or("-"), to)
            );
        }
        pkg::UpgradeOutcome::UpToDate { id, version } => {
            println!("{}", tf2(Msg::PkgUpToDate, &id, version));
        }
    }
    Ok(())
}

fn search_cmd(query: &str, config: &Config) -> anyhow::Result<()> {
    let results = RegistryClient::new(config)?.search(query)?;
    if results.is_empty() {
        println!("{}", tf(Msg::SearchNoResults, query));
        return Ok(());
    }
    let name_w = results
        .iter()
        .map(|p| p.entry.name.len())
        .max()
        .unwrap_or(4)
        .max(4);
    for pkg in results {
        println!(
            "{:<name_w$}  {:<9}  {}",
            pkg.entry.name,
            pkg.entry.latest.as_deref().unwrap_or("-"),
            pkg.entry.description.as_deref().unwrap_or(""),
        );
    }
    Ok(())
}

fn source_cmd(action: SourceAction) -> anyhow::Result<()> {
    let mut sources = SourceList::load()?;
    match action {
        SourceAction::Add { url, name } => {
            let added_name = sources.add(&url, name.as_deref())?.name.clone();
            sources.save()?;
            println!("{}", tf(Msg::SourceAdded, added_name));
        }
        SourceAction::Remove { name } => {
            sources.remove(&name)?;
            sources.save()?;
            println!("{}", tf(Msg::SourceRemoved, name));
        }
        SourceAction::List => {
            let all = sources.list();
            if all.is_empty() {
                println!("{}", t(Msg::SourcesEmpty));
            } else {
                let name_w = all.iter().map(|(s, _)| s.name.len()).max().unwrap_or(4);
                for (source, scope) in all {
                    println!(
                        "{:<name_w$}  {:<6}  {}",
                        source.name,
                        scope.label(),
                        source.url
                    );
                }
            }
        }
    }
    Ok(())
}

fn app_cmd(action: AppAction, config: &Config) -> anyhow::Result<()> {
    let manager = AppManager::new(config);
    let ctx = UserContext::current();
    match action {
        AppAction::List => print_app_list(&manager.list(&ctx)?, ctx.is_root),
        AppAction::Info { id } => {
            let status = manager.status(&ctx, &id)?;
            let m = &status.meta;
            println!("{}  {}", m.id, m.name);
            println!("  kind:    {}", m.runtime.kind());
            println!("  state:   {}", state_label(status.state));
            println!("  version: {}", m.version.as_deref().unwrap_or("-"));
            println!("  source:  {}", m.source.as_deref().unwrap_or("-"));
            if let Some(quota) = &m.quota {
                println!("  quota:   {}", quota_label(quota));
            }
            println!("  {}", tf(Msg::OwnerLabel, &m.owner.name));
        }
        AppAction::Install { spec } => install_cmd(&spec, config)?,
        AppAction::Attach { id } => attach_cmd(&id, config)?,
        AppAction::Upgrade { spec } => upgrade_cmd(&spec, config)?,
        AppAction::Start { id } => match manager.start(&ctx, &id)? {
            Outcome::Done => println!("{}", tf(Msg::AppStarted, &id)),
            Outcome::AlreadyInState => println!("{}", tf(Msg::AppAlreadyRunning, &id)),
        },
        AppAction::Stop { id } => match manager.stop(&ctx, &id)? {
            Outcome::Done => println!("{}", tf(Msg::AppStopped, &id)),
            Outcome::AlreadyInState => println!("{}", tf(Msg::AppNotRunning, &id)),
        },
        AppAction::Restart { id } => {
            manager.restart(&ctx, &id)?;
            println!("{}", tf(Msg::AppRestarted, &id));
        }
        AppAction::Logs { id, tail } => {
            let logs = manager.logs(&ctx, &id, tail)?;
            if logs.trim().is_empty() {
                println!("{}", t(Msg::NoLogs));
            } else {
                println!("{logs}");
            }
        }
        AppAction::Settings { id } => app_settings_cmd(&id, config)?,
        AppAction::Remove { id, yes } => {
            if !yes {
                anyhow::bail!(tf(Msg::AppRemoveNeedsYes, &id));
            }
            manager.remove(&ctx, &id)?;
            println!("{}", tf(Msg::AppRemoved, &id));
        }
    }
    Ok(())
}

/// "cpu ≤ 2, ram ≤ 512.0 MiB, disk ≤ 10.0 GiB" — set quota limits only.
fn quota_label(quota: &asc_daemon::daemon::apps::meta::Quota) -> String {
    let mut parts = Vec::new();
    if let Some(cores) = quota.cpu_cores {
        parts.push(format!("cpu ≤ {cores}"));
    }
    if let Some(ram) = quota.ram_bytes {
        parts.push(format!("ram ≤ {}", monitor::human_bytes(ram)));
    }
    if let Some(disk) = quota.disk_bytes {
        parts.push(format!("disk ≤ {}", monitor::human_bytes(disk)));
    }
    parts.join(", ")
}

/// `asc app settings <id>` — interactive settings editor (DMN-017): pick a
/// setting by number, enter a value validated against asc.settings.yaml.
/// Values are stored in `config/settings.json`; the runtime picks them up on
/// the next restart.
fn app_settings_cmd(id: &str, config: &Config) -> anyhow::Result<()> {
    use asc_daemon::daemon::pkg::manifest::Manifest;
    use asc_daemon::daemon::pkg::settings::{
        SettingKind, SettingValues, SettingsFile, manifest_dir_of,
    };

    let manager = AppManager::new(config);
    let ctx = UserContext::current();
    manager.get_authorized(&ctx, id)?;
    let app_dir = manager.store().app_dir(id)?;

    let manifest_dir = manifest_dir_of(config, id, &app_dir)?;
    let manifest = Manifest::load(&manifest_dir)?;
    let defs = match SettingsFile::load_for(&manifest_dir, &manifest)? {
        Some(file) if !file.settings.is_empty() => file.settings,
        _ => {
            println!("{}", tf(Msg::SettingsNone, id));
            return Ok(());
        }
    };

    let config_dir = app_dir.join("config");
    let mut values = SettingValues::load(&config_dir)?;
    values.merge_defaults(&defs);
    let key_w = defs.iter().map(|d| d.key.len()).max().unwrap_or(4);

    let mut changed = false;
    loop {
        println!();
        println!("{}", tf(Msg::SettingsHeader, id));
        for (i, def) in defs.iter().enumerate() {
            let hint = def
                .constraint_hint()
                .map(|h| format!("  ({h})"))
                .unwrap_or_default();
            let title = def
                .title
                .as_deref()
                .map(|t| format!("  — {t}"))
                .unwrap_or_default();
            println!(
                "  {:>2}) {:<key_w$} = {}{hint}{title}",
                i + 1,
                def.key,
                values.display(def),
            );
        }
        println!();
        let line = read_line(t(Msg::SettingsPromptSelect))?;
        if line.is_empty() || line.eq_ignore_ascii_case("q") {
            break;
        }
        let Some(def) = line
            .parse::<usize>()
            .ok()
            .and_then(|n| n.checked_sub(1))
            .and_then(|i| defs.get(i))
        else {
            eprintln!("asc: {}", t(Msg::AuthInvalidChoice));
            continue;
        };

        // Enums are picked from a numbered list; everything else is typed in.
        if def.kind == SettingKind::Enum {
            for (i, value) in def.values.iter().enumerate() {
                println!("  {}) {}", i + 1, def.display_of(&serde_json::json!(value)));
            }
        }
        let hint = def
            .constraint_hint()
            .map(|h| format!(" ({h})"))
            .unwrap_or_default();
        let mut raw = read_line(&tf2(Msg::SettingsPromptValue, &def.key, hint))?;
        if raw.is_empty() {
            continue;
        }
        // An enum answer may be the option number instead of the value.
        if def.kind == SettingKind::Enum
            && let Ok(n) = raw.parse::<usize>()
            && (1..=def.values.len()).contains(&n)
            && let Ok(picked) = serde_json::to_value(&def.values[n - 1])
        {
            raw = match &picked {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
        }
        match def.parse_value(&raw) {
            Ok(value) => {
                let shown = def.display_of(&value);
                values.set(&def.key, value);
                values.save(&config_dir)?;
                changed = true;
                println!("{}", tf2(Msg::SettingsSaved, &def.key, shown));
            }
            Err(err) => eprintln!("asc: {err:#}"),
        }
    }
    if changed {
        println!("{}", tf(Msg::SettingsRestartHint, id));
    }
    Ok(())
}

/// `asc attach` — interactive app console in the terminal: the app's output
/// goes to stdout, the terminal's stdin goes to the app. Talks straight to
/// the Engine API (standalone, no running daemon needed); Docker fans the
/// output out to every attached client, so the CLI and browser tabs coexist.
fn attach_cmd(id: &str, config: &Config) -> anyhow::Result<()> {
    let manager = AppManager::new(config);
    let ctx = UserContext::current();
    let status = manager.status(&ctx, id)?;
    let Runtime::Docker { container } = &status.meta.runtime else {
        anyhow::bail!(tf2(Msg::AttachDockerOnly, id, status.meta.runtime.kind()));
    };
    if status.state != RuntimeState::Running {
        anyhow::bail!(tf2(Msg::AttachStartFirst, id, id));
    }
    // The hint goes to stderr so piped stdout stays pure app output.
    eprintln!("{}", tf(Msg::AttachHint, id));

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(attach_loop(&config.docker, container));
    // tokio's stdin reads on the blocking pool; a plain drop of the runtime
    // would wait for the pending read (i.e. for the user to press Enter).
    runtime.shutdown_background();
    result
}

async fn attach_loop(
    cfg: &asc_daemon::daemon::config::DockerConfig,
    container: &str,
) -> anyhow::Result<()> {
    use futures_util::StreamExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut attach = docker::attach(cfg, container).await?;
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut buf = [0u8; 4096];
    loop {
        tokio::select! {
            output = attach.output.next() => match output {
                Some(Ok(chunk)) => {
                    stdout.write_all(&chunk.into_bytes()).await?;
                    stdout.flush().await?;
                }
                Some(Err(err)) => anyhow::bail!("docker attach: {err}"),
                None => break, // app stopped
            },
            read = stdin.read(&mut buf) => {
                let n = read?;
                if n == 0 {
                    break; // stdin closed (Ctrl+D / piped input finished)
                }
                attach.input.write_all(&buf[..n]).await?;
            }
        }
    }
    Ok(())
}

/// `asc stats` — one-shot resource usage per app, like `docker stats
/// --no-stream`. Root gets everyone's apps grouped by owner.
fn stats_cmd(sort: StatsSort, config: &Config) -> anyhow::Result<()> {
    let manager = AppManager::new(config);
    let ctx = UserContext::current();
    let mut stats = manager.stats(&ctx)?;
    if stats.is_empty() {
        println!("{}", t(Msg::AppListEmpty));
        return Ok(());
    }
    // Highest consumers first; apps without data (stopped) go last.
    stats.sort_by(|a, b| {
        let key = |s: &AppStats| match sort {
            StatsSort::Cpu => s.cpu_percent.unwrap_or(-1.0),
            StatsSort::Mem => s.memory_bytes.map(|m| m as f64).unwrap_or(-1.0),
        };
        key(b).total_cmp(&key(a))
    });

    let id_w = stats
        .iter()
        .map(|s| s.meta.id.len())
        .max()
        .unwrap_or(2)
        .max(2);
    let print_rows = |rows: &[&AppStats]| {
        println!(
            "{:<id_w$}  {:<7}  {:>7}  {:>10}",
            "ID", "KIND", "CPU %", "MEM"
        );
        for s in rows {
            let cpu = s
                .cpu_percent
                .map(|c| format!("{c:.1}%"))
                .unwrap_or_else(|| "-".into());
            let mem = s
                .memory_bytes
                .map(monitor::human_bytes)
                .unwrap_or_else(|| "-".into());
            println!(
                "{:<id_w$}  {:<7}  {:>7}  {:>10}",
                s.meta.id,
                s.meta.runtime.kind(),
                cpu,
                mem,
            );
        }
    };
    if ctx.is_root {
        let mut owners: Vec<&str> = stats.iter().map(|s| s.meta.owner.name.as_str()).collect();
        owners.sort_unstable();
        owners.dedup();
        for (i, owner) in owners.iter().enumerate() {
            if i > 0 {
                println!();
            }
            println!("{}", tf(Msg::OwnerLabel, owner));
            let rows: Vec<&AppStats> = stats
                .iter()
                .filter(|s| s.meta.owner.name == *owner)
                .collect();
            print_rows(&rows);
        }
    } else {
        let rows: Vec<&AppStats> = stats.iter().collect();
        print_rows(&rows);
    }
    Ok(())
}

fn state_label(state: RuntimeState) -> &'static str {
    match state {
        RuntimeState::Running => t(Msg::StateActive),
        RuntimeState::Stopped => t(Msg::StateInactive),
    }
}

/// Table of apps; root gets them grouped by owner. Column headers are
/// technical identifiers and stay English by convention (like `docker ps`).
fn print_app_list(apps: &[AppStatus], group_by_owner: bool) {
    if apps.is_empty() {
        println!("{}", t(Msg::AppListEmpty));
        return;
    }
    let id_w = apps
        .iter()
        .map(|a| a.meta.id.len())
        .max()
        .unwrap_or(2)
        .max(2);
    let print_rows = |rows: &[&AppStatus]| {
        println!("{:<id_w$}  {:<7}  {:<10}  VERSION", "ID", "KIND", "STATE");
        for app in rows {
            println!(
                "{:<id_w$}  {:<7}  {:<10}  {}",
                app.meta.id,
                app.meta.runtime.kind(),
                state_label(app.state),
                app.meta.version.as_deref().unwrap_or("-"),
            );
        }
    };
    if group_by_owner {
        let mut owners: Vec<&str> = apps.iter().map(|a| a.meta.owner.name.as_str()).collect();
        owners.sort_unstable();
        owners.dedup();
        for (i, owner) in owners.iter().enumerate() {
            if i > 0 {
                println!();
            }
            println!("{}", tf(Msg::OwnerLabel, owner));
            let rows: Vec<&AppStatus> = apps
                .iter()
                .filter(|a| a.meta.owner.name == *owner)
                .collect();
            print_rows(&rows);
        }
    } else {
        let rows: Vec<&AppStatus> = apps.iter().collect();
        print_rows(&rows);
    }
}

fn service_cmd(action: ServiceAction) -> anyhow::Result<()> {
    if !matches!(action, ServiceAction::Status) {
        service::require_root()?;
    }
    let manager = service::detect()?;
    match action {
        ServiceAction::Install => {
            manager.install()?;
            println!("{}", t(Msg::ServiceInstalled));
        }
        ServiceAction::Uninstall => {
            manager.uninstall()?;
            println!("{}", t(Msg::ServiceUninstalled));
        }
        ServiceAction::Start => {
            manager.start()?;
            println!("{}", t(Msg::ServiceStarted));
        }
        ServiceAction::Stop => {
            manager.stop()?;
            println!("{}", t(Msg::ServiceStopped));
        }
        ServiceAction::Restart => {
            manager.restart()?;
            println!("{}", t(Msg::ServiceRestarted));
        }
        ServiceAction::Status => print_service_state()?,
    }
    Ok(())
}

fn status_cmd(config: &Config) -> anyhow::Result<()> {
    // The banner is for humans: piped output (grep, scripts) stays clean.
    // SAFETY: isatty() has no preconditions.
    if unsafe { libc::isatty(libc::STDOUT_FILENO) } == 1 {
        println!("{}", asc_daemon::BANNER);
        println!();
    }
    println!("asc {}", asc_daemon::VERSION);
    print_service_state()?;
    let manager = AppManager::new(config);
    let apps = manager.list(&UserContext::current())?;
    let running = apps
        .iter()
        .filter(|a| a.state == RuntimeState::Running)
        .count();
    println!("{}", tf2(Msg::StatusApps, running, apps.len()));
    print_system_metrics();
    Ok(())
}

/// System metrics block of `asc status`. Sampled by the CLI itself (no
/// running daemon required); skipped silently if /proc is unreadable.
fn print_system_metrics() {
    let Ok(metrics) = monitor::system::snapshot_blocking() else {
        return;
    };
    let usage = metrics
        .cpu
        .usage_percent
        .map(|u| format!("{u:.0}%"))
        .unwrap_or_else(|| "-".into());
    let load = format!(
        "{:.2} {:.2} {:.2}",
        metrics.cpu.load1, metrics.cpu.load5, metrics.cpu.load15
    );
    println!("{}", tf3(Msg::StatusCpu, usage, metrics.cpu.cores, load));
    println!(
        "{}",
        tf(
            Msg::StatusMemory,
            usage_string(metrics.memory.used, metrics.memory.total)
        )
    );
    for disk in &metrics.disks {
        println!(
            "{}",
            tf2(
                Msg::StatusDisk,
                &disk.mount,
                usage_string(disk.used, disk.total)
            )
        );
    }
}

/// "1.2 GiB / 15.6 GiB (7%)" — language-neutral usage figure.
fn usage_string(used: u64, total: u64) -> String {
    let percent = (used * 100).checked_div(total).unwrap_or(0);
    format!(
        "{} / {} ({percent}%)",
        monitor::human_bytes(used),
        monitor::human_bytes(total)
    )
}

fn print_service_state() -> anyhow::Result<()> {
    let state = match service::detect() {
        Ok(manager) => match manager.state()? {
            ServiceState::Active => t(Msg::StateActive).to_string(),
            ServiceState::Inactive => t(Msg::StateInactive).to_string(),
            ServiceState::NotInstalled => t(Msg::StateNotInstalled).to_string(),
        },
        // Unsupported host (no systemd / not Linux): report why instead of failing.
        Err(err) => err.to_string(),
    };
    println!("{}", tf(Msg::StatusService, state));
    Ok(())
}

fn config_cmd(action: ConfigAction, mut config: Config) -> anyhow::Result<()> {
    match action {
        ConfigAction::Lang { lang: None } => {
            println!("{}", tf(Msg::ConfigLangCurrent, config.language));
        }
        ConfigAction::Lang { lang: Some(lang) } => {
            config.language = lang;
            config.save()?;
            i18n::set_lang(lang);
            println!("{}", tf(Msg::ConfigLangSet, lang));
        }
    }
    Ok(())
}
