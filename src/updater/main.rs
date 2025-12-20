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

use asc_daemon::daemon::config::{Channel, Config};
use asc_daemon::daemon::i18n::{self, Lang, Msg, t, tf, tf2};
use asc_daemon::daemon::{logging, service};

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
    },
    /// Update the daemon to the channel's latest release
    Update {
        /// Do not wait for active daemon tasks to finish
        #[arg(long)]
        force: bool,
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
    Status,
}

#[derive(Subcommand)]
enum AutoAction {
    Enable,
    Disable,
    Status,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("asc-updater: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    // Show warnings on stderr; RUST_LOG can raise verbosity.
    logging::init("warn");
    let mut config = Config::load()?;
    i18n::set_lang(config.language);

    if !matches!(
        cli.command,
        Command::Status
            | Command::Auto {
                action: AutoAction::Status
            }
    ) {
        service::require_root()?;
    }

    match cli.command {
        Command::Install { silent } => {
            if !silent && !confirm_settings(&mut config)? {
                bail!(t(Msg::UpdAborted));
            }
            i18n::set_lang(config.language);
            installer::install(&config)
        }
        Command::Update { force } => installer::update(&config, force),
        Command::Auto { action } => auto_cmd(action, config),
        Command::Channel { channel } => {
            config.updater.channel = channel;
            config.save()?;
            println!("{}", tf(Msg::UpdChannelSet, channel));
            Ok(())
        }
        Command::Rollback => installer::rollback(&config),
        Command::Status => status_cmd(&config),
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

fn status_cmd(config: &Config) -> Result<()> {
    println!("asc-updater {}", asc_daemon::VERSION);
    match installer::installed_version(config) {
        Some(version) => println!("{}", tf(Msg::UpdStatusInstalled, version)),
        None => println!("{}", t(Msg::UpdNotInstalled)),
    }
    match github::latest_release(config.updater.channel) {
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
}
