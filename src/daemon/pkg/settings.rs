//! `asc.settings.yaml` — user-adjustable app settings and the resource quota
//! (DMN-017 settings, DMN-021 quota).
//!
//! Mirrors `registry/schema/asc.settings.schema.json` (the source of truth
//! for the format). The manifest references the file via `settings:`; values
//! chosen by the user live in `<app_dir>/config/settings.json` and are
//! applied to the runtime on the next (re)start.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use super::manifest::Manifest;
use crate::daemon::apps::meta::Quota;
use crate::daemon::config::Config;
use crate::daemon::i18n::{Msg, t, tf};

/// Root of `asc.settings.yaml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsFile {
    /// Resource quota applied to the app instance (DMN-021).
    #[serde(default)]
    pub quota: Option<QuotaSpec>,
    /// User-adjustable settings (DMN-017).
    #[serde(default)]
    pub settings: Vec<SettingDef>,
    /// Start command override with `${VAR}` interpolation from the package
    /// env defaults (DMN-018). Docker: replaces the container command (runs
    /// through `/bin/sh -c`); native: replaces `runtime.start`.
    #[serde(default)]
    pub start_command: Option<String>,
}

/// `quota:` section — resource limits as the package author writes them.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuotaSpec {
    /// CPU cores limit, e.g. `0.5` or `2`.
    #[serde(default)]
    pub max_cpu: Option<f64>,
    /// Memory limit as a size string, e.g. `512M`, `2G`.
    #[serde(default)]
    pub max_ram: Option<String>,
    /// Disk usage limit, e.g. `10G`. Recorded in meta.json; per-runtime
    /// enforcement (Docker storage-opt / fs quotas) is a next increment.
    #[serde(default)]
    pub max_disk: Option<String>,
}

impl QuotaSpec {
    /// Parse the human sizes into the normalized form stored in meta.json.
    pub fn normalize(&self) -> Result<Quota> {
        if let Some(cpu) = self.max_cpu
            && cpu <= 0.0
        {
            bail!(t(Msg::ErrQuotaCpu));
        }
        Ok(Quota {
            cpu_cores: self.max_cpu,
            ram_bytes: self.max_ram.as_deref().map(parse_size).transpose()?,
            disk_bytes: self.max_disk.as_deref().map(parse_size).transpose()?,
        })
    }
}

/// `512M` / `2G` / `1.5 GiB` → bytes (binary units, like Docker's `-m`).
pub fn parse_size(raw: &str) -> Result<u64> {
    let s = raw.trim();
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (number, suffix) = s.split_at(split);
    let value: f64 = number
        .parse()
        .map_err(|_| anyhow!(tf(Msg::ErrQuotaSize, raw)))?;
    let factor: u64 = match suffix.trim().to_ascii_uppercase().as_str() {
        "" | "B" => 1,
        "K" | "KB" | "KIB" => 1 << 10,
        "M" | "MB" | "MIB" => 1 << 20,
        "G" | "GB" | "GIB" => 1 << 30,
        "T" | "TB" | "TIB" => 1u64 << 40,
        _ => bail!(tf(Msg::ErrQuotaSize, raw)),
    };
    let bytes = value * factor as f64;
    if !(bytes > 0.0 && bytes.is_finite()) {
        bail!(tf(Msg::ErrQuotaSize, raw));
    }
    Ok(bytes as u64)
}

/// One setting definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingDef {
    pub key: String,
    #[serde(rename = "type")]
    pub kind: SettingKind,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub default: Option<serde_yaml::Value>,
    #[serde(default)]
    pub required: bool,
    /// Allowed values (kind: enum).
    #[serde(default)]
    pub values: Vec<serde_yaml::Value>,
    #[serde(default)]
    pub limits: Option<Limits>,
    /// Environment variable to expose the value as.
    #[serde(default)]
    pub env: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SettingKind {
    String,
    Number,
    Boolean,
    Enum,
    Secret,
    /// Published container ports; the value is a list of port numbers.
    /// With `env:` the list is exposed comma-joined (a single port — as is).
    Ports,
    /// App volumes; the value is a list of `/container/path[:host]` or
    /// `name:/container/path[:ro]` entries (see the package-manager doc).
    Volumes,
}

impl SettingKind {
    /// The settings-editor category this kind belongs to.
    pub fn category(self) -> SettingCategory {
        match self {
            SettingKind::Ports => SettingCategory::Ports,
            SettingKind::Volumes => SettingCategory::Volumes,
            _ => SettingCategory::Environments,
        }
    }
}

/// Categories of the settings editor (`asc app settings`): the user first
/// picks a category, then edits its settings. Labels are technical
/// identifiers and stay English by convention, like table headers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingCategory {
    Environments,
    Ports,
    Volumes,
    Quota,
    StartCommand,
}

impl SettingCategory {
    pub const ALL: [SettingCategory; 5] = [
        SettingCategory::Environments,
        SettingCategory::Ports,
        SettingCategory::Volumes,
        SettingCategory::Quota,
        SettingCategory::StartCommand,
    ];

    pub fn label(self) -> &'static str {
        match self {
            SettingCategory::Environments => "environments",
            SettingCategory::Ports => "ports",
            SettingCategory::Volumes => "volumes",
            SettingCategory::Quota => "quota",
            SettingCategory::StartCommand => "start_command",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
    #[serde(default)]
    pub min_length: Option<usize>,
    #[serde(default)]
    pub max_length: Option<usize>,
    /// Regex constraint from the schema — accepted, not enforced yet (the
    /// daemon has no regex dependency; the platform UI validates it).
    #[serde(default)]
    pub pattern: Option<String>,
}

impl SettingsFile {
    pub const FILE: &'static str = "asc.settings.yaml";

    /// Load the settings file referenced by the manifest; `None` when the
    /// manifest declares no `settings:`.
    pub fn load_for(manifest_dir: &Path, manifest: &Manifest) -> Result<Option<Self>> {
        let Some(rel) = &manifest.settings else {
            return Ok(None);
        };
        // The path comes from the package repository — keep it inside it.
        let rel_path = Path::new(rel);
        if rel_path.is_absolute()
            || rel_path.has_root()
            || rel_path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            bail!("invalid settings path '{rel}' in asc.yaml");
        }
        let path = manifest_dir.join(rel_path);
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("cannot read settings file {}", path.display()))?;
        let file: SettingsFile = serde_yaml::from_str(&raw)
            .with_context(|| format!("invalid settings file {}", path.display()))?;
        file.validate()?;
        Ok(Some(file))
    }

    /// Consistency checks beyond what serde enforces.
    pub fn validate(&self) -> Result<()> {
        let mut seen = std::collections::HashSet::new();
        for def in &self.settings {
            if !valid_key(&def.key) {
                bail!(
                    "invalid setting key '{}': use [a-z0-9_], starting with a letter or digit",
                    def.key
                );
            }
            if !seen.insert(def.key.as_str()) {
                bail!("duplicate setting key '{}'", def.key);
            }
            if def.kind == SettingKind::Enum && def.values.is_empty() {
                bail!("setting '{}' is an enum but has no values", def.key);
            }
            // Ports/volumes hold lists: a scalar default is a manifest bug.
            if matches!(def.kind, SettingKind::Ports | SettingKind::Volumes)
                && let Some(default) = &def.default
                && !default.is_sequence()
            {
                bail!(
                    "setting '{}' is of type {:?} — its default must be a list",
                    def.key,
                    def.kind
                );
            }
            if def.kind == SettingKind::Volumes
                && let Some(serde_yaml::Value::Sequence(items)) = &def.default
            {
                for item in items {
                    let entry = yaml_scalar(item);
                    super::install::validate_volume(&entry)
                        .with_context(|| format!("setting '{}' default", def.key))?;
                }
            }
            if let Some(limits) = &def.limits {
                if let (Some(min), Some(max)) = (limits.min, limits.max)
                    && min > max
                {
                    bail!(
                        "setting '{}': limits.min is greater than limits.max",
                        def.key
                    );
                }
                if let (Some(min), Some(max)) = (limits.min_length, limits.max_length)
                    && min > max
                {
                    bail!(
                        "setting '{}': limits.min_length is greater than limits.max_length",
                        def.key
                    );
                }
            }
        }
        if let Some(quota) = &self.quota {
            quota.normalize()?;
        }
        Ok(())
    }
}

/// Schema key pattern `^[a-z0-9][a-z0-9_]*$` without a regex dependency.
fn valid_key(key: &str) -> bool {
    let mut chars = key.chars();
    let ok_first = chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    ok_first && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

impl SettingDef {
    /// Validate raw user input against this definition and return the typed
    /// value to store. Errors are user-facing (translated).
    pub fn parse_value(&self, raw: &str) -> Result<serde_json::Value> {
        let raw = raw.trim();
        match self.kind {
            SettingKind::Number => {
                let n: f64 = raw.parse().map_err(|_| anyhow!(t(Msg::ErrSettingNumber)))?;
                if let Some(limits) = &self.limits
                    && (limits.min.is_some_and(|min| n < min)
                        || limits.max.is_some_and(|max| n > max))
                {
                    bail!(tf(Msg::ErrSettingRange, range_hint(limits.min, limits.max)));
                }
                Ok(number_value(n))
            }
            SettingKind::Boolean => match raw.to_ascii_lowercase().as_str() {
                "true" | "yes" | "on" | "1" | "да" => Ok(serde_json::Value::Bool(true)),
                "false" | "no" | "off" | "0" | "нет" => Ok(serde_json::Value::Bool(false)),
                _ => bail!(t(Msg::ErrSettingBool)),
            },
            SettingKind::Enum => {
                for value in &self.values {
                    if yaml_scalar(value) == raw {
                        return serde_json::to_value(value).context("cannot convert enum value");
                    }
                }
                bail!(tf(Msg::ErrSettingEnum, self.values_hint()));
            }
            SettingKind::String | SettingKind::Secret => {
                let len = raw.chars().count();
                if let Some(limits) = &self.limits
                    && (limits.min_length.is_some_and(|min| len < min)
                        || limits.max_length.is_some_and(|max| len > max))
                {
                    bail!(tf(
                        Msg::ErrSettingLength,
                        range_hint(
                            limits.min_length.map(|v| v as f64),
                            limits.max_length.map(|v| v as f64)
                        )
                    ));
                }
                Ok(serde_json::Value::String(raw.to_string()))
            }
            SettingKind::Ports => {
                let mut ports = Vec::new();
                for token in raw.split([',', ' ']).filter(|t| !t.is_empty()) {
                    let port: u16 = token
                        .parse()
                        .ok()
                        .filter(|p| *p != 0)
                        .ok_or_else(|| anyhow!(t(Msg::ErrSettingPort)))?;
                    if let Some(limits) = &self.limits
                        && (limits.min.is_some_and(|min| f64::from(port) < min)
                            || limits.max.is_some_and(|max| f64::from(port) > max))
                    {
                        bail!(tf(Msg::ErrSettingRange, range_hint(limits.min, limits.max)));
                    }
                    ports.push(serde_json::json!(port));
                }
                if ports.is_empty() {
                    bail!(t(Msg::ErrSettingPort));
                }
                Ok(serde_json::Value::Array(ports))
            }
            SettingKind::Volumes => {
                let mut volumes = Vec::new();
                for token in raw.split([',', ' ']).filter(|t| !t.is_empty()) {
                    if super::install::validate_volume(token).is_err() {
                        bail!(tf(Msg::ErrSettingVolume, token));
                    }
                    volumes.push(serde_json::json!(token));
                }
                if volumes.is_empty() {
                    bail!(tf(Msg::ErrSettingVolume, raw));
                }
                Ok(serde_json::Value::Array(volumes))
            }
        }
    }

    /// Short technical constraint hint for lists/prompts (`1..=200`, `a|b|c`).
    pub fn constraint_hint(&self) -> Option<String> {
        match self.kind {
            SettingKind::Enum => Some(self.values_hint()),
            SettingKind::Number | SettingKind::Ports => {
                let limits = self.limits.as_ref()?;
                (limits.min.is_some() || limits.max.is_some())
                    .then(|| range_hint(limits.min, limits.max))
            }
            SettingKind::Volumes => None,
            SettingKind::String | SettingKind::Secret => {
                let limits = self.limits.as_ref()?;
                (limits.min_length.is_some() || limits.max_length.is_some()).then(|| {
                    format!(
                        "len {}",
                        range_hint(
                            limits.min_length.map(|v| v as f64),
                            limits.max_length.map(|v| v as f64)
                        )
                    )
                })
            }
            SettingKind::Boolean => Some("true|false".into()),
        }
    }

    /// `a|b|c` — the allowed enum values.
    pub fn values_hint(&self) -> String {
        self.values
            .iter()
            .map(yaml_scalar)
            .collect::<Vec<_>>()
            .join("|")
    }

    /// Value as shown to the user; secrets are masked, lists are readable.
    pub fn display_of(&self, value: &serde_json::Value) -> String {
        if self.kind == SettingKind::Secret {
            return "•••".into();
        }
        if let serde_json::Value::Array(items) = value {
            return items.iter().map(json_scalar).collect::<Vec<_>>().join(", ");
        }
        json_scalar(value)
    }
}

/// `min..=max` with open ends: `1..=200`, `≥1`, `≤200`.
fn range_hint(min: Option<f64>, max: Option<f64>) -> String {
    match (min, max) {
        (Some(min), Some(max)) => format!("{}..={}", trim_float(min), trim_float(max)),
        (Some(min), None) => format!("≥{}", trim_float(min)),
        (None, Some(max)) => format!("≤{}", trim_float(max)),
        (None, None) => String::new(),
    }
}

fn trim_float(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

/// Integral floats become JSON integers (`10`, not `10.0`).
fn number_value(n: f64) -> serde_json::Value {
    if n.fract() == 0.0 && (i64::MIN as f64..=i64::MAX as f64).contains(&n) {
        serde_json::Value::Number((n as i64).into())
    } else {
        serde_json::Number::from_f64(n)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null)
    }
}

fn yaml_scalar(value: &serde_yaml::Value) -> String {
    match value {
        serde_yaml::Value::String(s) => s.clone(),
        other => serde_yaml::to_string(other)
            .map(|s| s.trim().to_string())
            .unwrap_or_default(),
    }
}

/// JSON value as a plain string; lists (ports, volumes) join with a comma —
/// a single-element list reads as the bare value (`CS2_PORT=27015`).
fn json_scalar(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(items) => {
            items.iter().map(json_scalar).collect::<Vec<_>>().join(",")
        }
        other => other.to_string(),
    }
}

/// Chosen setting values of one app: `<app_dir>/config/settings.json`.
#[derive(Debug, Default)]
pub struct SettingValues {
    map: serde_json::Map<String, serde_json::Value>,
}

impl SettingValues {
    pub const FILE: &'static str = "settings.json";

    /// Reserved keys for the user's app-level overrides. The `$` prefix
    /// cannot collide with package setting keys (see [`valid_key`]).
    pub const QUOTA_KEY: &'static str = "$quota";
    pub const START_COMMAND_KEY: &'static str = "$start_command";

    /// The user's start-command override (the `start_command` editor
    /// category); wins over the package's `start_command`.
    pub fn start_command_override(&self) -> Option<&str> {
        self.get(Self::START_COMMAND_KEY).and_then(|v| v.as_str())
    }

    /// The user's quota overrides (the `quota` editor category): a partial
    /// [`QuotaSpec`] whose set fields win over the package quota.
    pub fn quota_override(&self) -> Result<Option<QuotaSpec>> {
        let Some(value) = self.get(Self::QUOTA_KEY) else {
            return Ok(None);
        };
        serde_json::from_value(value.clone())
            .map(Some)
            .context("invalid $quota override in settings.json")
    }

    /// Load the values; a missing file means nothing was chosen yet.
    pub fn load(config_dir: &Path) -> Result<Self> {
        let path = config_dir.join(Self::FILE);
        let raw = match fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
        };
        let map =
            serde_json::from_str(&raw).with_context(|| format!("invalid {}", path.display()))?;
        Ok(Self { map })
    }

    /// Persist atomically (tmp + rename). The file may hold secrets, so it is
    /// written 0600 before any content lands in it.
    pub fn save(&self, config_dir: &Path) -> Result<()> {
        let path = config_dir.join(Self::FILE);
        let tmp = config_dir.join("settings.json.tmp");
        let raw =
            serde_json::to_string_pretty(&self.map).context("cannot serialize setting values")?;
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)
                .with_context(|| format!("cannot write {}", tmp.display()))?;
            file.write_all(raw.as_bytes())
                .with_context(|| format!("cannot write {}", tmp.display()))?;
        }
        fs::rename(&tmp, &path).with_context(|| format!("cannot replace {}", path.display()))?;
        Ok(())
    }

    /// Fill in defaults for keys the user has not set (install, upgrade).
    pub fn merge_defaults(&mut self, defs: &[SettingDef]) {
        for def in defs {
            if self.map.contains_key(&def.key) {
                continue;
            }
            if let Some(default) = &def.default
                && let Ok(value) = serde_json::to_value(default)
            {
                self.map.insert(def.key.clone(), value);
            }
        }
    }

    pub fn get(&self, key: &str) -> Option<&serde_json::Value> {
        self.map.get(key)
    }

    pub fn set(&mut self, key: &str, value: serde_json::Value) {
        self.map.insert(key.to_string(), value);
    }

    /// Drop a stored value (used by the editor's '-' reset).
    pub fn remove(&mut self, key: &str) {
        self.map.remove(key);
    }

    /// Current value of a setting as shown in the editor (`-` when unset).
    pub fn display(&self, def: &SettingDef) -> String {
        match self.get(&def.key) {
            Some(value) => def.display_of(value),
            None => "-".into(),
        }
    }

    /// `(ENV_NAME, value)` pairs for the settings that declare `env:` and
    /// have a value (chosen by the user or seeded from the defaults).
    /// Secrets are included — exposing them to the app is what their `env:`
    /// is for; required settings without a value are simply absent.
    pub fn env_pairs(&self, defs: &[SettingDef]) -> Vec<(String, String)> {
        defs.iter()
            .filter_map(|def| {
                let name = def.env.clone()?;
                let value = self.get(&def.key)?;
                Some((name, json_scalar(value)))
            })
            .collect()
    }
}

/// Manifest directory of an installed app: the repository root, or — for
/// monorepo and stack packages — located through the registry entry and the
/// origin recorded in meta.json (`package: "stack/app"`), preferring the
/// source the app was installed from.
pub fn manifest_dir_of(config: &Config, app_dir: &Path) -> Result<PathBuf> {
    let meta = crate::daemon::apps::meta::AppMeta::load(app_dir)?;
    locate_installed(config, &meta, app_dir).map(|(dir, _)| dir)
}

/// Like [`manifest_dir_of`], but for callers that already hold the meta and
/// need the stack manifest too (its shared env merges into the app manifest).
pub fn locate_installed(
    config: &Config,
    meta: &crate::daemon::apps::meta::AppMeta,
    app_dir: &Path,
) -> Result<(PathBuf, Option<super::manifest::StackManifest>)> {
    let repo = app_dir.join("repository");
    if repo.join(Manifest::FILE).exists() {
        return Ok((repo, None));
    }
    let package_spec = meta.package.clone().unwrap_or_else(|| meta.id.clone());
    let (package, stack_app) = match package_spec.split_once('/') {
        Some((package, app)) => (package, Some(app)),
        None => (package_spec.as_str(), None),
    };
    let installed_from = meta
        .source
        .as_deref()
        .and_then(|s| s.split_once(':'))
        .map(|(name, _)| name);
    let resolved = super::registry::RegistryClient::new(config)?
        .resolve_preferring(package, installed_from)?;
    super::install::locate_manifest(&repo, resolved.entry.source.path.as_deref(), stack_app)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(yaml: &str) -> SettingDef {
        serde_yaml::from_str(yaml).unwrap()
    }

    #[test]
    fn parses_full_settings_file() {
        let yaml = r#"
quota:
  max_cpu: 1.5
  max_ram: 512M
  max_disk: 10G
settings:
  - key: server_name
    type: string
    default: "My Server"
    required: true
  - key: max_players
    type: number
    default: 10
    limits: { min: 1, max: 200 }
  - key: difficulty
    type: enum
    values: [peaceful, easy, normal, hard]
    default: normal
  - key: rcon_password
    type: secret
    required: true
  - key: enable_backups
    type: boolean
    default: true
"#;
        let file: SettingsFile = serde_yaml::from_str(yaml).unwrap();
        file.validate().unwrap();
        let quota = file.quota.unwrap().normalize().unwrap();
        assert_eq!(quota.cpu_cores, Some(1.5));
        assert_eq!(quota.ram_bytes, Some(512 << 20));
        assert_eq!(quota.disk_bytes, Some(10 << 30));
        assert_eq!(file.settings.len(), 5);
    }

    #[test]
    fn size_parsing() {
        assert_eq!(parse_size("512M").unwrap(), 512 << 20);
        assert_eq!(parse_size("2G").unwrap(), 2 << 30);
        assert_eq!(
            parse_size("1.5G").unwrap(),
            (1.5 * (1u64 << 30) as f64) as u64
        );
        assert_eq!(parse_size("100").unwrap(), 100);
        assert_eq!(parse_size("1 GiB").unwrap(), 1 << 30);
        assert!(parse_size("").is_err());
        assert!(parse_size("-1G").is_err());
        assert!(parse_size("10X").is_err());
    }

    #[test]
    fn quota_rejects_nonpositive_cpu() {
        let spec = QuotaSpec {
            max_cpu: Some(0.0),
            max_ram: None,
            max_disk: None,
        };
        assert!(spec.normalize().is_err());
    }

    #[test]
    fn number_limits_are_enforced() {
        let d = def("{ key: players, type: number, limits: { min: 1, max: 200 } }");
        assert_eq!(d.parse_value("10").unwrap(), serde_json::json!(10));
        assert!(d.parse_value("0").is_err());
        assert!(d.parse_value("201").is_err());
        assert!(d.parse_value("abc").is_err());
    }

    #[test]
    fn enum_accepts_only_listed_values() {
        let d = def("{ key: mode, type: enum, values: [easy, hard] }");
        assert_eq!(d.parse_value("easy").unwrap(), serde_json::json!("easy"));
        assert!(d.parse_value("medium").is_err());
        assert_eq!(d.values_hint(), "easy|hard");
    }

    #[test]
    fn boolean_accepts_common_spellings() {
        let d = def("{ key: on, type: boolean }");
        assert_eq!(d.parse_value("yes").unwrap(), serde_json::json!(true));
        assert_eq!(d.parse_value("off").unwrap(), serde_json::json!(false));
        assert!(d.parse_value("maybe").is_err());
    }

    #[test]
    fn string_length_limits() {
        let d = def("{ key: name, type: string, limits: { min_length: 2, max_length: 4 } }");
        assert!(d.parse_value("a").is_err());
        assert!(d.parse_value("abcde").is_err());
        assert_eq!(d.parse_value("abc").unwrap(), serde_json::json!("abc"));
    }

    #[test]
    fn secrets_are_masked() {
        let d = def("{ key: token, type: secret }");
        assert_eq!(d.display_of(&serde_json::json!("hunter2")), "•••");
    }

    #[test]
    fn duplicate_and_invalid_keys_are_rejected() {
        let dup: SettingsFile = serde_yaml::from_str(
            "settings:\n  - { key: a, type: string }\n  - { key: a, type: number }\n",
        )
        .unwrap();
        assert!(dup.validate().is_err());
        let bad: SettingsFile =
            serde_yaml::from_str("settings:\n  - { key: BadKey, type: string }\n").unwrap();
        assert!(bad.validate().is_err());
    }

    #[test]
    fn enum_without_values_is_rejected() {
        let file: SettingsFile =
            serde_yaml::from_str("settings:\n  - { key: mode, type: enum }\n").unwrap();
        assert!(file.validate().is_err());
    }

    #[test]
    fn values_roundtrip_and_merge_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let defs = [
            def("{ key: players, type: number, default: 10 }"),
            def("{ key: name, type: string }"),
        ];
        let mut values = SettingValues::load(dir.path()).unwrap();
        values.merge_defaults(&defs);
        assert_eq!(values.get("players"), Some(&serde_json::json!(10)));
        assert_eq!(values.get("name"), None);
        values.set("players", serde_json::json!(50));
        values.save(dir.path()).unwrap();

        let reloaded = SettingValues::load(dir.path()).unwrap();
        assert_eq!(reloaded.get("players"), Some(&serde_json::json!(50)));
        assert!(!dir.path().join("settings.json.tmp").exists());
    }

    #[test]
    fn ports_values_parse_validate_and_join() {
        let d = def("{ key: game_port, type: ports, default: [27015] }");
        assert_eq!(d.kind.category(), SettingCategory::Ports);
        assert_eq!(
            d.parse_value("27015, 27020").unwrap(),
            serde_json::json!([27015, 27020])
        );
        assert_eq!(d.parse_value("27016").unwrap(), serde_json::json!([27016]));
        for bad in ["0", "70000", "abc", ""] {
            assert!(d.parse_value(bad).is_err(), "must reject '{bad}'");
        }
        // Per-port limits apply to every element.
        let limited = def("{ key: p, type: ports, limits: { min: 1024, max: 2048 } }");
        assert!(limited.parse_value("80").is_err());
        assert!(limited.parse_value("1024 2048").is_ok());
    }

    #[test]
    fn volumes_values_parse_and_validate() {
        let d = def("{ key: vols, type: volumes }");
        assert_eq!(d.kind.category(), SettingCategory::Volumes);
        assert_eq!(
            d.parse_value("/data /opt/x:store shared:/srv:ro").unwrap(),
            serde_json::json!(["/data", "/opt/x:store", "shared:/srv:ro"])
        );
        for bad in ["not-a-volume", "/data:a/b", ""] {
            assert!(d.parse_value(bad).is_err(), "must reject '{bad}'");
        }
    }

    #[test]
    fn list_defaults_are_validated() {
        // A scalar default for a list type is a manifest bug.
        let bad: SettingsFile =
            serde_yaml::from_str("settings:\n  - { key: p, type: ports, default: 27015 }\n")
                .unwrap();
        assert!(bad.validate().is_err());
        // Volume defaults are syntax-checked at load time.
        let bad: SettingsFile =
            serde_yaml::from_str("settings:\n  - { key: v, type: volumes, default: [broken] }\n")
                .unwrap();
        assert!(bad.validate().is_err());
        let ok: SettingsFile = serde_yaml::from_str(
            "settings:\n  - { key: v, type: volumes, default: [/data, /srv:maps] }\n",
        )
        .unwrap();
        ok.validate().unwrap();
    }

    #[test]
    fn overrides_live_under_reserved_keys() {
        let mut values = SettingValues::default();
        assert!(values.start_command_override().is_none());
        assert!(values.quota_override().unwrap().is_none());
        values.set(
            SettingValues::START_COMMAND_KEY,
            serde_json::json!("./run --fast"),
        );
        values.set(
            SettingValues::QUOTA_KEY,
            serde_json::json!({ "max_cpu": 1.5 }),
        );
        assert_eq!(values.start_command_override(), Some("./run --fast"));
        assert_eq!(values.quota_override().unwrap().unwrap().max_cpu, Some(1.5));
        // Reserved keys cannot collide with package settings.
        assert!(!valid_key(SettingValues::QUOTA_KEY));
        assert!(!valid_key(SettingValues::START_COMMAND_KEY));
    }

    #[test]
    fn env_pairs_cover_typed_values_and_skip_unset() {
        let defs = [
            def("{ key: name, type: string, default: My Server, env: SERVER_NAME }"),
            def("{ key: players, type: number, default: 10, env: MAX_PLAYERS }"),
            def("{ key: pvp, type: boolean, default: true, env: PVP }"),
            def("{ key: token, type: secret, env: TOKEN }"), // no value yet
            def("{ key: internal, type: string, default: x }"), // no env
        ];
        let mut values = SettingValues::default();
        values.merge_defaults(&defs);
        values.set("players", serde_json::json!(50));
        assert_eq!(
            values.env_pairs(&defs),
            [
                ("SERVER_NAME".to_string(), "My Server".to_string()),
                ("MAX_PLAYERS".to_string(), "50".to_string()),
                ("PVP".to_string(), "true".to_string()),
            ]
        );
    }

    #[test]
    fn settings_path_cannot_escape_the_repo() {
        let manifest: Manifest = serde_yaml::from_str(
            "name: x\nversion: '1'\ntype: utility\nsettings: ../../etc/passwd\n",
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        assert!(SettingsFile::load_for(dir.path(), &manifest).is_err());
    }
}
