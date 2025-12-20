//! `asc` — CLI and daemon entry point (`asc serve` runs the daemon itself,
//! everything else is management commands).

use clap::{Parser, Subcommand};

use asc_daemon::daemon::apps::{
    AppManager, AppStats, AppStatus, Outcome, RuntimeState, UserContext,
};
use asc_daemon::daemon::config::Config;
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
        Command::Search { query } => search_cmd(&query, &config),
        Command::Update => {
            RegistryClient::new(&config)?.update()?;
            println!("{}", t(Msg::UpdateDone));
            Ok(())
        }
        Command::Source { action } => source_cmd(action),
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
    let report = pkg::install(config, &ctx, spec)?;
    println!("{}", tf2(Msg::PkgInstalled, &report.id, &report.version));
    println!("{}", tf(Msg::PkgStartHint, &report.id));
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
            println!("  {}", tf(Msg::OwnerLabel, &m.owner.name));
        }
        AppAction::Install { spec } => install_cmd(&spec, config)?,
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
