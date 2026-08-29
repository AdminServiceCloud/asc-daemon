//! `asc-updater` — installer and update manager for the asc daemon (DMN-014).
//!
//! Deliberately a separate binary: it can replace and restart a broken
//! daemon without depending on it. `install.sh` bootstraps this updater,
//! which then downloads and installs the daemon itself.

mod github;
mod installer;

use std::io::{BufRead, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde::Serialize;

use asc_daemon::daemon::config::{Channel, Config, TlsMode};
use asc_daemon::daemon::i18n::{self, Lang, Msg, t, tf, tf2};
use asc_daemon::daemon::{logging, platform, service};

#[derive(Parser)]
#[command(
    name = "asc-updater",
    version,
    about = "AdminService.Cloud updater — installs and updates the asc daemon"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Install the daemon (interactive by default)
    Install {
        /// No questions: install everything with default settings
        #[arg(long)]
        silent: bool,
        /// One-time registration token from the platform: binds this node to
        /// the organization that issued it
        #[arg(long, value_name = "TOKEN")]
        token: Option<String>,
        /// Platform base URL (default https://adminservice.cloud)
        #[arg(long, value_name = "URL")]
        url: Option<String>,
        /// Expose the daemon API to the network over TLS instead of keeping
        /// it on loopback behind the platform's SSH tunnel
        #[arg(long)]
        direct: bool,
    },
    /// Update the daemon to the channel's latest release
    Update {
        /// Do not wait for active daemon tasks to finish
        #[arg(long)]
        force: bool,
        /// Emit newline-delimited JSON progress events
        #[arg(long)]
        json: bool,
    },
    /// Manage automatic updates (systemd timer)
    Auto {
        #[command(subcommand)]
        action: AutoAction,
    },
    /// Switch the update channel
    Channel { channel: Channel },
    /// Roll back to the previously installed version
    Rollback,
    /// Show installed and available versions
    Status {
        /// Emit a single machine-readable JSON object
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum AutoAction {
    Enable,
    Disable,
    Status,
}

fn main() {
    let cli = Cli::parse();
    let machine_update = matches!(&cli.command, Command::Update { json: true, .. });
    if let Err(err) = run(cli) {
        if machine_update {
            emit_update_event(installer::UpdateEvent::error(safe_machine_message(
                &format!("{err:#}"),
            )));
        } else {
            eprintln!("asc-updater: {err:#}");
        }
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    // Show warnings on stderr; RUST_LOG can raise verbosity.
    logging::init("warn");
    let mut config = Config::load()?;
    i18n::set_lang(config.language);

    if !matches!(
        &cli.command,
        Command::Status { .. }
            | Command::Auto {
                action: AutoAction::Status
            }
    ) {
        service::require_root()?;
    }

    match cli.command {
        Command::Install {
            silent,
            token,
            url,
            direct,
        } => {
            if !silent && !confirm_settings(&mut config)? {
                bail!(t(Msg::UpdAborted));
            }
            i18n::set_lang(config.language);
            if direct {
                enable_direct_api(&mut config)?;
            }
            installer::install(&config)?;
            // Registration is deliberately last and never fatal: the daemon is
            // installed and useful locally even when the platform is
            // unreachable, and `asc connect` retries.
            if let Some(token) = token {
                connect(&mut config, &token, url.as_deref());
            }
            Ok(())
        }
        Command::Update { force, json } => {
            if json {
                installer::update_with_progress(&config, force, emit_update_event)
            } else {
                installer::update(&config, force)
            }
        }
        Command::Auto { action } => auto_cmd(action, config),
        Command::Channel { channel } => {
            config.updater.channel = channel;
            config.save()?;
            println!("{}", tf(Msg::UpdChannelSet, channel));
            Ok(())
        }
        Command::Rollback => installer::rollback(&config),
        Command::Status { json } => status_cmd(&config, json),
    }
}

/// Switches the API from loopback to the network, with TLS. Without TLS the
/// bearer token — which grants full control of the machine — would cross the
/// network in the clear, so the two are turned on together and never apart.
fn enable_direct_api(config: &mut Config) -> Result<()> {
    let port = config
        .api
        .listen
        .rsplit(':')
        .next()
        .unwrap_or("8420")
        .to_string();
    config.api.listen = format!("0.0.0.0:{port}");
    config.api.tls = TlsMode::SelfSigned;
    config.save().context("cannot save config.toml")?;
    println!("{}", t(Msg::PlatformDirectEnabled));
    Ok(())
}

/// Bind the freshly installed node to a platform, reporting but swallowing
/// failures.
fn connect(config: &mut Config, token: &str, url: Option<&str>) {
    match platform::register(config, token, url) {
        Ok(registration) => println!(
            "{}",
            tf2(
                Msg::PlatformRegistered,
                &registration.platform_url,
                &registration.node_id
            )
        ),
        Err(err) => {
            eprintln!("{}", tf(Msg::PlatformRegisterFailed, format!("{err:#}")));
            eprintln!("{}", t(Msg::PlatformRetryHint));
        }
    }
}

fn auto_cmd(action: AutoAction, mut config: Config) -> Result<()> {
    match action {
        AutoAction::Enable => {
            config.updater.enabled = true;
            config.save()?;
            installer::setup_timer(&config)?;
            println!("{}", t(Msg::UpdAutoEnabled));
        }
        AutoAction::Disable => {
            config.updater.enabled = false;
            config.save()?;
            installer::disable_timer()?;
            println!("{}", t(Msg::UpdAutoDisabled));
        }
        AutoAction::Status => print_settings(&config),
    }
    Ok(())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusOutput {
    updater_version: &'static str,
    installed_version: String,
    available_version: String,
    channel: String,
    update_available: bool,
}

fn status_cmd(config: &Config, json: bool) -> Result<()> {
    if json {
        let installed_version = installer::installed_version(config)
            .context("daemon is not installed or its version cannot be determined")?;
        let available_version = github::latest_release(config.updater.channel)
            .context("cannot check the available daemon version")?
            .tag_name;
        let update_available =
            installer::is_strictly_newer(&installed_version, &available_version)?;
        let output = StatusOutput {
            updater_version: asc_daemon::VERSION,
            installed_version,
            available_version,
            channel: config.updater.channel.to_string(),
            update_available,
        };
        println!("{}", serde_json::to_string(&output)?);
        return Ok(());
    }

    let installed = installer::installed_version(config);
    let available = github::latest_release(config.updater.channel);
    println!("asc-updater {}", asc_daemon::VERSION);
    match installed {
        Some(version) => println!("{}", tf(Msg::UpdStatusInstalled, version)),
        None => println!("{}", t(Msg::UpdNotInstalled)),
    }
    match available {
        Ok(release) => println!(
            "{}",
            tf2(
                Msg::UpdStatusAvailable,
                config.updater.channel,
                release.tag_name
            )
        ),
        Err(err) => eprintln!("asc-updater: {err:#}"),
    }
    print_settings(config);
    Ok(())
}

fn emit_update_event(event: installer::UpdateEvent) {
    // Serialization of this fixed structure is infallible. Flush every line
    // so an SSH caller can stream progress without waiting for process exit.
    println!(
        "{}",
        serde_json::to_string(&event).expect("update event is serializable")
    );
    std::io::stdout().flush().ok();
}

fn safe_machine_message(message: &str) -> String {
    let compact = message.split_whitespace().collect::<Vec<_>>().join(" ");
    let truncated = compact.chars().take(512).collect::<String>();
    if truncated.is_empty() {
        "daemon update failed".to_string()
    } else {
        truncated
    }
}

fn print_settings(config: &Config) {
    let auto = if config.updater.enabled {
        t(Msg::WordEnabled)
    } else {
        t(Msg::WordDisabled)
    };
    println!("{}", tf(Msg::UpdSettingLanguage, config.language));
    println!("{}", tf(Msg::UpdSettingAuto, auto));
    println!("{}", tf(Msg::UpdSettingChannel, config.updater.channel));
    println!("{}", tf(Msg::UpdSettingSchedule, &config.updater.schedule));
    println!(
        "{}",
        tf(Msg::UpdSettingDir, config.updater.install_dir.display())
    );
}

/// Interactive install dialog: show defaults, then Y (accept) / n (abort) /
/// c (adjust each setting). Returns false when the user aborts.
fn confirm_settings(config: &mut Config) -> Result<bool> {
    println!();
    println!("{}", t(Msg::UpdSettingsHeader));
    println!();
    print_settings(config);
    println!();
    let answer = ask(t(Msg::UpdConfirmDefaults), "Y")?.to_lowercase();
    match answer.as_str() {
        "" | "y" | "yes" | "д" | "да" => Ok(true),
        "c" | "change" | "и" | "изменить" => {
            adjust_settings(config)?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn adjust_settings(config: &mut Config) -> Result<()> {
    let lang = ask(t(Msg::UpdPromptLanguage), &config.language.to_string())?;
    config.language = lang.parse::<Lang>().map_err(anyhow::Error::msg)?;
    i18n::set_lang(config.language);

    let auto = ask(
        t(Msg::UpdPromptAuto),
        if config.updater.enabled { "on" } else { "off" },
    )?;
    config.updater.enabled = match auto.to_lowercase().as_str() {
        "on" | "yes" | "y" | "вкл" => true,
        "off" | "no" | "n" | "выкл" => false,
        other => bail!("unknown value '{other}', expected on/off"),
    };

    let channel = ask(
        t(Msg::UpdPromptChannel),
        &config.updater.channel.to_string(),
    )?;
    config.updater.channel = channel.parse::<Channel>().map_err(anyhow::Error::msg)?;

    let schedule = ask(t(Msg::UpdPromptSchedule), &config.updater.schedule)?;
    validate_schedule(&schedule)?;
    config.updater.schedule = schedule;

    let dir = ask(
        t(Msg::UpdPromptDir),
        &config.updater.install_dir.display().to_string(),
    )?;
    config.updater.install_dir = PathBuf::from(dir);
    Ok(())
}

/// `HH:MM` for the systemd OnCalendar expression.
fn validate_schedule(schedule: &str) -> Result<()> {
    let ok = matches!(
        schedule.split(':').collect::<Vec<_>>().as_slice(),
        [h, m] if h.len() == 2
            && m.len() == 2
            && h.parse::<u8>().is_ok_and(|h| h < 24)
            && m.parse::<u8>().is_ok_and(|m| m < 60)
    );
    if !ok {
        bail!("invalid schedule '{schedule}': expected HH:MM (e.g. 04:00)");
    }
    Ok(())
}

/// Ask a question with a default, reading the answer from the controlling
/// terminal. `install.sh` pipes the script into bash, so stdin is not the
/// terminal — `/dev/tty` is.
fn ask(question: &str, default: &str) -> Result<String> {
    print!("{question} [{default}]: ");
    std::io::stdout().flush().ok();
    let line = read_tty_line()?;
    let trimmed = line.trim();
    Ok(if trimmed.is_empty() {
        default.to_string()
    } else {
        trimmed.to_string()
    })
}

fn read_tty_line() -> Result<String> {
    let mut line = String::new();
    if let Ok(tty) = std::fs::File::open("/dev/tty") {
        std::io::BufReader::new(tty)
            .read_line(&mut line)
            .context("cannot read from terminal")?;
        return Ok(line);
    }
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .context("cannot read from stdin")?;
    Ok(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_validation() {
        validate_schedule("04:00").unwrap();
        validate_schedule("23:59").unwrap();
        assert!(validate_schedule("4:00").is_err());
        assert!(validate_schedule("24:00").is_err());
        assert!(validate_schedule("04:60").is_err());
        assert!(validate_schedule("nope").is_err());
    }

    #[test]
    fn json_flags_parse_on_status_and_update() {
        let status = Cli::try_parse_from(["asc-updater", "status", "--json"]).unwrap();
        assert!(matches!(status.command, Command::Status { json: true }));

        let update = Cli::try_parse_from(["asc-updater", "update", "--json", "--force"]).unwrap();
        assert!(matches!(
            update.command,
            Command::Update {
                force: true,
                json: true
            }
        ));
    }

    #[test]
    fn status_json_uses_stable_camel_case_fields() {
        let output = StatusOutput {
            updater_version: "0.15.0",
            installed_version: "0.14.0".into(),
            available_version: "v0.15.0".into(),
            channel: "stable".into(),
            update_available: true,
        };
        let json = serde_json::to_value(output).unwrap();
        assert_eq!(json["updaterVersion"], "0.15.0");
        assert_eq!(json["installedVersion"], "0.14.0");
        assert_eq!(json["availableVersion"], "v0.15.0");
        assert_eq!(json["channel"], "stable");
        assert_eq!(json["updateAvailable"], true);
    }

    #[test]
    fn machine_error_messages_are_single_line_and_bounded() {
        let message = format!("  failure\nwith\tspaces {}", "x".repeat(600));
        let safe = safe_machine_message(&message);
        assert!(!safe.contains('\n'));
        assert!(!safe.contains('\t'));
        assert_eq!(safe.chars().count(), 512);
    }
}
