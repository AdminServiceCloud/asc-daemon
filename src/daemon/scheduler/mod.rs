//! In-daemon task scheduler (DMN-012, first increment).
//!
//! A cron-like evaluator that wakes up once a minute and runs whatever is
//! due. The first (and so far only) consumer is scheduled app backups
//! (DMN-009): every app whose backup policy (`asc app settings`, the
//! `backups` category) has a `schedule` gets `create_backup` runs to its
//! configured storages, with the policy's `keep` rotation applied.
//!
//! Schedule syntax (see [`Schedule::parse`]): `daily@HH:MM`, bare `HH:MM`
//! (same as daily) or a five-field cron expression
//! `minute hour day-of-month month day-of-week`. Times are the node's local
//! time, like cron. The persistent task queue, priorities and the `asc task`
//! CLI from the DMN-012 design are the next increment.

use anyhow::{Context, Result, bail};
use tracing::{info, warn};

use crate::daemon::apps::AppStore;
use crate::daemon::backup;
use crate::daemon::backup::storage::{self, StorageList};
use crate::daemon::config::Config;
use crate::daemon::i18n::{Msg, tf};
use crate::daemon::pkg::settings::SettingValues;

/// One local-time instant, broken down to the fields cron matches against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Moment {
    /// 0–59.
    pub minute: u32,
    /// 0–23.
    pub hour: u32,
    /// 1–31.
    pub day: u32,
    /// 1–12.
    pub month: u32,
    /// 0–6, Sunday = 0 (like cron).
    pub weekday: u32,
}

impl Moment {
    /// The node's local time for a unix timestamp.
    pub fn local(epoch_secs: i64) -> Self {
        // time_t is 32-bit on armv7 (y2038 applies there, as it does to the
        // whole platform) — go through libc's alias, not a fixed width.
        let epoch: libc::time_t = epoch_secs as libc::time_t;
        // SAFETY: localtime_r fills the caller's buffer and touches no shared
        // state; a zeroed tm is a valid out-parameter.
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        unsafe { libc::localtime_r(&epoch, &mut tm) };
        Self {
            minute: tm.tm_min as u32,
            hour: tm.tm_hour as u32,
            day: tm.tm_mday as u32,
            month: (tm.tm_mon + 1) as u32,
            weekday: tm.tm_wday as u32,
        }
    }

    fn now() -> Self {
        Self::local(unix_now())
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// A parsed schedule: one bitmask per cron field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Schedule {
    minute: u64,
    hour: u64,
    day: u64,
    month: u64,
    weekday: u64,
    /// Whether day-of-month / day-of-week were restricted (not `*`): like
    /// vixie cron, when **both** are restricted a date matching **either**
    /// fires the job.
    day_restricted: bool,
    weekday_restricted: bool,
}

impl Schedule {
    /// Parse a schedule string. Accepted forms:
    /// - `daily@HH:MM` or bare `HH:MM` — every day at that local time;
    /// - `minute hour day-of-month month day-of-week` — cron, with `*`,
    ///   values, `a-b` ranges, `a,b,c` lists and `/n` steps; day-of-week is
    ///   0–7 where both 0 and 7 are Sunday.
    pub fn parse(raw: &str) -> Result<Self> {
        let s = raw.trim();
        let invalid = || anyhow::anyhow!(tf(Msg::ErrBackupSchedule, raw));
        if s.is_empty() {
            return Err(invalid());
        }
        // daily@HH:MM / HH:MM sugar.
        let time_part = match s.split_once('@') {
            Some((prefix, time)) if prefix.eq_ignore_ascii_case("daily") => Some(time),
            Some(_) => return Err(invalid()),
            None if s.contains(':') => Some(s),
            None => None,
        };
        if let Some(time) = time_part {
            let (hh, mm) = time.split_once(':').ok_or_else(invalid)?;
            let hour: u32 = hh.parse().map_err(|_| invalid())?;
            let minute: u32 = mm.parse().map_err(|_| invalid())?;
            if hh.len() != 2 || mm.len() != 2 || hour > 23 || minute > 59 {
                return Err(invalid());
            }
            return Ok(Self {
                minute: 1 << minute,
                hour: 1 << hour,
                day: mask_all(1, 31),
                month: mask_all(1, 12),
                weekday: mask_all(0, 6),
                day_restricted: false,
                weekday_restricted: false,
            });
        }
        // Five-field cron.
        let fields: Vec<&str> = s.split_whitespace().collect();
        let [minute, hour, day, month, weekday] = fields.as_slice() else {
            return Err(invalid());
        };
        let parse = |field: &str, min: u32, max: u32| -> Result<u64> {
            parse_field(field, min, max).map_err(|_| invalid())
        };
        let mut weekday_mask = parse(weekday, 0, 7)?;
        // 7 is Sunday too; fold it onto 0.
        if weekday_mask & (1 << 7) != 0 {
            weekday_mask = (weekday_mask & !(1 << 7)) | 1;
        }
        Ok(Self {
            minute: parse(minute, 0, 59)?,
            hour: parse(hour, 0, 23)?,
            day: parse(day, 1, 31)?,
            month: parse(month, 1, 12)?,
            weekday: weekday_mask,
            day_restricted: *day != "*",
            weekday_restricted: *weekday != "*",
        })
    }

    /// Whether the schedule fires at this instant (minute resolution).
    pub fn matches(&self, at: Moment) -> bool {
        let bit = |mask: u64, value: u32| mask & (1 << value) != 0;
        if !bit(self.minute, at.minute) || !bit(self.hour, at.hour) || !bit(self.month, at.month) {
            return false;
        }
        let day_ok = bit(self.day, at.day);
        let weekday_ok = bit(self.weekday, at.weekday);
        // Vixie-cron rule: both fields restricted → either one may match.
        if self.day_restricted && self.weekday_restricted {
            day_ok || weekday_ok
        } else {
            day_ok && weekday_ok
        }
    }
}

/// Bitmask with bits `min..=max` set.
fn mask_all(min: u32, max: u32) -> u64 {
    (min..=max).fold(0, |mask, v| mask | 1 << v)
}

/// One cron field (`*`, `*/15`, `5`, `1-5`, `1,3,5`, `10-40/10`) → bitmask.
fn parse_field(field: &str, min: u32, max: u32) -> Result<u64> {
    let mut mask = 0u64;
    for part in field.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((range, step)) => {
                let step: u32 = step.parse().context("invalid step")?;
                if step == 0 {
                    bail!("step must be positive");
                }
                (range, step)
            }
            None => (part, 1),
        };
        let (lo, hi) = match range {
            "*" => (min, max),
            _ => match range.split_once('-') {
                Some((lo, hi)) => (lo.parse()?, hi.parse()?),
                // A bare value matches itself; with a step (`5/15`, the
                // common cron extension) it means "from 5 to max, step 15".
                None => {
                    let v: u32 = range.parse()?;
                    if part.contains('/') { (v, max) } else { (v, v) }
                }
            },
        };
        if lo < min || hi > max || lo > hi {
            bail!("value out of range {min}-{max}");
        }
        mask |= (lo..=hi).step_by(step as usize).fold(0, |m, v| m | 1 << v);
    }
    if mask == 0 {
        bail!("empty field");
    }
    Ok(mask)
}

/// Spawn the scheduler loop; it dies with the runtime on daemon shutdown.
pub fn start(config: &Config) {
    let config = config.clone();
    tokio::spawn(run(config));
}

/// Wake up at the start of every minute and run whatever is due. Each pass
/// runs on the blocking pool — backup archiving is filesystem-heavy.
async fn run(config: Config) {
    info!("scheduler started (backup schedules)");
    let mut last_stamp: Option<i64> = None;
    loop {
        let now = unix_now();
        let sleep_secs = 60 - (now.rem_euclid(60)) as u64;
        tokio::time::sleep(std::time::Duration::from_secs(sleep_secs)).await;

        // Dedup by wall-clock minute: a fast loop or a clock step backwards
        // must not run the same minute's jobs twice.
        let stamp = unix_now() / 60;
        if last_stamp == Some(stamp) {
            continue;
        }
        last_stamp = Some(stamp);

        let config = config.clone();
        let result =
            tokio::task::spawn_blocking(move || run_due_backups(&config, Moment::now())).await;
        if let Err(err) = result {
            warn!(error = %err, "scheduler pass panicked");
        }
    }
}

/// One scheduler pass: back up every app whose policy schedule fires now.
/// Failures are logged per app — one broken app must not stop the rest.
fn run_due_backups(config: &Config, at: Moment) {
    let store = AppStore::new(config.daemon.apps_dir.clone());
    let apps = match store.list() {
        Ok(apps) => apps,
        Err(err) => {
            warn!(error = %format!("{err:#}"), "scheduler: cannot list apps");
            return;
        }
    };
    // The daemon's own scope: the root daemon sees the system storages, a
    // user-run `asc serve` also the user's own. A policy naming a storage
    // this daemon cannot see fails per app below and is logged.
    let storages = match StorageList::load() {
        Ok(list) => list,
        Err(err) => {
            warn!(error = %format!("{err:#}"), "scheduler: cannot load backup storages");
            return;
        }
    };
    for meta in apps {
        let policy = match load_policy(&store, &meta.id) {
            Ok(Some(policy)) => policy,
            Ok(None) => continue,
            Err(err) => {
                warn!(app = %meta.id, error = %format!("{err:#}"), "scheduler: cannot read backup policy");
                continue;
            }
        };
        let Some(raw) = policy.schedule.as_deref() else {
            continue;
        };
        let schedule = match Schedule::parse(raw) {
            Ok(schedule) => schedule,
            Err(err) => {
                warn!(app = %meta.id, schedule = raw, error = %err, "scheduler: invalid backup schedule");
                continue;
            }
        };
        if !schedule.matches(at) {
            continue;
        }
        let targets = if policy.storages.is_empty() {
            vec![storage::LOCAL_NAME.to_string()]
        } else {
            policy.storages.clone()
        };
        for name in &targets {
            match backup::create_backup(config, &store, &meta, &storages, name, policy.keep) {
                Ok(saved) => info!(
                    app = %meta.id,
                    backup = %saved.name,
                    storage = %saved.storage,
                    bytes = saved.bytes,
                    "scheduled backup created"
                ),
                Err(err) => warn!(
                    app = %meta.id,
                    storage = %name,
                    error = %format!("{err:#}"),
                    "scheduled backup failed"
                ),
            }
        }
    }
}

/// The app's backup policy, `None` when it has none configured.
fn load_policy(
    store: &AppStore,
    app_id: &str,
) -> Result<Option<crate::daemon::pkg::settings::BackupPolicy>> {
    let config_dir = store.app_dir(app_id)?.join("config");
    SettingValues::load(&config_dir)?.backup_policy()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn moment(minute: u32, hour: u32, day: u32, month: u32, weekday: u32) -> Moment {
        Moment {
            minute,
            hour,
            day,
            month,
            weekday,
        }
    }

    #[test]
    fn daily_sugar_fires_once_a_day() {
        for raw in ["daily@03:15", "03:15", "DAILY@03:15"] {
            let s = Schedule::parse(raw).unwrap();
            assert!(s.matches(moment(15, 3, 1, 1, 0)), "{raw}");
            assert!(s.matches(moment(15, 3, 31, 12, 6)), "{raw}");
            assert!(!s.matches(moment(16, 3, 1, 1, 0)), "{raw}");
            assert!(!s.matches(moment(15, 4, 1, 1, 0)), "{raw}");
        }
    }

    #[test]
    fn invalid_schedules_are_rejected() {
        for raw in [
            "",
            "daily",
            "daily@25:00",
            "daily@03:60",
            "3:5",
            "daily@3:05",
            "weekly@03:00",
            "* * * *",
            "* * * * * *",
            "60 * * * *",
            "* 24 * * *",
            "* * 0 * *",
            "* * 32 * *",
            "* * * 13 *",
            "* * * * 8",
            "*/0 * * * *",
            "a * * * *",
            "5-1 * * * *",
        ] {
            assert!(Schedule::parse(raw).is_err(), "must reject '{raw}'");
        }
    }

    #[test]
    fn cron_fields_match() {
        // Every 15 minutes during working hours on weekdays.
        let s = Schedule::parse("*/15 9-18 * * 1-5").unwrap();
        assert!(s.matches(moment(0, 9, 10, 6, 1)));
        assert!(s.matches(moment(45, 18, 10, 6, 5)));
        assert!(!s.matches(moment(10, 9, 10, 6, 1))); // off-step minute
        assert!(!s.matches(moment(0, 8, 10, 6, 1))); // before hours
        assert!(!s.matches(moment(0, 9, 10, 6, 0))); // Sunday

        // Lists and single values.
        let s = Schedule::parse("0 3 1,15 * *").unwrap();
        assert!(s.matches(moment(0, 3, 1, 2, 3)));
        assert!(s.matches(moment(0, 3, 15, 2, 3)));
        assert!(!s.matches(moment(0, 3, 2, 2, 3)));
    }

    #[test]
    fn sunday_is_both_0_and_7() {
        let with_seven = Schedule::parse("0 0 * * 7").unwrap();
        let with_zero = Schedule::parse("0 0 * * 0").unwrap();
        let sunday = moment(0, 0, 5, 1, 0);
        assert!(with_seven.matches(sunday));
        assert!(with_zero.matches(sunday));
        assert_eq!(with_seven, with_zero);
    }

    #[test]
    fn restricted_day_and_weekday_match_either() {
        // Vixie cron: 'on the 13th OR on Friday' when both are restricted.
        let s = Schedule::parse("0 0 13 * 5").unwrap();
        assert!(s.matches(moment(0, 0, 13, 6, 2))); // the 13th, a Tuesday
        assert!(s.matches(moment(0, 0, 20, 6, 5))); // a Friday, not the 13th
        assert!(!s.matches(moment(0, 0, 20, 6, 2))); // neither
    }

    #[test]
    fn moment_from_epoch_is_utc_independent() {
        // 2026-01-01 00:00:00 UTC was a Thursday; whatever the node's
        // timezone, the fields must be internally consistent.
        let m = Moment::local(1_767_225_600);
        assert!(m.minute < 60 && m.hour < 24);
        assert!((1..=31).contains(&m.day));
        assert!((1..=12).contains(&m.month));
        assert!(m.weekday < 7);
    }
}
