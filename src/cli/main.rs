//! `asc` — CLI and daemon entry point (`asc serve` runs the daemon itself,
//! everything else is management commands).

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};

use asc_daemon::daemon::apps::meta::Runtime;
use asc_daemon::daemon::apps::{
    AppManager, AppStats, AppStatus, ImageSource, Outcome, RuntimeState, UserContext,
};
use asc_daemon::daemon::client::{self, RemoteApp};
use asc_daemon::daemon::config::Config;
use asc_daemon::daemon::docker;
use asc_daemon::daemon::i18n::{self, Lang, Msg, t, tf, tf2, tf3};
use asc_daemon::daemon::monitor;
use asc_daemon::daemon::pkg::{self, RegistryClient, SourceList};
use asc_daemon::daemon::progress;
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
    command: Option<Command>,
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
    /// Show CPU, memory and disk usage per app, like `docker stats
    /// --no-stream` (your own apps; run under sudo to see everyone's,
    /// grouped by owner)
    Stats {
        /// Sort rows by consumption
        #[arg(long, value_enum, default_value_t = StatsSort::Cpu)]
        sort: StatsSort,
        /// Keep refreshing in place until interrupted (Ctrl+C), like plain
        /// `docker stats`
        #[arg(long)]
        live: bool,
    },
    /// Manage apps (your own; run under sudo to manage everyone's)
    App {
        #[command(subcommand)]
        action: AppAction,
    },
    /// List apps, shorthand for `asc app list` (root sees all users' apps).
    /// `asc ls ports|disk|stats` switch to the ports, disk-usage or live
    /// stats views of the same apps
    #[command(visible_alias = "ps")]
    Ls {
        #[command(subcommand)]
        action: Option<LsAction>,
    },
    /// Show published ports per app, shorthand for `asc app ports` (root sees
    /// all users' apps): each app and the host==container ports it publishes
    Ports,
    /// Show disk usage, shorthand for `asc app disk` (root sees all users'
    /// apps): total space occupied by apps as a bar against the store's
    /// filesystem capacity, then each app's own usage, largest first
    Disk,
    /// List installed stacks (`asc.stack.yaml` packages) and their member
    /// apps, hierarchically (root sees all users' apps); apps installed on
    /// their own, outside any stack, are not shown here — see `asc ls`
    Stacks,
    /// Install from a registry (<name>, <stack> or <stack>/<app>, with
    /// optional @<version>) or directly from a git repository URL
    /// (https://, ssh:// or git@host:path)
    Install {
        spec: String,
        /// Registry source to install from (when several provide the
        /// package; not used for a direct repository install)
        #[arg(long)]
        source: Option<String>,
        /// Custom app name (skips the interactive prompt); commands accept
        /// it interchangeably with the app id
        #[arg(long)]
        name: Option<String>,
        /// Branch to check out (direct repository installs only)
        #[arg(long, conflicts_with = "tag")]
        branch: Option<String>,
        /// Tag to check out (direct repository installs only)
        #[arg(long, conflicts_with = "branch")]
        tag: Option<String>,
        /// Pull the prebuilt image when the manifest offers both `image` and
        /// `image-build` (DMN-050); skips the interactive choice
        #[arg(long, conflicts_with = "build")]
        image: bool,
        /// Build the image locally when the manifest offers both `image` and
        /// `image-build` (DMN-050); skips the interactive choice
        #[arg(long, conflicts_with = "image")]
        build: bool,
    },
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
    /// Create, restore and manage app backups (DMN-009)
    Backup {
        #[command(subcommand)]
        action: BackupAction,
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

/// Views of the app list reachable as `asc ls <view>` — the same apps seen
/// through ports, disk usage or live stats. Each mirrors a top-level command
/// (`asc ports` / `asc disk` / `asc stats`) so both spellings stay in step.
#[derive(Subcommand)]
enum LsAction {
    /// Published ports per app (same as `asc ports`)
    Ports,
    /// Disk usage per app (same as `asc disk`)
    Disk,
    /// CPU, memory and disk stats per app (same as `asc stats`)
    Stats,
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
    /// Save credentials for a git host or an image registry
    /// (e.g. github.com/myorg, ghcr.io/myorg)
    Add {
        /// Host, host/prefix, repository URL or image reference
        target: String,
        /// What the credential authorizes against
        #[arg(long = "type", value_name = "repo|registry", default_value = "repo")]
        kind: String,
        /// Access token (https repositories and registries)
        #[arg(long)]
        token: Option<String>,
        /// SSH key for git@/ssh repositories; omit the path to pick
        /// interactively from ~/.ssh
        #[arg(long, num_args = 0..=1)]
        ssh_key: Option<Option<std::path::PathBuf>>,
        /// User name — required by image registries alongside the token
        #[arg(long)]
        username: Option<String>,
        /// Use this credential only for one app (its uuid or id from `asc ls`)
        #[arg(long)]
        app: Option<String>,
    },
    /// List configured credentials (types and methods only, never secrets)
    List,
    /// Remove credentials for a host or prefix
    Remove {
        target: String,
        /// Remove only this type; by default every type with that pattern goes
        #[arg(long = "type", value_name = "repo|registry")]
        kind: Option<String>,
    },
}

#[derive(Subcommand)]
enum BackupAction {
    /// Back up an app: repository, config and data, minus asc.backup.yaml
    /// exclusions
    Create {
        app: String,
        /// Storage to back up to (repeatable); defaults to the app's
        /// backup policy (`asc app settings`), else just 'local'
        #[arg(long = "storage")]
        storages: Vec<String>,
    },
    /// Restore an app from a backup — the app must be stopped first
    /// (destructive: replaces the app's repository, config and data)
    Restore {
        app: String,
        /// Backup name, as shown by `asc backup list`
        backup: String,
        /// Storage the backup lives on (default: local)
        #[arg(long)]
        storage: Option<String>,
    },
    /// List an app's backups on one storage, oldest first
    List {
        app: String,
        /// Storage to list (default: local)
        #[arg(long)]
        storage: Option<String>,
    },
    /// Delete an app's oldest backups on one storage beyond --keep
    Prune {
        app: String,
        /// Storage to prune (default: local)
        #[arg(long)]
        storage: Option<String>,
        #[arg(long)]
        keep: u32,
    },
    /// Manage backup storages: 'local' always exists; add S3/FTP/SFTP or
    /// another local directory
    Storage {
        #[command(subcommand)]
        action: StorageAction,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum StorageType {
    Local,
    S3,
    Ftp,
    Sftp,
}

/// Fields of `asc backup storage add`, boxed in [`StorageAction::Add`] —
/// this one variant carries every provider's fields at once (clippy's
/// `large_enum_variant`), so it is the odd one out size-wise next to
/// `List`/`Remove`.
#[derive(clap::Args)]
struct StorageAddArgs {
    name: String,
    #[arg(long, value_enum)]
    r#type: StorageType,
    #[arg(long)]
    dir: Option<String>,
    #[arg(long)]
    bucket: Option<String>,
    #[arg(long)]
    region: Option<String>,
    #[arg(long)]
    endpoint: Option<String>,
    #[arg(long)]
    access_key: Option<String>,
    #[arg(long)]
    secret_key: Option<String>,
    #[arg(long)]
    prefix: Option<String>,
    #[arg(long)]
    host: Option<String>,
    #[arg(long)]
    port: Option<u16>,
    #[arg(long)]
    user: Option<String>,
    #[arg(long)]
    password: Option<String>,
    /// SFTP private key path (SFTP only, instead of --password)
    #[arg(long)]
    key: Option<std::path::PathBuf>,
}

#[derive(Subcommand)]
enum StorageAction {
    /// Add a storage; the required flags depend on --type (local: --dir;
    /// s3: --bucket --region [--endpoint] --access-key --secret-key
    /// [--prefix]; ftp/sftp: --host [--port] --user [--password] [--dir]
    /// [--key] (sftp only, instead of --password))
    Add(Box<StorageAddArgs>),
    /// List configured storages ('local' is always there, unlisted)
    List,
    /// Remove a configured storage ('local' cannot be removed)
    Remove { name: String },
}

#[derive(Subcommand)]
enum AppAction {
    /// List apps (root sees all users' apps, grouped by owner)
    List,
    /// Show one app's details
    Info {
        id: String,
    },
    /// Show disk usage: quota bar (if a quota is set) and a breakdown by
    /// image, repository, data and custom volumes. Without an id: total
    /// space occupied by all apps, then each app's usage, largest first.
    Disk {
        id: Option<String>,
    },
    /// Show published ports (host==container) with their transport. Without
    /// an id: every app and its ports as a table.
    Ports {
        id: Option<String>,
    },
    /// Clone an app instance (data, env, settings) into a new one (DMN-019)
    Clone {
        id: String,
        /// Custom name for the clone (skips the interactive prompt)
        #[arg(long)]
        name: Option<String>,
    },
    /// Install an app from a registry or a git repository URL (same as
    /// top-level `asc install`)
    Install {
        spec: String,
        /// Registry source to install from (when several provide the
        /// package; not used for a direct repository install)
        #[arg(long)]
        source: Option<String>,
        /// Custom app name (skips the interactive prompt); commands accept
        /// it interchangeably with the app id
        #[arg(long)]
        name: Option<String>,
        /// Branch to check out (direct repository installs only)
        #[arg(long, conflicts_with = "tag")]
        branch: Option<String>,
        /// Tag to check out (direct repository installs only)
        #[arg(long, conflicts_with = "branch")]
        tag: Option<String>,
        /// Pull the prebuilt image when the manifest offers both (DMN-050)
        #[arg(long, conflicts_with = "build")]
        image: bool,
        /// Build the image locally when the manifest offers both (DMN-050)
        #[arg(long, conflicts_with = "image")]
        build: bool,
    },
    /// Attach to the app's console (same as top-level `asc attach`)
    Attach {
        id: String,
    },
    /// Upgrade the app to a new version (same as top-level `asc upgrade`)
    Upgrade {
        spec: String,
    },
    /// Start the app and attach to its console (Docker apps, interactive
    /// terminal); use -d to start in the background
    Start {
        id: String,
        /// Detached mode: start the app without attaching to its console
        #[arg(short = 'd', long)]
        detach: bool,
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
    /// Show or set debug logging (on|off); persists to config.toml [log] level
    Debug { state: Option<OnOff> },
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum OnOff {
    On,
    Off,
}

impl std::fmt::Display for OnOff {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            OnOff::On => "on",
            OnOff::Off => "off",
        })
    }
}

fn main() {
    if let Err(err) = run() {
        eprintln!("asc: {err:#}");
        std::process::exit(1);
    }
}

/// Base URL of the public docs site; update here if it ever moves (e.g. a
/// custom domain) — every `--help` footer link is derived from it.
const DOCS_BASE_URL: &str = "https://adminservicecloud.github.io/asc-documentaion";

/// Docs-site URL for a command tree node, given its path from the root
/// (`path[0]` is always `"asc"`). Top-level commands get their own page
/// (`commands/app`); anything deeper is an anchor on its top-level command's
/// page (`commands/backup#storage-add`) — see `docs/commands/` in
/// asc-documentaion for the page/heading convention this must match.
fn docs_url(path: &[String]) -> String {
    let tail = match path.len() {
        0 | 1 => "commands/asc".to_string(),
        2 => format!("commands/{}", path[1]),
        _ => format!("commands/{}#{}", path[1], path[2..].join("-")),
    };
    match i18n::lang() {
        Lang::Ru => format!("{DOCS_BASE_URL}/ru/{tail}"),
        Lang::En => format!("{DOCS_BASE_URL}/{tail}"),
    }
}

/// Recursively stamps every node of the command tree with an `after_help`
/// footer pointing at its docs page (see `docs_url`). Must run after
/// `i18n::set_lang` (the footer text/URL is language-dependent) and before
/// `.get_matches()` (clap prints help — and exits — from inside that call).
fn inject_help_links(cmd: &mut clap::Command, path: &[String]) {
    let footer = format!("{}: {}", t(Msg::MoreInfo), docs_url(path));
    let taken = std::mem::replace(cmd, clap::Command::new(""));
    *cmd = taken.after_help(footer);

    let child_names: Vec<String> = cmd
        .get_subcommands()
        .map(|s| s.get_name().to_string())
        .collect();
    for (sub, name) in cmd.get_subcommands_mut().zip(child_names) {
        let mut child_path = path.to_vec();
        child_path.push(name);
        inject_help_links(sub, &child_path);
    }
}

/// Bare `asc` (no subcommand): the ASCII banner, copyright and a pointer to
/// `--help` — a welcome screen instead of a bare usage error.
fn banner_cmd() -> anyhow::Result<()> {
    println!("{}", asc_daemon::BANNER);
    println!();
    println!(
        "asc {} — AdminService.Cloud daemon & CLI",
        asc_daemon::VERSION
    );
    println!("Copyright (c) 2020 - 2026 Omar El Sayed");
    println!("https://github.com/AdminServiceCloud/asc-daemon");
    println!();
    println!("{}", t(Msg::BannerHelpHint));
    Ok(())
}

fn run() -> anyhow::Result<()> {
    // Loaded once, early, best-effort: a malformed config.toml must never
    // block `--help`/`-h` (clap prints them — and exits — from inside
    // get_matches() below, before we'd get a chance to surface a config
    // error). The same Result is consumed for real once a command is chosen.
    let config_result = Config::load();
    i18n::set_lang(
        config_result
            .as_ref()
            .map(|c| c.language)
            .unwrap_or_default(),
    );

    let mut cmd = Cli::command();
    inject_help_links(&mut cmd, &["asc".to_string()]);
    let matches = cmd.get_matches();
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());

    // Bare `asc` (no subcommand): banner + copyright + repository link,
    // like `git` or `npm` printing a welcome screen instead of an error.
    let Some(command) = cli.command else {
        return banner_cmd();
    };
    let config = config_result?;
    // Every command gets tracing, not just `serve`: `asc install` and
    // friends run package/docker logic in-process, so `asc config debug on`
    // is only useful if the CLI itself emits the debug! calls it enables.
    logging::init(&config.log.level);

    // CLI commands die quietly on a closed pipe (`asc status | head`), like
    // any Unix tool. The daemon keeps Rust's default (SIGPIPE ignored):
    // getting killed by a log-pipe hiccup is not acceptable for a service.
    if !matches!(command, Command::Serve) {
        // SAFETY: resetting a signal disposition has no preconditions.
        unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    }

    match command {
        Command::Serve => tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(server::run(config)),
        Command::Service { action } => service_cmd(action),
        Command::Status => status_cmd(&config),
        Command::Stats { sort, live } => stats_cmd(sort, live, &config),
        Command::App { action } => app_cmd(action, &config),
        Command::Ls { action } => match action {
            None => app_cmd(AppAction::List, &config),
            Some(LsAction::Ports) => app_cmd(AppAction::Ports { id: None }, &config),
            Some(LsAction::Disk) => app_cmd(AppAction::Disk { id: None }, &config),
            Some(LsAction::Stats) => stats_cmd(StatsSort::Cpu, false, &config),
        },
        Command::Ports => app_cmd(AppAction::Ports { id: None }, &config),
        Command::Disk => app_cmd(AppAction::Disk { id: None }, &config),
        Command::Stacks => stacks_cmd(&config),
        Command::Install {
            spec,
            source,
            name,
            branch,
            tag,
            image,
            build,
        } => install_cmd(
            &spec,
            source.as_deref(),
            name,
            branch.as_deref(),
            tag.as_deref(),
            image_choice_flag(image, build),
            &config,
        ),
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
        Command::Backup { action } => backup_cmd(action, &config),
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

/// The daemon connection for app commands, when the local API socket is
/// present (DMN-042): the daemon then owns identity (SO_PEERCRED) and
/// authorization, and the CLI needs neither docker.sock access nor sudo.
/// `None` — no daemon on this host, the command runs in-process (DMN-041).
/// A present-but-unresponsive daemon is an error for a regular user (they
/// have no other way to the system apps) and a warned fallback for root
/// (recovery must not depend on the daemon being healthy).
fn daemon_backend(config: &Config) -> anyhow::Result<Option<client::Daemon>> {
    match client::Daemon::connect(config) {
        Ok(daemon) => Ok(daemon),
        Err(err) => {
            if UserContext::current().is_root {
                eprintln!("{}", t(Msg::DaemonDirectFallback));
                Ok(None)
            } else {
                Err(err)
            }
        }
    }
}

/// Run `op` under a "processing..." spinner (tty-gated): start/stop wait on
/// Docker or the daemon with no output of their own, and a silent terminal
/// reads as a hung command.
fn with_spinner<T>(op: impl FnOnce() -> T) -> T {
    let spinner = progress::interactive().then(|| progress::Spinner::new(t(Msg::Processing)));
    let result = op();
    if let Some(spinner) = spinner {
        spinner.finish();
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn install_cmd(
    spec: &str,
    source: Option<&str>,
    name: Option<String>,
    branch: Option<&str>,
    tag: Option<&str>,
    image_choice: Option<ImageSource>,
    config: &Config,
) -> anyhow::Result<()> {
    let daemon = daemon_backend(config)?;
    if pkg::is_git_url(spec) {
        return install_from_git_cmd(
            spec,
            source,
            name,
            branch,
            tag,
            image_choice,
            config,
            daemon,
        );
    }
    if branch.is_some() || tag.is_some() {
        anyhow::bail!(
            "--branch and --tag are only used for a direct repository install (a git URL as the spec); pin a registry version with @<version> instead"
        );
    }
    let name = match name {
        Some(name) => Some(name),
        None => prompt_app_name(spec, config, daemon.as_ref())?,
    };
    let name = name.as_deref();
    let outcome = match &daemon {
        Some(daemon) => install_daemon_loop(daemon, spec, source, name, None, None, image_choice)?,
        None => {
            let ctx = UserContext::current();
            let mut spec = spec.to_string();
            let mut source = source.map(str::to_string);
            let mut license_ack = false;
            let mut image_choice = image_choice;
            // Interactive recoveries loop until the install passes or the user
            // declines: auth setup for private repositories, a source pick when
            // several provide the package, a version pick for `pkg@` (DMN-048),
            // an image-source pick when both are offered (DMN-050), license
            // consent (DMN-028).
            loop {
                match pkg::install(
                    config,
                    &ctx,
                    &spec,
                    source.as_deref(),
                    name,
                    license_ack,
                    image_choice,
                ) {
                    Ok(outcome) => break outcome,
                    Err(err) if offer_auth_setup(&err) => continue,
                    Err(err) => {
                        if let Some(chosen) = pick_version(&err)? {
                            spec = chosen.spec;
                            source = chosen.source;
                            continue;
                        }
                        if let Some(chosen) = pick_source(&err)? {
                            source = Some(chosen);
                            continue;
                        }
                        if let Some(chosen) = pick_image(&err)? {
                            image_choice = Some(chosen);
                            continue;
                        }
                        if accept_license(&err)? {
                            license_ack = true;
                            continue;
                        }
                        return Err(err);
                    }
                }
            }
        }
    };
    print_install_outcome(&outcome);
    Ok(())
}

/// Map the mutually exclusive `--image` / `--build` flags (clap rejects both
/// at once) to an image source; neither set leaves the choice open — the
/// installer prompts, or errors non-interactively (DMN-050).
fn image_choice_flag(image: bool, build: bool) -> Option<ImageSource> {
    match (image, build) {
        (true, _) => Some(ImageSource::Prebuilt),
        (_, true) => Some(ImageSource::Build),
        _ => None,
    }
}

/// Install through the daemon, with the same interactive recoveries as the
/// in-process path — the client reconstructs the typed errors from the
/// structured REST payloads, so `pick_source`/`accept_license` work
/// unchanged. Auth setup for private repositories is not offered here: the
/// daemon clones with its own credentials, per-user git auth over the
/// daemon is DMN-043.
#[allow(clippy::too_many_arguments)]
fn install_daemon_loop(
    daemon: &client::Daemon,
    spec: &str,
    source: Option<&str>,
    name: Option<&str>,
    branch: Option<&str>,
    tag: Option<&str>,
    image_choice: Option<ImageSource>,
) -> anyhow::Result<pkg::InstallOutcome> {
    let mut spec = spec.to_string();
    let mut source = source.map(str::to_string);
    let mut license_ack = false;
    let mut image_choice = image_choice;
    loop {
        match daemon.install(
            &spec,
            source.as_deref(),
            name,
            branch,
            tag,
            license_ack,
            image_choice,
        ) {
            Ok(outcome) => return Ok(outcome),
            Err(err) => {
                if let Some(chosen) = pick_version(&err)? {
                    spec = chosen.spec;
                    source = chosen.source;
                    continue;
                }
                if let Some(chosen) = pick_source(&err)? {
                    source = Some(chosen);
                    continue;
                }
                if let Some(chosen) = pick_image(&err)? {
                    image_choice = Some(chosen);
                    continue;
                }
                if accept_license(&err)? {
                    license_ack = true;
                    continue;
                }
                return Err(err);
            }
        }
    }
}

fn print_install_outcome(outcome: &pkg::InstallOutcome) {
    match outcome {
        pkg::InstallOutcome::App(report) => {
            println!("{}", tf2(Msg::PkgInstalled, &report.id, &report.version));
            println!("{}", tf(Msg::PkgStartHint, &report.id));
        }
        pkg::InstallOutcome::Stack {
            stack,
            installed,
            skipped,
        } => {
            for id in skipped {
                println!("{}", tf(Msg::PkgStackAppSkipped, id));
            }
            for report in installed {
                println!("{}", tf2(Msg::PkgInstalled, &report.id, &report.version));
            }
            println!("{}", tf2(Msg::PkgStackInstalled, stack, installed.len()));
            // Dependency order doubles as the recommended start order.
            for report in installed {
                println!("{}", tf(Msg::PkgStartHint, &report.id));
            }
        }
    }
}

/// `asc install <git-url>`: bypasses the registry entirely and clones the
/// repository straight in — for one-off installs and private forks that
/// were never published to a registry. `--branch`/`--tag` pick the ref to
/// check out (default branch HEAD otherwise); private repositories reuse the
/// same `asc auth` credentials as registry installs (host/prefix matching).
#[allow(clippy::too_many_arguments)]
fn install_from_git_cmd(
    url: &str,
    source: Option<&str>,
    name: Option<String>,
    branch: Option<&str>,
    tag: Option<&str>,
    image_choice: Option<ImageSource>,
    config: &Config,
    daemon: Option<client::Daemon>,
) -> anyhow::Result<()> {
    if source.is_some() {
        anyhow::bail!("--source has no effect on a direct repository install");
    }
    let name = match name {
        Some(name) => Some(name),
        None => prompt_git_app_name(url, config, daemon.as_ref())?,
    };
    let name = name.as_deref();
    if let Some(daemon) = &daemon {
        let outcome = install_daemon_loop(daemon, url, None, name, branch, tag, image_choice)?;
        print_install_outcome(&outcome);
        return Ok(());
    }
    let ctx = UserContext::current();
    let git_ref = match (branch, tag) {
        (Some(b), None) => Some(pkg::GitRef::Branch(b)),
        (None, Some(t)) => Some(pkg::GitRef::Tag(t)),
        (None, None) => None,
        (Some(_), Some(_)) => unreachable!("clap rejects --branch together with --tag"),
    };
    let mut license_ack = false;
    let mut image_choice = image_choice;
    // Same interactive recoveries as a registry install, minus the source
    // pick (there is only ever one source: the URL itself).
    let report = loop {
        match pkg::install_from_git(config, &ctx, url, git_ref, name, license_ack, image_choice) {
            Ok(report) => break report,
            Err(err) if offer_auth_setup(&err) => continue,
            Err(err) => {
                if let Some(chosen) = pick_image(&err)? {
                    image_choice = Some(chosen);
                    continue;
                }
                if accept_license(&err)? {
                    license_ack = true;
                    continue;
                }
                return Err(err);
            }
        }
    };
    println!("{}", tf2(Msg::PkgInstalled, &report.id, &report.version));
    println!("{}", tf(Msg::PkgStartHint, &report.id));
    Ok(())
}

/// Interactive custom-name prompt of `asc install` (DMN-024): Enter keeps
/// the default (the name from the package manifest), anything else becomes
/// the app's name — commands then accept it interchangeably with the id.
/// When instances of the package are already installed, the default shows
/// the suffixed instance id the install would allocate (DMN-033). For a
/// whole-stack spec the name is a prefix (DMN-034) and the prompt says so.
/// Skipped for non-interactive stdin, where `--name` is the way.
fn prompt_app_name(
    spec: &str,
    config: &Config,
    daemon: Option<&client::Daemon>,
) -> anyhow::Result<Option<String>> {
    // SAFETY: isatty() has no preconditions.
    if unsafe { libc::isatty(libc::STDIN_FILENO) } != 1 {
        return Ok(None);
    }
    let base = spec.split_once('@').map(|(n, _)| n).unwrap_or(spec);
    // A whole-stack install names its apps by prefix; the check is
    // best-effort (local index only) — any doubt falls back to the app
    // prompt, the install itself resolves the package authoritatively.
    let is_stack = !base.contains('/')
        && RegistryClient::new(config)
            .and_then(|client| client.resolve_all(base))
            .map(|candidates| {
                !candidates.is_empty() && candidates.iter().all(|c| c.entry.package_type == "stack")
            })
            .unwrap_or(false);
    let default = instance_default(base, config, daemon);
    let msg = if is_stack {
        Msg::PkgPromptStackName
    } else {
        Msg::PkgPromptName
    };
    let answer = read_line(&tf(msg, &default))?;
    Ok(Some(answer).filter(|a| !a.is_empty()))
}

/// Same custom-name prompt as [`prompt_app_name`], for a direct repository
/// install: the default is derived from the repository name instead of a
/// registry manifest (there is none to read yet).
fn prompt_git_app_name(
    url: &str,
    config: &Config,
    daemon: Option<&client::Daemon>,
) -> anyhow::Result<Option<String>> {
    // SAFETY: isatty() has no preconditions.
    if unsafe { libc::isatty(libc::STDIN_FILENO) } != 1 {
        return Ok(None);
    }
    let base = pkg::repo_name(url)?;
    let default = instance_default(&base, config, daemon);
    let answer = read_line(&tf(Msg::PkgPromptName, &default))?;
    Ok(Some(answer).filter(|a| !a.is_empty()))
}

/// The default instance id shown in the name prompt (DMN-033): the next
/// free `<base>`, `<base>-2`, ... Against the daemon's app list when the
/// install goes through the daemon (that is where the id will be
/// allocated), against the local store otherwise. Best-effort either way —
/// the install itself allocates the id authoritatively.
fn instance_default(base: &str, config: &Config, daemon: Option<&client::Daemon>) -> String {
    let Some(daemon) = daemon else {
        // Stack specs ('stack/app') are not valid ids; fall back to the spec.
        let store = asc_daemon::daemon::apps::AppStore::new(config.daemon.apps_dir.clone());
        return pkg::instance_id(&store, base).unwrap_or_else(|_| base.to_string());
    };
    let Ok(apps) = daemon.list() else {
        return base.to_string();
    };
    let taken: std::collections::HashSet<&str> = apps
        .iter()
        .flat_map(|a| [a.id.as_str(), a.name.as_str()])
        .collect();
    if !taken.contains(base) {
        return base.to_string();
    }
    for n in 2u32.. {
        let candidate = format!("{base}-{n}");
        if candidate.len() > 64 {
            break;
        }
        if !taken.contains(candidate.as_str()) {
            return candidate;
        }
    }
    base.to_string()
}

fn auth_cmd(action: AuthAction) -> anyhow::Result<()> {
    use asc_daemon::daemon::pkg::auth::{GitAuth, Kind, Method, normalize};
    let mut auth = GitAuth::load()?;
    match action {
        AuthAction::Add {
            target,
            kind,
            token,
            ssh_key,
            username,
            app,
        } => {
            let kind = Kind::parse(&kind)?;
            let method = match (token, ssh_key) {
                (Some(token), None) => Method::Token { token },
                (None, Some(Some(key))) => Method::SshKey { key },
                (None, Some(None)) => Method::SshKey {
                    key: pick_ssh_key(&target)?,
                },
                _ => anyhow::bail!(t(Msg::AuthNeedMethod)),
            };
            // A registry accepts only a token, and the Engine wants a user
            // name with it — fail here rather than at the first pull.
            if kind == Kind::Registry {
                if matches!(method, Method::SshKey { .. }) {
                    anyhow::bail!(t(Msg::AuthRegistryNeedsToken));
                }
                if username.is_none() {
                    anyhow::bail!(t(Msg::AuthRegistryNeedsUsername));
                }
            }
            let saved = auth.add(kind, &target, method, username, app)?;
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
                let kind_w = 8;
                for (cred, scope) in all {
                    // The app binding is part of what the entry addresses, so
                    // it belongs next to the pattern; secrets never print.
                    let bound = match &cred.app {
                        Some(app) => format!("  app={app}"),
                        None => String::new(),
                    };
                    println!(
                        "{:<kind_w$}  {:<name_w$}  {:<6}  {}{}",
                        cred.kind.label(),
                        cred.pattern,
                        scope.label(),
                        cred.method.label(),
                        bound,
                    );
                }
            }
        }
        AuthAction::Remove { target, kind } => {
            let kind = kind.as_deref().map(Kind::parse).transpose()?;
            auth.remove(kind, &target)?;
            auth.save()?;
            println!("{}", tf(Msg::AuthRemoved, normalize(&target)));
        }
    }
    Ok(())
}

/// `asc backup ...` (DMN-009): create/restore/list/prune archives and manage
/// the storages they go to. Every subcommand resolves the app through
/// `get_authorized`, so a user only ever touches their own apps' backups
/// (root, everyone's) — same rule as every other `asc app` command.
fn backup_cmd(action: BackupAction, config: &Config) -> anyhow::Result<()> {
    use asc_daemon::daemon::backup::{self, storage};
    use asc_daemon::daemon::pkg::settings::SettingValues;

    match action {
        BackupAction::Create { app, storages } => {
            let manager = AppManager::new(config);
            let ctx = UserContext::current();
            let meta = manager.get_authorized(&ctx, &app)?;
            let config_dir = manager.store().app_dir(&meta.id)?.join("config");
            let policy = SettingValues::load(&config_dir)?
                .backup_policy()?
                .unwrap_or_default();
            // Explicit --storage wins; otherwise the app's own policy;
            // otherwise just the always-available local storage.
            let targets = if !storages.is_empty() {
                storages
            } else if !policy.storages.is_empty() {
                policy.storages.clone()
            } else {
                vec![storage::LOCAL_NAME.to_string()]
            };
            let storage_list = storage::StorageList::load()?;
            for name in &targets {
                let info = backup::create_backup(
                    config,
                    manager.store(),
                    &meta,
                    &storage_list,
                    name,
                    policy.keep,
                )?;
                println!(
                    "{}",
                    tf3(
                        Msg::BackupCreated,
                        &info.name,
                        &info.storage,
                        monitor::human_bytes(info.bytes)
                    )
                );
            }
        }
        BackupAction::Restore {
            app,
            backup: backup_name,
            storage,
        } => {
            let manager = AppManager::new(config);
            let ctx = UserContext::current();
            let status = manager.status(&ctx, &app)?;
            if status.state == RuntimeState::Running {
                anyhow::bail!(tf2(
                    Msg::BackupRestoreStopFirst,
                    &status.meta.id,
                    &status.meta.id
                ));
            }
            let storage_name = storage.unwrap_or_else(|| storage::LOCAL_NAME.to_string());
            let storage_list = storage::StorageList::load()?;
            backup::restore_backup(
                config,
                manager.store(),
                &status.meta,
                &storage_list,
                &storage_name,
                &backup_name,
            )?;
            println!(
                "{}",
                tf2(Msg::BackupRestored, &status.meta.id, &backup_name)
            );
        }
        BackupAction::List { app, storage } => {
            let manager = AppManager::new(config);
            let ctx = UserContext::current();
            let meta = manager.get_authorized(&ctx, &app)?;
            let storage_name = storage.unwrap_or_else(|| storage::LOCAL_NAME.to_string());
            let storage_list = storage::StorageList::load()?;
            let names = backup::list_backups(config, &storage_list, &storage_name, &meta.id)?;
            if names.is_empty() {
                println!("{}", t(Msg::BackupListEmpty));
            } else {
                for name in names {
                    println!("{name}");
                }
            }
        }
        BackupAction::Prune { app, storage, keep } => {
            let manager = AppManager::new(config);
            let ctx = UserContext::current();
            let meta = manager.get_authorized(&ctx, &app)?;
            let storage_name = storage.unwrap_or_else(|| storage::LOCAL_NAME.to_string());
            let storage_list = storage::StorageList::load()?;
            let impl_ = backup::resolve_storage(config, &storage_list, &storage_name)?;
            let removed = backup::prune(impl_.as_ref(), &meta.id, keep)?;
            println!("{}", tf2(Msg::BackupPruned, removed.len(), &storage_name));
        }
        BackupAction::Storage { action } => storage_action_cmd(action)?,
    }
    Ok(())
}

/// `asc backup storage ...`: 'local' is always available and never listed
/// as an entry (its directory is fixed); everything else is configured here.
fn storage_action_cmd(action: StorageAction) -> anyhow::Result<()> {
    use asc_daemon::daemon::backup::storage::{StorageKind, StorageList};

    match action {
        StorageAction::Add(args) => {
            let StorageAddArgs {
                name,
                r#type,
                dir,
                bucket,
                region,
                endpoint,
                access_key,
                secret_key,
                prefix,
                host,
                port,
                user,
                password,
                key,
            } = *args;
            let require = |field: Option<String>, flag: &str, kind: &str| {
                field
                    .ok_or_else(|| anyhow::anyhow!(tf2(Msg::BackupStorageMissingField, flag, kind)))
            };
            let kind = match r#type {
                StorageType::Local => StorageKind::Local {
                    dir: require(dir, "--dir", "local")?.into(),
                },
                StorageType::S3 => StorageKind::S3 {
                    bucket: require(bucket, "--bucket", "s3")?,
                    region: require(region, "--region", "s3")?,
                    endpoint,
                    access_key: require(access_key, "--access-key", "s3")?,
                    secret_key: require(secret_key, "--secret-key", "s3")?,
                    prefix,
                },
                StorageType::Ftp => StorageKind::Ftp {
                    host: require(host, "--host", "ftp")?,
                    port: port.unwrap_or(21),
                    user: require(user, "--user", "ftp")?,
                    password: require(password, "--password", "ftp")?,
                    dir,
                },
                StorageType::Sftp => {
                    if password.is_none() && key.is_none() {
                        anyhow::bail!(tf2(
                            Msg::BackupStorageMissingField,
                            "--password or --key",
                            "sftp"
                        ));
                    }
                    StorageKind::Sftp {
                        host: require(host, "--host", "sftp")?,
                        port: port.unwrap_or(22),
                        user: require(user, "--user", "sftp")?,
                        password,
                        key,
                        dir,
                    }
                }
            };
            let mut storages = StorageList::load()?;
            storages.add(&name, kind)?;
            storages.save()?;
            println!("{}", tf(Msg::BackupStorageAdded, &name));
        }
        StorageAction::List => {
            let storages = StorageList::load()?;
            let names = storages.names();
            let name_w = names.iter().map(|n| n.len()).max().unwrap_or(4).max(4);
            for name in &names {
                let label = storages
                    .get(name)
                    .map(|e| e.kind.label())
                    .unwrap_or("local");
                println!("{name:<name_w$}  {label}");
            }
        }
        StorageAction::Remove { name } => {
            let mut storages = StorageList::load()?;
            storages.remove(&name)?;
            storages.save()?;
            println!("{}", tf(Msg::BackupStorageRemoved, &name));
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
/// When `err` says several sources provide the package and stdin is a
/// terminal, print the numbered candidate list (source + repository) and let
/// the user pick one. `Ok(None)` = not that error or non-interactive.
fn pick_source(err: &anyhow::Error) -> anyhow::Result<Option<String>> {
    let Some(ambiguous) = err.downcast_ref::<pkg::AmbiguousPackage>() else {
        return Ok(None);
    };
    // Non-interactive callers get the structured error with the hint.
    // SAFETY: isatty() has no preconditions.
    if unsafe { libc::isatty(libc::STDIN_FILENO) } != 1 {
        return Ok(None);
    }
    println!("{}", tf(Msg::PkgPickSource, &ambiguous.name));
    for (i, (source, git)) in ambiguous.candidates.iter().enumerate() {
        println!("  {}) {source}  {git}", i + 1);
    }
    let choice = read_line("> ")?;
    let index: usize = choice
        .parse()
        .ok()
        .filter(|n| (1..=ambiguous.candidates.len()).contains(n))
        .ok_or_else(|| anyhow::anyhow!(t(Msg::AuthInvalidChoice)))?;
    Ok(Some(ambiguous.candidates[index - 1].0.clone()))
}

/// When `err` says the manifest offers both a prebuilt image and a local
/// build (DMN-050) and stdin is a terminal, print the two options and let the
/// user pick one. `Ok(None)` = not that error or non-interactive (the caller
/// then surfaces the `--image`/`--build` hint from the error message).
fn pick_image(err: &anyhow::Error) -> anyhow::Result<Option<ImageSource>> {
    let Some(choice) = err.downcast_ref::<pkg::ImageChoiceRequired>() else {
        return Ok(None);
    };
    // SAFETY: isatty() has no preconditions.
    if unsafe { libc::isatty(libc::STDIN_FILENO) } != 1 {
        return Ok(None);
    }
    println!("{}", tf(Msg::PkgPickImage, &choice.package));
    println!("  1) {}", tf(Msg::PkgImageOptionPrebuilt, &choice.image));
    println!("  2) {}", tf(Msg::PkgImageOptionBuild, &choice.build));
    let input = read_line("> ")?;
    match input.trim() {
        "1" => Ok(Some(ImageSource::Prebuilt)),
        "2" => Ok(Some(ImageSource::Build)),
        _ => Err(anyhow::anyhow!(t(Msg::AuthInvalidChoice))),
    }
}

/// The user's chosen install spec after a version pick (DMN-048).
struct VersionChoice {
    /// Re-invocation spec: `<package>@<ref>`.
    spec: String,
    /// The source that was already selected, to keep across the retry.
    source: Option<String>,
}

/// When `err` says `asc install pkg@` needs a version and stdin is a terminal,
/// print the repository's tags and branches as a numbered list and let the
/// user pick one, returning the `pkg@<ref>` spec to retry with. `Ok(None)` =
/// not that error or non-interactive (the caller then surfaces the list in the
/// error message).
fn pick_version(err: &anyhow::Error) -> anyhow::Result<Option<VersionChoice>> {
    let Some(choice) = err.downcast_ref::<pkg::VersionChoiceRequired>() else {
        return Ok(None);
    };
    // SAFETY: isatty() has no preconditions.
    if unsafe { libc::isatty(libc::STDIN_FILENO) } != 1 {
        return Ok(None);
    }
    println!("{}", tf(Msg::PkgPickVersion, &choice.package));
    // Tags first (the common pick), then branches; both share one numbering.
    let mut refs: Vec<&str> = Vec::new();
    if !choice.tags.is_empty() {
        println!("  {}:", t(Msg::PkgTagsHeader));
        for tag in &choice.tags {
            println!("  {}) {tag}", refs.len() + 1);
            refs.push(tag);
        }
    }
    if !choice.branches.is_empty() {
        println!("  {}:", t(Msg::PkgBranchesHeader));
        for branch in &choice.branches {
            println!("  {}) {branch}", refs.len() + 1);
            refs.push(branch);
        }
    }
    let input = read_line("> ")?;
    let index: usize = input
        .parse()
        .ok()
        .filter(|n| (1..=refs.len()).contains(n))
        .ok_or_else(|| anyhow::anyhow!(t(Msg::AuthInvalidChoice)))?;
    Ok(Some(VersionChoice {
        spec: format!("{}@{}", choice.package, refs[index - 1]),
        source: choice.source.clone(),
    }))
}

/// When `err` says the package repository ships a license (DMN-028), print
/// where the package comes from (source + repository), the license text, and
/// ask for consent. Non-interactive stdin accepts automatically with a
/// notice, so scripted installs keep working. `Ok(false)` = not that error,
/// or the user declined.
fn accept_license(err: &anyhow::Error) -> anyhow::Result<bool> {
    let Some(required) = err.downcast_ref::<pkg::LicenseRequired>() else {
        return Ok(false);
    };
    println!(
        "{}",
        tf3(
            Msg::PkgLicenseNotice,
            &required.package,
            &required.source,
            &required.git
        )
    );
    println!();
    println!("{}", required.license.trim_end());
    println!();
    // SAFETY: isatty() has no preconditions.
    if unsafe { libc::isatty(libc::STDIN_FILENO) } != 1 {
        println!("{}", t(Msg::PkgLicenseAutoAccepted));
        return Ok(true);
    }
    let answer = read_line(t(Msg::PkgLicensePrompt))?;
    Ok(matches!(
        answer.to_lowercase().as_str(),
        "y" | "yes" | "д" | "да"
    ))
}

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
        // This recovery only ever runs for a git clone, so the credential is
        // a repo one, applying to every app under that host.
        let saved = store.add(auth::Kind::Repo, &host, method, None, None)?;
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
    // No version column: the version is a git tag of the package repository
    // (DMN-047), resolved at install time — printing it here would mean an
    // `ls-remote` per row. `asc install <pkg>@` lists a package's versions.
    for pkg in results {
        println!(
            "{:<name_w$}  {}",
            pkg.entry.name,
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
    // Lifecycle commands go through the daemon when it is present
    // (DMN-042); the rest operate on local files/consoles and stay
    // in-process until their daemon RPCs exist (DMN-043).
    let routable = matches!(
        action,
        AppAction::List
            | AppAction::Info { .. }
            | AppAction::Start { .. }
            | AppAction::Stop { .. }
            | AppAction::Restart { .. }
            | AppAction::Logs { .. }
            | AppAction::Remove { .. }
    );
    if routable && let Some(daemon) = daemon_backend(config)? {
        return app_cmd_daemon(&daemon, action, config);
    }
    app_cmd_local(action, config)
}

/// App commands over the daemon's unix socket: the daemon reads the
/// caller's uid from SO_PEERCRED and shows/manages only their apps (root —
/// everyone's), so no docker group and no sudo are needed here.
fn app_cmd_daemon(
    daemon: &client::Daemon,
    action: AppAction,
    config: &Config,
) -> anyhow::Result<()> {
    match action {
        AppAction::List => {
            let rows: Vec<AppRow> = daemon.list()?.iter().map(remote_row).collect();
            print_app_list(&rows, UserContext::current().is_root, "");
        }
        AppAction::Info { id } => {
            let app = daemon.info(&id)?;
            println!("{}  {}", app.id, app.name);
            if let Some(title) = &app.title {
                println!("  title:   {title}");
            }
            println!("  kind:    {}", app.kind);
            println!("  state:   {}", remote_state_label(&app.state));
            println!("  version: {}", app.version.as_deref().unwrap_or("-"));
            println!("  source:  {}", app.source.as_deref().unwrap_or("-"));
            if let Some(quota) = &app.quota {
                println!("  quota:   {}", quota_label(quota));
            }
            println!("  {}", tf(Msg::OwnerLabel, &app.owner));
        }
        AppAction::Start { id, detach } => {
            match with_spinner(|| daemon.start(&id))? {
                false => println!("{}", tf(Msg::AppStarted, &id)),
                true => println!("{}", tf(Msg::AppAlreadyRunning, &id)),
            }
            // Auto-attach like the in-process path — the attach itself
            // still opens Docker directly, so it only works where the
            // caller can reach docker.sock (root); the console over the
            // daemon socket is a follow-up (DMN-043).
            // SAFETY: isatty() has no preconditions.
            let interactive = unsafe { libc::isatty(libc::STDIN_FILENO) } == 1;
            if !detach
                && interactive
                && UserContext::current().is_root
                && daemon.info(&id)?.kind == "docker"
            {
                attach_cmd(&id, config)?;
            }
        }
        AppAction::Stop { id } => match with_spinner(|| daemon.stop(&id))? {
            false => println!("{}", tf(Msg::AppStopped, &id)),
            true => println!("{}", tf(Msg::AppNotRunning, &id)),
        },
        AppAction::Restart { id } => {
            with_spinner(|| daemon.restart(&id))?;
            println!("{}", tf(Msg::AppRestarted, &id));
        }
        AppAction::Logs { id, tail } => {
            let logs = daemon.logs(&id, tail)?;
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
            daemon.remove(&id)?;
            println!("{}", tf(Msg::AppRemoved, &id));
        }
        // Only lifecycle actions are routed here (see app_cmd).
        other => app_cmd_local(other, config)?,
    }
    Ok(())
}

fn app_cmd_local(action: AppAction, config: &Config) -> anyhow::Result<()> {
    let manager = AppManager::new(config);
    let ctx = UserContext::current();
    match action {
        AppAction::List => {
            let rows: Vec<AppRow> = manager.list(&ctx)?.iter().map(local_row).collect();
            print_app_list(&rows, ctx.is_root, "");
        }
        AppAction::Info { id } => {
            let status = manager.status(&ctx, &id)?;
            let m = &status.meta;
            println!("{}  {}", m.id, m.display_name());
            if m.custom_name.is_some() {
                println!("  title:   {}", m.name);
            }
            println!("  kind:    {}", m.runtime.kind());
            println!("  state:   {}", state_label(status.state));
            println!("  version: {}", m.version.as_deref().unwrap_or("-"));
            println!("  source:  {}", m.source.as_deref().unwrap_or("-"));
            if let Some(quota) = &m.quota {
                println!("  quota:   {}", quota_label(quota));
            }
            println!("  {}", tf(Msg::OwnerLabel, &m.owner.name));
        }
        AppAction::Disk { id } => disk_cmd(id.as_deref(), config)?,
        AppAction::Ports { id } => ports_cmd(id.as_deref(), config)?,
        AppAction::Clone { id, name } => clone_cmd(&id, name, config)?,
        AppAction::Install {
            spec,
            source,
            name,
            branch,
            tag,
            image,
            build,
        } => install_cmd(
            &spec,
            source.as_deref(),
            name,
            branch.as_deref(),
            tag.as_deref(),
            image_choice_flag(image, build),
            config,
        )?,
        AppAction::Attach { id } => attach_cmd(&id, config)?,
        AppAction::Upgrade { spec } => upgrade_cmd(&spec, config)?,
        AppAction::Start { id, detach } => {
            confirm_start_resources(&manager, &ctx, &id, config)?;
            match with_spinner(|| manager.start(&ctx, &id))? {
                Outcome::Done => println!("{}", tf(Msg::AppStarted, &id)),
                Outcome::AlreadyInState => println!("{}", tf(Msg::AppAlreadyRunning, &id)),
            }
            // Like `docker run` without -d: the console is attached by
            // default. Skipped with -d, for non-Docker apps (nothing to
            // attach to) and for non-interactive stdin (scripts must not
            // block on an interactive console).
            // SAFETY: isatty() has no preconditions.
            let interactive = unsafe { libc::isatty(libc::STDIN_FILENO) } == 1;
            if !detach
                && interactive
                && matches!(
                    manager.status(&ctx, &id)?.meta.runtime,
                    Runtime::Docker { .. }
                )
            {
                attach_cmd(&id, config)?;
            }
        }
        AppAction::Stop { id } => match with_spinner(|| manager.stop(&ctx, &id))? {
            Outcome::Done => println!("{}", tf(Msg::AppStopped, &id)),
            Outcome::AlreadyInState => println!("{}", tf(Msg::AppNotRunning, &id)),
        },
        AppAction::Restart { id } => {
            with_spinner(|| manager.restart(&ctx, &id))?;
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

/// Requirements check before start (DMN-029): compare the manifest
/// `requirements` with what the host has free right now; when short, warn
/// and — interactively — ask to continue at the user's own risk. Read
/// failures (manifest, metrics) never block the start: the check is advice,
/// not enforcement.
fn confirm_start_resources(
    manager: &AppManager,
    ctx: &UserContext,
    reference: &str,
    config: &Config,
) -> anyhow::Result<()> {
    use asc_daemon::daemon::pkg::manifest::Manifest;
    use asc_daemon::daemon::pkg::settings::locate_installed;

    // Missing/foreign apps: let `start` report the proper error.
    let Ok(meta) = manager.get_authorized(ctx, reference) else {
        return Ok(());
    };
    let Ok(app_dir) = manager.store().app_dir(&meta.id) else {
        return Ok(());
    };
    let Ok((manifest_dir, _)) = locate_installed(config, &meta, &app_dir) else {
        return Ok(());
    };
    let Ok(manifest) = Manifest::load(&manifest_dir) else {
        return Ok(());
    };
    let Some(req) = &manifest.requirements else {
        return Ok(());
    };
    let Ok(metrics) = monitor::system::snapshot_blocking() else {
        return Ok(());
    };
    let shortages = resource_shortages(req, &metrics, &app_dir);
    if shortages.is_empty() {
        return Ok(());
    }
    eprintln!(
        "{}",
        tf2(Msg::AppLowResources, &meta.id, shortages.join(", "))
    );
    // Scripts get the warning on stderr and proceed; a human decides.
    // SAFETY: isatty() has no preconditions.
    if unsafe { libc::isatty(libc::STDIN_FILENO) } != 1 {
        return Ok(());
    }
    let answer = read_line(t(Msg::AppStartRiskPrompt))?;
    if !matches!(answer.to_lowercase().as_str(), "y" | "yes" | "д" | "да") {
        anyhow::bail!(tf(Msg::AppStartDeclined, &meta.id));
    }
    Ok(())
}

/// Requirements the host cannot cover right now, as language-neutral
/// `need > have` figures ("RAM 4.0 GiB > 1.2 GiB"). Unparsable requirement
/// strings are skipped — a broken manifest must not block the start.
fn resource_shortages(
    req: &asc_daemon::daemon::pkg::manifest::Requirements,
    metrics: &monitor::system::SystemMetrics,
    app_dir: &std::path::Path,
) -> Vec<String> {
    use asc_daemon::daemon::pkg::settings::parse_size;
    let mut short = Vec::new();
    if let Some(need) = req.ram.as_deref().and_then(|s| parse_size(s).ok())
        && need > metrics.memory.available
    {
        short.push(format!(
            "RAM {} > {}",
            monitor::human_bytes(need),
            monitor::human_bytes(metrics.memory.available)
        ));
    }
    if let Some(need) = req.cpu
        && need > metrics.cpu.cores as f64
    {
        short.push(format!("CPU {need} > {}", metrics.cpu.cores));
    }
    if let Some(need) = req.disk.as_deref().and_then(|s| parse_size(s).ok()) {
        // The filesystem the app directory lives on: longest matching mount.
        let disk = metrics
            .disks
            .iter()
            .filter(|d| app_dir.starts_with(&d.mount))
            .max_by_key(|d| d.mount.len());
        if let Some(disk) = disk
            && need > disk.available
        {
            short.push(format!(
                "disk {} > {} ({})",
                monitor::human_bytes(need),
                monitor::human_bytes(disk.available),
                disk.mount
            ));
        }
    }
    short
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

/// `asc app disk <id>` (DMN-035): a quota bar when a disk quota is set, then
/// an itemized breakdown. Column labels are technical identifiers and stay
/// English, like the rest of `asc app info`.
fn disk_cmd(reference: Option<&str>, config: &Config) -> anyhow::Result<()> {
    use asc_daemon::daemon::apps::disk;

    let manager = AppManager::new(config);
    let ctx = UserContext::current();
    let Some(reference) = reference else {
        return disk_summary_cmd(&manager, &ctx);
    };
    let meta = manager.get_authorized(&ctx, reference)?;
    let usage = disk::usage(config, manager.store(), &meta)?;

    println!("{}  {}", meta.id, meta.display_name());
    match usage.quota_bytes {
        Some(quota) => {
            println!("  {}", usage_string(usage.app_dir_bytes, quota));
            println!("  {}", static_bar(usage.app_dir_bytes, quota, 30));
        }
        None => println!("  {}", monitor::human_bytes(usage.app_dir_bytes)),
    }
    println!();
    println!(
        "  Images:      {}",
        usage
            .image_bytes
            .map(monitor::human_bytes)
            .unwrap_or_else(|| "-".to_string())
    );
    println!(
        "  Repository:  {}",
        monitor::human_bytes(usage.repository_bytes)
    );
    println!("  Data:        {}", monitor::human_bytes(usage.data_bytes));
    if !usage.volumes.is_empty() {
        println!("  Volumes:");
        for volume in &usage.volumes {
            let size = volume
                .bytes
                .map(monitor::human_bytes)
                .unwrap_or_else(|| "-".to_string());
            let note = if volume.shared {
                " (shared, not counted)"
            } else if !volume.counted {
                " (not counted)"
            } else {
                ""
            };
            println!("    {} -> {}{note}  {size}", volume.entry, volume.path);
        }
    }
    Ok(())
}

/// `asc disk` with no id: total space occupied by every visible app (root
/// sees all users' apps, like [`print_app_list`]) as a bar against the apps
/// store's filesystem capacity, then each app's own usage, largest first.
/// Sizes are the cheap directory-walk figure ([`disk::dir_size`], the same
/// one `asc stats` uses) — no image/volume breakdown, no Docker queries.
fn disk_summary_cmd(manager: &AppManager, ctx: &UserContext) -> anyhow::Result<()> {
    use asc_daemon::daemon::apps::disk;
    use asc_daemon::daemon::monitor::system;

    let mut rows: Vec<(AppStatus, u64)> = manager
        .list(ctx)?
        .into_iter()
        .map(|app| {
            let bytes = manager
                .store()
                .app_dir(&app.meta.id)
                .map(|dir| disk::dir_size(&dir))
                .unwrap_or(0);
            (app, bytes)
        })
        .collect();
    rows.sort_by_key(|(_, bytes)| std::cmp::Reverse(*bytes));

    let apps_bytes: u64 = rows.iter().map(|(_, bytes)| bytes).sum();
    match system::filesystem_total(manager.store().root()) {
        Some(fs_total) => {
            println!("Apps: {}", usage_string(apps_bytes, fs_total));
            println!("{}", static_bar(apps_bytes, fs_total, 30));
        }
        None => println!("Apps: {}", monitor::human_bytes(apps_bytes)),
    }
    println!();

    if rows.is_empty() {
        println!("{}", t(Msg::AppListEmpty));
        return Ok(());
    }
    let id_w = rows
        .iter()
        .map(|(app, _)| app.meta.id.len())
        .max()
        .unwrap_or(2)
        .max(2);
    let name_w = rows
        .iter()
        .map(|(app, _)| app.meta.display_name().chars().count())
        .max()
        .unwrap_or(4)
        .max(4);
    let show_user = ctx.is_root;
    let user_w = rows
        .iter()
        .map(|(app, _)| app.meta.owner.name.chars().count())
        .max()
        .unwrap_or(4)
        .max(4);
    if show_user {
        println!(
            "{:<id_w$}  {:<name_w$}  {:<user_w$}  SIZE",
            "ID", "NAME", "USER"
        );
    } else {
        println!("{:<id_w$}  {:<name_w$}  SIZE", "ID", "NAME");
    }
    for (app, bytes) in &rows {
        if show_user {
            println!(
                "{:<id_w$}  {:<name_w$}  {:<user_w$}  {}",
                app.meta.id,
                app.meta.display_name(),
                app.meta.owner.name,
                monitor::human_bytes(*bytes),
            );
        } else {
            println!(
                "{:<id_w$}  {:<name_w$}  {}",
                app.meta.id,
                app.meta.display_name(),
                monitor::human_bytes(*bytes),
            );
        }
    }
    Ok(())
}

/// `asc ports [<app>]` / `asc app ports [<app>]` (DMN-049): with an id, the
/// app's published ports one per line; without one, a table of every visible
/// app and its ports (root sees all users' apps, like [`print_app_list`]).
fn ports_cmd(reference: Option<&str>, config: &Config) -> anyhow::Result<()> {
    use asc_daemon::daemon::apps::ports;

    let manager = AppManager::new(config);
    let ctx = UserContext::current();
    let Some(reference) = reference else {
        return ports_summary_cmd(&manager, &ctx, config);
    };
    let meta = manager.get_authorized(&ctx, reference)?;
    let list = ports::published(config, manager.store(), &meta)?;

    println!("{}  {}", meta.id, meta.display_name());
    if list.is_empty() {
        println!("  {}", t(Msg::PortsNone));
    } else {
        for (port, protocol) in &list {
            println!("  {}", port_label(*port, *protocol));
        }
    }
    Ok(())
}

/// `asc ports` with no id: every visible app and the host==container ports it
/// publishes, as a table (root sees all users' apps, like [`print_app_list`]).
/// Ports come from the app's settings ([`ports::published`]); an app whose
/// manifest cannot be read shows no ports rather than failing the report.
fn ports_summary_cmd(
    manager: &AppManager,
    ctx: &UserContext,
    config: &Config,
) -> anyhow::Result<()> {
    use asc_daemon::daemon::apps::ports;

    let rows: Vec<(AppStatus, String)> = manager
        .list(ctx)?
        .into_iter()
        .map(|app| {
            let label = match ports::published(config, manager.store(), &app.meta) {
                Ok(list) if !list.is_empty() => list
                    .iter()
                    .map(|(port, protocol)| port_label(*port, *protocol))
                    .collect::<Vec<_>>()
                    .join(", "),
                _ => "-".to_string(),
            };
            (app, label)
        })
        .collect();

    if rows.is_empty() {
        println!("{}", t(Msg::AppListEmpty));
        return Ok(());
    }
    let id_w = rows
        .iter()
        .map(|(app, _)| app.meta.id.len())
        .max()
        .unwrap_or(2)
        .max(2);
    let name_w = rows
        .iter()
        .map(|(app, _)| app.meta.display_name().chars().count())
        .max()
        .unwrap_or(4)
        .max(4);
    let show_user = ctx.is_root;
    let user_w = rows
        .iter()
        .map(|(app, _)| app.meta.owner.name.chars().count())
        .max()
        .unwrap_or(4)
        .max(4);
    if show_user {
        println!(
            "{:<id_w$}  {:<name_w$}  {:<user_w$}  PORTS",
            "ID", "NAME", "USER"
        );
    } else {
        println!("{:<id_w$}  {:<name_w$}  PORTS", "ID", "NAME");
    }
    for (app, label) in &rows {
        if show_user {
            println!(
                "{:<id_w$}  {:<name_w$}  {:<user_w$}  {}",
                app.meta.id,
                app.meta.display_name(),
                app.meta.owner.name,
                label,
            );
        } else {
            println!(
                "{:<id_w$}  {:<name_w$}  {}",
                app.meta.id,
                app.meta.display_name(),
                label,
            );
        }
    }
    Ok(())
}

/// One published port with its transport, `docker ps`-style: `27015/tcp`,
/// `27015/udp`, or `27015/tcp+udp` when both transports share the port.
fn port_label(port: u16, protocol: docker::PortProtocol) -> String {
    format!("{port}/{}", protocol.transports().join("+"))
}

/// Static `[████░░░░]`-style bar (not `indicatif` — that's for streaming
/// operations; this renders once for a plain snapshot).
fn static_bar(used: u64, total: u64, width: usize) -> String {
    let filled = if total == 0 {
        0
    } else {
        ((used as u128 * width as u128) / total as u128).min(width as u128) as usize
    };
    format!("[{}{}]", "█".repeat(filled), "░".repeat(width - filled))
}

/// `asc app clone <id>` (DMN-019): a full copy of an app instance (data,
/// env, settings) under a new id — the CLI's part is the same custom-name
/// prompt as `asc install` plus a live byte-progress bar over the copy
/// (`docker pull`/`git clone` style, on by default on a terminal).
fn clone_cmd(reference: &str, name: Option<String>, config: &Config) -> anyhow::Result<()> {
    let manager = AppManager::new(config);
    let ctx = UserContext::current();
    let source = manager.get_authorized(&ctx, reference)?;
    let name = match name {
        Some(name) => Some(name),
        None => prompt_app_name(&source.id, config, None)?,
    };

    // The source directory's total size is only known once `pkg::clone_app`
    // measures it internally, so the bar starts empty and grows its length
    // on the first progress callback instead of being sized upfront.
    let bar = progress::interactive().then(|| progress::CopyBar::new(0));
    let mut bar_total = 0u64;
    let report = pkg::clone_app(
        config,
        &ctx,
        manager.store(),
        &source,
        name.as_deref(),
        |copied, total| {
            if let Some(bar) = &bar {
                if total != bar_total {
                    bar.set_length(total);
                    bar_total = total;
                }
                bar.set_position(copied);
            }
        },
    );
    if let Some(bar) = bar {
        bar.finish();
    }
    let meta = report?;
    println!("{}", tf2(Msg::AppCloned, &source.id, &meta.id));
    println!("{}", tf(Msg::PkgStartHint, &meta.id));
    Ok(())
}

/// `asc app settings <id>` — interactive settings editor (DMN-017/030).
/// The user first picks a **category** — environments, ports, volumes,
/// quota, start_command — then edits its settings. Package-defined settings
/// are validated against asc.settings.yaml; quota and start_command take
/// app-level overrides on top of the package values. Everything lands in
/// `config/settings.json`; the runtime picks it up on the next restart.
fn app_settings_cmd(reference: &str, config: &Config) -> anyhow::Result<()> {
    use asc_daemon::daemon::pkg::manifest::Manifest;
    use asc_daemon::daemon::pkg::settings::{
        SettingCategory, SettingValues, SettingsFile, manifest_dir_of,
    };

    let manager = AppManager::new(config);
    let ctx = UserContext::current();
    // The canonical id: `reference` may have been the app's custom name.
    let id = &manager.get_authorized(&ctx, reference)?.id;
    let app_dir = manager.store().app_dir(id)?;

    let manifest_dir = manifest_dir_of(config, &app_dir)?;
    let manifest = Manifest::load(&manifest_dir)?;
    let Some(file) = SettingsFile::load_for(&manifest_dir, &manifest)? else {
        println!("{}", tf(Msg::SettingsNone, id));
        return Ok(());
    };

    let config_dir = app_dir.join("config");
    let mut values = SettingValues::load(&config_dir)?;
    values.merge_defaults(&file.settings);

    let mut changed = false;
    loop {
        println!();
        println!("{}", tf(Msg::SettingsHeader, id));
        for (i, category) in SettingCategory::ALL.iter().enumerate() {
            let count = file
                .settings
                .iter()
                .filter(|d| d.kind.category() == *category)
                .count();
            let suffix = match category {
                SettingCategory::Quota
                | SettingCategory::StartCommand
                | SettingCategory::Backups => String::new(),
                _ => format!(" ({count})"),
            };
            println!("  {}) {}{suffix}", i + 1, category.label());
        }
        println!();
        let line = read_line(t(Msg::SettingsPromptCategory))?;
        if line.is_empty() || line.eq_ignore_ascii_case("q") {
            break;
        }
        let Some(category) = line
            .parse::<usize>()
            .ok()
            .and_then(|n| n.checked_sub(1))
            .and_then(|i| SettingCategory::ALL.get(i))
        else {
            eprintln!("asc: {}", t(Msg::AuthInvalidChoice));
            continue;
        };
        match category {
            SettingCategory::Quota => edit_quota(&file, &mut values, &config_dir, &mut changed)?,
            SettingCategory::StartCommand => {
                edit_start_command(&file, &mut values, &config_dir, &mut changed)?
            }
            SettingCategory::Backups => edit_backup_policy(&mut values, &config_dir, &mut changed)?,
            _ => {
                let defs: Vec<_> = file
                    .settings
                    .iter()
                    .filter(|d| d.kind.category() == *category)
                    .collect();
                edit_setting_defs(&defs, &mut values, &config_dir, &mut changed)?;
            }
        }
    }
    if changed {
        println!("{}", tf(Msg::SettingsRestartHint, id));
    }
    Ok(())
}

/// One category of package-defined settings: the numbered list → pick →
/// validate → save loop of the editor.
fn edit_setting_defs(
    defs: &[&asc_daemon::daemon::pkg::settings::SettingDef],
    values: &mut asc_daemon::daemon::pkg::settings::SettingValues,
    config_dir: &std::path::Path,
    changed: &mut bool,
) -> anyhow::Result<()> {
    use asc_daemon::daemon::pkg::settings::SettingKind;

    if defs.is_empty() {
        println!("{}", t(Msg::SettingsCategoryEmpty));
        return Ok(());
    }
    let key_w = defs.iter().map(|d| d.key.len()).max().unwrap_or(4);
    loop {
        println!();
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
                values.save(config_dir)?;
                *changed = true;
                println!("{}", tf2(Msg::SettingsSaved, &def.key, shown));
            }
            Err(err) => eprintln!("asc: {err:#}"),
        }
    }
    Ok(())
}

/// The quota category: max_cpu / max_ram / max_disk overrides on top of the
/// package `quota:` section. '-' resets a field to the package value.
fn edit_quota(
    file: &asc_daemon::daemon::pkg::settings::SettingsFile,
    values: &mut asc_daemon::daemon::pkg::settings::SettingValues,
    config_dir: &std::path::Path,
    changed: &mut bool,
) -> anyhow::Result<()> {
    use asc_daemon::daemon::pkg::settings::{QuotaSpec, SettingValues, parse_size};

    const FIELDS: [&str; 3] = ["max_cpu", "max_ram", "max_disk"];
    loop {
        let over = values.quota_override()?.unwrap_or(QuotaSpec {
            max_cpu: None,
            max_ram: None,
            max_disk: None,
        });
        let package = file.quota.as_ref();
        let effective = [
            over.max_cpu
                .map(|c| c.to_string())
                .or_else(|| package.and_then(|q| q.max_cpu).map(|c| c.to_string())),
            over.max_ram
                .clone()
                .or_else(|| package.and_then(|q| q.max_ram.clone())),
            over.max_disk
                .clone()
                .or_else(|| package.and_then(|q| q.max_disk.clone())),
        ];
        println!();
        for (i, field) in FIELDS.iter().enumerate() {
            println!(
                "  {}) {:<8} = {}",
                i + 1,
                field,
                effective[i].as_deref().unwrap_or("-")
            );
        }
        println!();
        let line = read_line(t(Msg::SettingsPromptSelect))?;
        if line.is_empty() || line.eq_ignore_ascii_case("q") {
            break;
        }
        let Some(index) = line
            .parse::<usize>()
            .ok()
            .and_then(|n| n.checked_sub(1))
            .filter(|i| *i < FIELDS.len())
        else {
            eprintln!("asc: {}", t(Msg::AuthInvalidChoice));
            continue;
        };
        let raw = read_line(&tf(Msg::SettingsValueOrReset, FIELDS[index]))?;
        if raw.is_empty() {
            continue;
        }
        let mut over = over;
        if raw == "-" {
            match index {
                0 => over.max_cpu = None,
                1 => over.max_ram = None,
                _ => over.max_disk = None,
            }
        } else {
            // Validate before storing: max_cpu is a positive core count,
            // sizes must parse like the quota section does.
            match index {
                0 => match raw.parse::<f64>() {
                    Ok(cores) if cores > 0.0 => over.max_cpu = Some(cores),
                    _ => {
                        eprintln!("asc: {}", t(Msg::ErrQuotaCpu));
                        continue;
                    }
                },
                _ => {
                    if let Err(err) = parse_size(&raw) {
                        eprintln!("asc: {err:#}");
                        continue;
                    }
                    if index == 1 {
                        over.max_ram = Some(raw.clone());
                    } else {
                        over.max_disk = Some(raw.clone());
                    }
                }
            }
        }
        // Store only the set fields; an all-default override disappears.
        let mut object = serde_json::Map::new();
        if let Some(cpu) = over.max_cpu {
            object.insert("max_cpu".into(), serde_json::json!(cpu));
        }
        if let Some(ram) = &over.max_ram {
            object.insert("max_ram".into(), serde_json::json!(ram));
        }
        if let Some(disk) = &over.max_disk {
            object.insert("max_disk".into(), serde_json::json!(disk));
        }
        if object.is_empty() {
            values.remove(SettingValues::QUOTA_KEY);
        } else {
            values.set(SettingValues::QUOTA_KEY, serde_json::Value::Object(object));
        }
        values.save(config_dir)?;
        *changed = true;
        if raw == "-" {
            println!("{}", tf(Msg::SettingsReset, FIELDS[index]));
        } else {
            println!("{}", tf2(Msg::SettingsSaved, FIELDS[index], &raw));
        }
    }
    Ok(())
}

/// The start_command category: an app-level override of the package's start
/// command ('${VAR}' references resolve from the settings env at apply time).
fn edit_start_command(
    file: &asc_daemon::daemon::pkg::settings::SettingsFile,
    values: &mut asc_daemon::daemon::pkg::settings::SettingValues,
    config_dir: &std::path::Path,
    changed: &mut bool,
) -> anyhow::Result<()> {
    use asc_daemon::daemon::pkg::settings::SettingValues;

    let effective = values
        .start_command_override()
        .or(file.start_command.as_deref())
        .unwrap_or("-");
    println!();
    println!("  start_command = {effective}");
    println!();
    let raw = read_line(&tf(Msg::SettingsValueOrReset, "start_command"))?;
    if raw.is_empty() {
        return Ok(());
    }
    if raw == "-" {
        values.remove(SettingValues::START_COMMAND_KEY);
        values.save(config_dir)?;
        *changed = true;
        println!("{}", tf(Msg::SettingsReset, "start_command"));
        return Ok(());
    }
    values.set(SettingValues::START_COMMAND_KEY, serde_json::json!(raw));
    values.save(config_dir)?;
    *changed = true;
    println!("{}", tf2(Msg::SettingsSaved, "start_command", &raw));
    Ok(())
}

/// The backups category (DMN-009): which configured storages to back up to
/// (multi-select — toggle numbers on and off), how many copies to keep per
/// storage, and a schedule the daemon's scheduler (DMN-012) enforces —
/// `daily@HH:MM` or a cron expression, validated on input here. Stored under
/// the `$backup` reserved key, same convention as quota/start_command; an
/// all-default policy is removed rather than stored empty.
fn edit_backup_policy(
    values: &mut asc_daemon::daemon::pkg::settings::SettingValues,
    config_dir: &std::path::Path,
    changed: &mut bool,
) -> anyhow::Result<()> {
    use asc_daemon::daemon::backup::storage::StorageList;
    use asc_daemon::daemon::pkg::settings::SettingValues;

    let storages = StorageList::load()?.names();
    const FIELDS: [&str; 3] = ["storages", "keep", "schedule"];
    loop {
        let mut policy = values.backup_policy()?.unwrap_or_default();
        println!();
        for (i, field) in FIELDS.iter().enumerate() {
            let shown = match *field {
                "storages" if policy.storages.is_empty() => "-".to_string(),
                "storages" => policy.storages.join(", "),
                "keep" => policy
                    .keep
                    .map(|k| k.to_string())
                    .unwrap_or_else(|| "-".into()),
                _ => policy.schedule.clone().unwrap_or_else(|| "-".into()),
            };
            println!("  {}) {:<9} = {}", i + 1, field, shown);
        }
        println!();
        let line = read_line(t(Msg::SettingsPromptSelect))?;
        if line.is_empty() || line.eq_ignore_ascii_case("q") {
            break;
        }
        let Some(index) = line
            .parse::<usize>()
            .ok()
            .and_then(|n| n.checked_sub(1))
            .filter(|i| *i < FIELDS.len())
        else {
            eprintln!("asc: {}", t(Msg::AuthInvalidChoice));
            continue;
        };
        match index {
            0 => {
                println!();
                for (i, name) in storages.iter().enumerate() {
                    let mark = if policy.storages.iter().any(|s| s == name) {
                        "x"
                    } else {
                        " "
                    };
                    println!("  {}) [{mark}] {name}", i + 1);
                }
                let raw = read_line(t(Msg::BackupPolicyStorageToggle))?;
                if raw.is_empty() {
                    continue;
                }
                for token in raw.split([',', ' ']).filter(|t| !t.is_empty()) {
                    let Some(n) = token
                        .parse::<usize>()
                        .ok()
                        .filter(|n| (1..=storages.len()).contains(n))
                    else {
                        eprintln!("asc: {}", t(Msg::AuthInvalidChoice));
                        continue;
                    };
                    let name = &storages[n - 1];
                    match policy.storages.iter().position(|s| s == name) {
                        Some(pos) => {
                            policy.storages.remove(pos);
                        }
                        None => policy.storages.push(name.clone()),
                    }
                }
            }
            1 => {
                let raw = read_line(t(Msg::BackupPolicyKeepPrompt))?;
                if raw.is_empty() {
                    continue;
                }
                if raw == "-" {
                    policy.keep = None;
                } else {
                    match raw.parse::<u32>() {
                        Ok(n) if n > 0 => policy.keep = Some(n),
                        _ => {
                            eprintln!("asc: keep must be a positive integer");
                            continue;
                        }
                    }
                }
            }
            _ => {
                let raw = read_line(t(Msg::BackupPolicySchedulePrompt))?;
                if raw.is_empty() {
                    continue;
                }
                if raw == "-" {
                    policy.schedule = None;
                } else {
                    // The scheduler will run this — reject what it cannot parse.
                    if let Err(err) = asc_daemon::daemon::scheduler::Schedule::parse(&raw) {
                        eprintln!("asc: {err}");
                        continue;
                    }
                    policy.schedule = Some(raw);
                }
            }
        }
        if policy.is_empty() {
            values.remove(SettingValues::BACKUP_KEY);
        } else {
            values.set(SettingValues::BACKUP_KEY, serde_json::to_value(&policy)?);
        }
        values.save(config_dir)?;
        *changed = true;
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
    let Runtime::Docker { container, .. } = &status.meta.runtime else {
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

/// `asc stats` — resource usage per app, like `docker stats --no-stream`;
/// with `--live`, keeps refreshing in place instead of exiting after one
/// sample, like plain `docker stats`. Root gets everyone's apps grouped by
/// owner. Each sample already costs ~500ms (CPU is a delta over a wall-clock
/// interval, [`AppManager::stats`] sleeps for it) — that doubles as the live
/// refresh cadence, no extra sleep needed.
fn stats_cmd(sort: StatsSort, live: bool, config: &Config) -> anyhow::Result<()> {
    let manager = AppManager::new(config);
    let ctx = UserContext::current();
    loop {
        let mut stats = manager.stats(&ctx)?;
        if live {
            // Clear the screen and home the cursor before each redraw, like
            // `docker stats`/`top`.
            print!("\x1b[2J\x1b[H");
        }
        if stats.is_empty() {
            println!("{}", t(Msg::AppListEmpty));
        } else {
            print_stats_table(&mut stats, sort, ctx.is_root);
        }
        if !live {
            return Ok(());
        }
    }
}

/// "rx / tx" (or "read / write"), like `docker stats`'s NET I/O / BLOCK I/O
/// columns — cumulative since the app started, not a rate. A dash when the
/// runtime cannot report the pair at all (e.g. network I/O for a systemd or
/// process app, which has no network namespace of its own).
fn format_io_pair(a: Option<u64>, b: Option<u64>) -> String {
    match (a, b) {
        (Some(a), Some(b)) => format!("{} / {}", monitor::human_bytes(a), monitor::human_bytes(b)),
        _ => "-".to_string(),
    }
}

/// One `asc stats` render: sorts (highest consumer first, stopped apps
/// last) and prints, grouped by owner for root.
fn print_stats_table(stats: &mut [AppStats], sort: StatsSort, group_by_owner: bool) {
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
            "{:<id_w$}  {:<7}  {:>7}  {:>10}  {:<12}  {:>10}  {:<21}  DISK I/O",
            "ID", "KIND", "CPU %", "MEM", "QUOTA", "DISK", "NET I/O"
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
            let quota = match s.quota_disk_bytes {
                Some(quota) => static_bar(s.disk_bytes, quota, 10),
                None => "-".to_string(),
            };
            println!(
                "{:<id_w$}  {:<7}  {:>7}  {:>10}  {:<12}  {:>10}  {:<21}  {}",
                s.meta.id,
                s.meta.runtime.kind(),
                cpu,
                mem,
                quota,
                monitor::human_bytes(s.disk_bytes),
                format_io_pair(s.net_rx_bytes, s.net_tx_bytes),
                format_io_pair(s.disk_read_bytes, s.disk_write_bytes),
            );
        }
    };
    if group_by_owner {
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
}

fn state_label(state: RuntimeState) -> &'static str {
    match state {
        RuntimeState::Running => t(Msg::StateActive),
        RuntimeState::Stopped => t(Msg::StateInactive),
    }
}

/// The API's "running"/"stopped" strings, translated like [`state_label`].
fn remote_state_label(state: &str) -> &'static str {
    if state == "running" {
        t(Msg::StateActive)
    } else {
        t(Msg::StateInactive)
    }
}

/// Whether ANSI colors should be written: only when stdout is a real
/// terminal and the user has not opted out via `NO_COLOR` (no-color.org).
fn color_enabled() -> bool {
    use std::io::IsTerminal;
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

/// `label` padded to `width` first, *then* wrapped in a color escape — doing
/// it in that order keeps the escape bytes out of the padding, so later
/// `{:<n}` columns stay aligned in the terminal.
fn colored_state(label: &str, is_running: bool, width: usize) -> String {
    let padded = format!("{label:<width$}");
    if !color_enabled() {
        return padded;
    }
    let code = if is_running { "32" } else { "31" }; // green / red
    format!("\x1b[{code}m{padded}\x1b[0m")
}

/// One `asc app list` table row — the common shape of an app whether it
/// came from the local store or from the daemon API.
struct AppRow {
    id: String,
    uuid: Option<String>,
    name: String,
    kind: String,
    state: &'static str,
    is_running: bool,
    version: Option<String>,
    owner: String,
}

fn local_row(app: &AppStatus) -> AppRow {
    AppRow {
        id: app.meta.id.clone(),
        uuid: app.meta.uuid.clone(),
        name: app.meta.display_name().to_string(),
        kind: app.meta.runtime.kind().to_string(),
        state: state_label(app.state),
        is_running: app.state == RuntimeState::Running,
        version: app.meta.version.clone(),
        owner: app.meta.owner.name.clone(),
    }
}

fn remote_row(app: &RemoteApp) -> AppRow {
    AppRow {
        id: app.id.clone(),
        uuid: app.uuid.clone(),
        name: app.name.clone(),
        kind: app.kind.clone(),
        state: remote_state_label(&app.state),
        is_running: app.state == "running",
        version: app.version.clone(),
        owner: app.owner.clone(),
    }
}

/// Table of apps; root also gets a USER column (their apps span every
/// account, unlike a regular user who only ever sees their own). Column
/// headers are technical identifiers and stay English by convention (like
/// `docker ps`). NAME is the user's custom name (or the package title): both
/// it and the ID address the app in commands. STATE is colored (green =
/// running, red = stopped) when stdout is a terminal. `indent` prefixes every
/// printed line — used by `asc stacks` to nest a stack's apps under its name;
/// pass "" for a flat, unnested list.
fn print_app_list(apps: &[AppRow], show_user: bool, indent: &str) {
    print_app_list_inner(apps, show_user, indent, false);
}

/// Same table, but each row gets a tree connector (`├── `/`└── `) instead of
/// a fixed indent — used by `asc stacks` to draw its apps as a branch under
/// the stack name. `header_indent` must be as wide as a connector (4 cols)
/// so the column header still lines up with the rows.
fn print_app_tree(apps: &[AppRow], show_user: bool, header_indent: &str) {
    print_app_list_inner(apps, show_user, header_indent, true);
}

fn print_app_list_inner(apps: &[AppRow], show_user: bool, indent: &str, tree: bool) {
    if apps.is_empty() {
        println!("{indent}{}", t(Msg::AppListEmpty));
        return;
    }
    let row_prefix = |i: usize| -> &str {
        if !tree {
            indent
        } else if i + 1 == apps.len() {
            "└── "
        } else {
            "├── "
        }
    };
    let id_w = apps.iter().map(|a| a.id.len()).max().unwrap_or(2).max(2);
    let name_w = apps
        .iter()
        .map(|a| a.name.chars().count())
        .max()
        .unwrap_or(4)
        .max(4);
    let state_w = 10;
    // VERSION is no longer the trailing column (UUID is), so it needs a width.
    let version_w = apps
        .iter()
        .map(|a| a.version.as_deref().unwrap_or("-").chars().count())
        .max()
        .unwrap_or(7)
        .max(7);
    if show_user {
        let user_w = apps
            .iter()
            .map(|a| a.owner.chars().count())
            .max()
            .unwrap_or(4)
            .max(4);
        println!(
            "{indent}{:<id_w$}  {:<name_w$}  {:<7}  {:<state_w$}  {:<user_w$}  {:<version_w$}  UUID",
            "ID", "NAME", "KIND", "STATE", "USER", "VERSION"
        );
        for (i, app) in apps.iter().enumerate() {
            println!(
                "{}{:<id_w$}  {:<name_w$}  {:<7}  {}  {:<user_w$}  {:<version_w$}  {}",
                row_prefix(i),
                app.id,
                app.name,
                app.kind,
                colored_state(app.state, app.is_running, state_w),
                app.owner,
                app.version.as_deref().unwrap_or("-"),
                app.uuid.as_deref().unwrap_or("-"),
            );
        }
    } else {
        println!(
            "{indent}{:<id_w$}  {:<name_w$}  {:<7}  {:<state_w$}  {:<version_w$}  UUID",
            "ID", "NAME", "KIND", "STATE", "VERSION"
        );
        for (i, app) in apps.iter().enumerate() {
            println!(
                "{}{:<id_w$}  {:<name_w$}  {:<7}  {}  {:<version_w$}  {}",
                row_prefix(i),
                app.id,
                app.name,
                app.kind,
                colored_state(app.state, app.is_running, state_w),
                app.version.as_deref().unwrap_or("-"),
                app.uuid.as_deref().unwrap_or("-"),
            );
        }
    }
}

/// `asc stacks`: installed apps grouped by the stack (`asc.stack.yaml`
/// package) they came from — a tree per stack (`├──`/`└──` branches over its
/// apps, sorted by id), the stack name annotated with how many of its apps
/// are running (root sees all users' apps, like [`print_app_list`]). An
/// app's stack is read from `meta.package` (DMN-003: recorded as
/// `"<stack>/<app>"` at install); an app installed on its own has no `/` in
/// `package` and is not part of any stack, so it never appears here.
fn stacks_cmd(config: &Config) -> anyhow::Result<()> {
    let manager = AppManager::new(config);
    let ctx = UserContext::current();

    let mut stacks: std::collections::BTreeMap<String, Vec<AppStatus>> =
        std::collections::BTreeMap::new();
    for app in manager.list(&ctx)? {
        if let Some((stack, _)) = app.meta.package.as_deref().and_then(|p| p.split_once('/')) {
            stacks.entry(stack.to_string()).or_default().push(app);
        }
    }

    if stacks.is_empty() {
        println!("{}", t(Msg::StacksEmpty));
        return Ok(());
    }

    let show_user = ctx.is_root;
    for (i, (stack, mut apps)) in stacks.into_iter().enumerate() {
        if i > 0 {
            println!();
        }
        apps.sort_by(|a, b| a.meta.id.cmp(&b.meta.id));
        let running = apps
            .iter()
            .filter(|a| a.state == RuntimeState::Running)
            .count();
        let rows: Vec<AppRow> = apps.iter().map(local_row).collect();
        println!("{stack} [{running}/{}]", apps.len());
        print_app_tree(&rows, show_user, "    ");
    }
    Ok(())
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
    // Through the daemon when it answers (the counts then reflect what the
    // caller may see per SO_PEERCRED); silently in-process otherwise —
    // status is diagnostics and must work with a broken daemon too.
    let (running, total) = match client::Daemon::connect(config).ok().flatten() {
        Some(daemon) => {
            let (_, running, total) = daemon.status()?;
            (running as usize, total as usize)
        }
        None => {
            let manager = AppManager::new(config);
            let apps = manager.list(&UserContext::current())?;
            let running = apps
                .iter()
                .filter(|a| a.state == RuntimeState::Running)
                .count();
            (running, apps.len())
        }
    };
    println!("{}", tf2(Msg::StatusApps, running, total));
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
        ConfigAction::Debug { state: None } => {
            let current = if config.log.level == "debug" {
                OnOff::On
            } else {
                OnOff::Off
            };
            println!("{}", tf(Msg::ConfigDebugCurrent, current));
        }
        ConfigAction::Debug { state: Some(state) } => {
            config.log.level = match state {
                OnOff::On => "debug".to_string(),
                OnOff::Off => "info".to_string(),
            };
            config.save()?;
            println!("{}", tf(Msg::ConfigDebugSet, state));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{port_label, resource_shortages};
    use asc_daemon::daemon::docker::PortProtocol;
    use asc_daemon::daemon::monitor::system::{
        CpuMetrics, DiskMetrics, MemoryMetrics, SystemMetrics,
    };

    #[test]
    fn port_label_renders_transport_docker_style() {
        assert_eq!(port_label(27015, PortProtocol::Tcp), "27015/tcp");
        assert_eq!(port_label(27015, PortProtocol::Udp), "27015/udp");
        // `both` shares the host==container port across transports.
        assert_eq!(port_label(27015, PortProtocol::Both), "27015/tcp+udp");
    }

    #[test]
    fn shortages_cover_ram_cpu_and_the_apps_filesystem() {
        let req: asc_daemon::daemon::pkg::manifest::Requirements =
            serde_yaml::from_str("{ ram: 4G, disk: 80G, cpu: 4 }").unwrap();
        let metrics = SystemMetrics {
            timestamp: 0,
            cpu: CpuMetrics {
                usage_percent: None,
                cores: 2,
                load1: 0.0,
                load5: 0.0,
                load15: 0.0,
            },
            memory: MemoryMetrics {
                total: 8 << 30,
                used: 7 << 30,
                available: 1 << 30,
                swap_total: 0,
                swap_used: 0,
            },
            disks: vec![
                DiskMetrics {
                    mount: "/".into(),
                    filesystem: "ext4".into(),
                    total: 100 << 30,
                    used: 50 << 30,
                    available: 50 << 30,
                },
                DiskMetrics {
                    mount: "/asc".into(),
                    filesystem: "ext4".into(),
                    total: 200 << 30,
                    used: 100 << 30,
                    available: 100 << 30,
                },
            ],
            network: vec![],
            uptime_secs: 0,
        };
        // RAM 4G > 1G free and CPU 4 > 2 cores are short; the disk check
        // uses the longest matching mount (/asc, 100G free) — enough for 80G.
        let short = resource_shortages(&req, &metrics, std::path::Path::new("/asc/apps/cs2"));
        assert_eq!(short.len(), 2, "got: {short:?}");
        assert!(short[0].starts_with("RAM"), "got: {short:?}");
        assert!(short[1].starts_with("CPU"), "got: {short:?}");

        // The same app on the root filesystem (50G free) is short on disk.
        let short = resource_shortages(&req, &metrics, std::path::Path::new("/opt/apps/cs2"));
        assert!(
            short.iter().any(|s| s.starts_with("disk")),
            "got: {short:?}"
        );
    }
}
