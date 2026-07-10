//! `meta.json` — per-app metadata, the source of truth for recovery.
//!
//! Lives at `/asc/apps/<id>/meta.json`. The index of installed apps is
//! rebuilt by scanning these files, so they must always be written atomically.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// Metadata of one installed application.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppMeta {
    /// Unique id — the directory name under the apps root.
    pub id: String,
    /// Human-readable name (defaults to the id).
    pub name: String,
    /// Linux user owning this app (fixed at install time).
    pub owner: Owner,
    /// Installed version — a git tag of the package repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Package source (registry name or repository URL).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// What the app should be doing; enforced after daemon restart/reboot.
    #[serde(default)]
    pub desired_state: DesiredState,
    /// Resource quota from asc.settings.yaml (DMN-021), normalized at
    /// install/upgrade time. `None` = unlimited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota: Option<Quota>,
    pub runtime: Runtime,
}

/// Normalized resource quota of one app.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Quota {
    /// CPU cores limit (enforced for Docker via NanoCpus).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_cores: Option<f64>,
    /// Memory limit in bytes (enforced for Docker via Memory).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ram_bytes: Option<u64>,
    /// Disk usage limit in bytes (recorded; enforcement is a next increment).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Owner {
    pub uid: u32,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DesiredState {
    Running,
    #[default]
    Stopped,
}

/// How the app runs; determines which driver manages it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Runtime {
    /// Docker container managed by the daemon.
    Docker { container: String },
    /// Native app running as a systemd unit (`asc-app-<id>.service`).
    Systemd { unit: String },
    /// Plain process supervised via pid-file.
    Process {
        command: String,
        #[serde(default)]
        args: Vec<String>,
    },
}

impl Runtime {
    /// Short technical kind identifier (not translated, used in tables/API).
    pub fn kind(&self) -> &'static str {
        match self {
            Runtime::Docker { .. } => "docker",
            Runtime::Systemd { .. } => "systemd",
            Runtime::Process { .. } => "process",
        }
    }
}

impl AppMeta {
    pub const FILE: &'static str = "meta.json";

    /// Load metadata from an app directory.
    pub fn load(dir: &Path) -> Result<Self> {
        let path = dir.join(Self::FILE);
        let raw =
            fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
        serde_json::from_str(&raw).with_context(|| format!("invalid {}", path.display()))
    }

    /// Persist metadata atomically (tmp file + rename), so a crash mid-write
    /// never leaves a truncated meta.json behind.
    pub fn save(&self, dir: &Path) -> Result<()> {
        let path = dir.join(Self::FILE);
        let tmp = dir.join("meta.json.tmp");
        let raw = serde_json::to_string_pretty(self).context("cannot serialize app metadata")?;
        fs::write(&tmp, raw).with_context(|| format!("cannot write {}", tmp.display()))?;
        fs::rename(&tmp, &path).with_context(|| format!("cannot replace {}", path.display()))?;
        Ok(())
    }
}

/// Validate an app id before using it as a directory name.
///
/// Strict on purpose: the id ends up in filesystem paths, container names and
/// systemd unit names, so anything beyond `[a-z0-9_-]` is rejected (this also
/// rules out path traversal).
pub fn validate_id(id: &str) -> Result<()> {
    let ok_len = (1..=64).contains(&id.len());
    let ok_start = id.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit());
    let ok_chars = id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if !(ok_len && ok_start && ok_chars) {
        bail!("invalid app id '{id}': use 1-64 chars [a-z0-9_-], starting with a letter or digit");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> AppMeta {
        AppMeta {
            id: "helloworld".into(),
            name: "Hello World".into(),
            owner: Owner {
                uid: 1000,
                name: "alice".into(),
            },
            version: Some("v1.2.0".into()),
            source: Some("official".into()),
            desired_state: DesiredState::Running,
            quota: Some(Quota {
                cpu_cores: Some(1.5),
                ram_bytes: Some(512 << 20),
                disk_bytes: Some(10 << 30),
            }),
            runtime: Runtime::Docker {
                container: "asc-helloworld".into(),
            },
        }
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        sample().save(dir.path()).unwrap();
        let loaded = AppMeta::load(dir.path()).unwrap();
        assert_eq!(loaded.id, "helloworld");
        assert_eq!(loaded.desired_state, DesiredState::Running);
        assert_eq!(loaded.runtime.kind(), "docker");
        assert_eq!(loaded.quota.unwrap().ram_bytes, Some(512 << 20));
        // No leftover tmp file after an atomic save.
        assert!(!dir.path().join("meta.json.tmp").exists());
    }

    #[test]
    fn runtime_tag_is_stable() {
        let json = serde_json::to_string(&sample().runtime).unwrap();
        assert!(json.contains(r#""type":"docker""#));
    }

    #[test]
    fn id_validation() {
        assert!(validate_id("helloworld").is_ok());
        assert!(validate_id("app-2_test").is_ok());
        assert!(validate_id("").is_err());
        assert!(validate_id("-lead").is_err());
        assert!(validate_id("UPPER").is_err());
        assert!(validate_id("../etc").is_err());
        assert!(validate_id("a b").is_err());
        assert!(validate_id(&"x".repeat(65)).is_err());
    }
}
