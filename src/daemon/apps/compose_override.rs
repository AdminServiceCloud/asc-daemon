//! The daemon's compose override file, `<app_dir>/.asc/compose.override.yaml`.
//!
//! Per-service settings of a compose app — extra environment variables,
//! CPU/memory limits and the restart policy — are not edits to the package's
//! own compose files: those belong to the repository and are replaced by
//! every upgrade. They live in this file instead, outside `repository/`
//! (so git never touches it) and given to the plugin as the last `-f`
//! ([`super::compose::project_of`]), where compose merges it over the
//! package's files.
//!
//! The file only ever carries the four fields above — no `volumes`, `ports`,
//! `build` or `env_file` — so it can neither widen what a service mounts
//! from the host nor carry a relative path. Values are written with every
//! `$` doubled: compose interpolates `${…}` in every file it reads, and a
//! password typed into a settings form must reach the container verbatim.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_yaml::{Mapping, Value};

use crate::daemon::compose::ResolvedProject;

/// What the override file sets for one service.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ServiceOverride {
    pub environment: BTreeMap<String, String>,
    /// Cores.
    pub cpus: Option<f64>,
    /// Bytes.
    pub memory_bytes: Option<u64>,
    /// `no` | `always` | `on-failure[:N]` | `unless-stopped`.
    pub restart: Option<String>,
}

impl ServiceOverride {
    pub fn is_empty(&self) -> bool {
        self.environment.is_empty()
            && self.cpus.is_none()
            && self.memory_bytes.is_none()
            && self.restart.is_none()
    }
}

/// The whole override file: one entry per service that has any setting.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OverrideFile {
    pub services: BTreeMap<String, ServiceOverride>,
}

/// First line of every file the daemon writes.
const HEADER: &str = "# Managed by asc: per-service settings of this compose app.\n\
                      # Applied over the package's compose files; kept across upgrades.\n";

impl OverrideFile {
    /// Read the file at `path`; a missing file is an empty override.
    pub fn load(path: &Path) -> Result<Self> {
        let raw = match fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
        };
        Self::parse(&raw).with_context(|| format!("invalid {}", path.display()))
    }

    /// Parse the YAML form. Limits are accepted in both spellings compose
    /// knows (`deploy.resources.limits.*` and the legacy `cpus`/`mem_limit`),
    /// since [`Self::render`] picks whichever the package's file uses.
    pub fn parse(raw: &str) -> Result<Self> {
        let doc: Value = serde_yaml::from_str(raw).context("not valid YAML")?;
        let mut file = Self::default();
        let Some(services) = doc.get("services") else {
            return Ok(file);
        };
        let Some(services) = services.as_mapping() else {
            if services.is_null() {
                return Ok(file);
            }
            bail!("`services` must be a mapping");
        };
        for (name, body) in services {
            let Some(name) = name.as_str() else {
                bail!("service names must be strings");
            };
            let mut entry = ServiceOverride::default();
            match body.get("environment") {
                Some(Value::Mapping(env)) => {
                    for (key, value) in env {
                        let Some(key) = key.as_str() else {
                            bail!("environment names of '{name}' must be strings");
                        };
                        entry
                            .environment
                            .insert(key.to_string(), unescape(&scalar_string(value)));
                    }
                }
                Some(Value::Sequence(list)) => {
                    for item in list {
                        let item = scalar_string(item);
                        let (key, value) = item.split_once('=').unwrap_or((&item, ""));
                        entry.environment.insert(key.to_string(), unescape(value));
                    }
                }
                _ => {}
            }
            let limits = body
                .get("deploy")
                .and_then(|d| d.get("resources"))
                .and_then(|r| r.get("limits"));
            entry.cpus = limits
                .and_then(|l| l.get("cpus"))
                .or_else(|| body.get("cpus"))
                .and_then(number_f64);
            entry.memory_bytes = limits
                .and_then(|l| l.get("memory"))
                .or_else(|| body.get("mem_limit"))
                .and_then(|v| match v {
                    Value::Number(n) => n.as_u64(),
                    Value::String(s) => crate::daemon::compose::parse_memory(s),
                    _ => None,
                });
            entry.restart = body.get("restart").map(scalar_string);
            if !entry.is_empty() {
                file.services.insert(name.to_string(), entry);
            }
        }
        Ok(file)
    }

    /// The YAML the daemon writes. `base` is the project as the package's
    /// own files resolve it: where a service sets its limits with the legacy
    /// `cpus:`/`mem_limit:` keys, the override uses those same keys — compose
    /// refuses a service that sets one limit both ways.
    pub fn render(&self, base: &ResolvedProject) -> String {
        let mut services = Mapping::new();
        for (name, entry) in &self.services {
            if entry.is_empty() {
                continue;
            }
            let legacy = base.services.get(name);
            let legacy_cpus = legacy.is_some_and(|s| s.legacy_cpus());
            let legacy_memory = legacy.is_some_and(|s| s.legacy_memory());
            let mut body = Mapping::new();
            if !entry.environment.is_empty() {
                let mut env = Mapping::new();
                for (key, value) in &entry.environment {
                    env.insert(Value::String(key.clone()), Value::String(escape(value)));
                }
                body.insert("environment".into(), Value::Mapping(env));
            }
            let mut limits = Mapping::new();
            if let Some(cpus) = entry.cpus {
                if legacy_cpus {
                    body.insert("cpus".into(), Value::Number(cpus.into()));
                } else {
                    limits.insert("cpus".into(), Value::Number(cpus.into()));
                }
            }
            if let Some(memory) = entry.memory_bytes {
                // A string, the way `config` prints it: a bare YAML number
                // above 2^53 would not survive every parser on the way.
                let memory = Value::String(memory.to_string());
                if legacy_memory {
                    body.insert("mem_limit".into(), memory);
                } else {
                    limits.insert("memory".into(), memory);
                }
            }
            if !limits.is_empty() {
                let mut resources = Mapping::new();
                resources.insert("limits".into(), Value::Mapping(limits));
                let mut deploy = Mapping::new();
                deploy.insert("resources".into(), Value::Mapping(resources));
                body.insert("deploy".into(), Value::Mapping(deploy));
            }
            if let Some(restart) = &entry.restart {
                body.insert("restart".into(), Value::String(restart.clone()));
            }
            services.insert(Value::String(name.clone()), Value::Mapping(body));
        }
        let mut doc = Mapping::new();
        doc.insert("services".into(), Value::Mapping(services));
        let yaml = serde_yaml::to_string(&Value::Mapping(doc)).unwrap_or_default();
        format!("{HEADER}{yaml}")
    }

    /// Split off the entries of services `known` no longer lists: they
    /// stay in `self` only for services that still exist, and are returned
    /// for the caller to set aside rather than drop.
    pub fn prune(&mut self, known: &[String]) -> OverrideFile {
        let (kept, orphaned): (BTreeMap<_, _>, BTreeMap<_, _>) = std::mem::take(&mut self.services)
            .into_iter()
            .partition(|(name, _)| known.iter().any(|k| k == name));
        self.services = kept;
        OverrideFile { services: orphaned }
    }
}

/// Write `content` next to `path` as a temporary file the daemon owns the
/// way it owns the app directory (`0600`: environment values may be
/// secrets), and return its path. Nothing replaces `path` yet — the caller
/// validates the candidate first ([`commit`]).
pub fn write_candidate(path: &Path, content: &str) -> Result<PathBuf> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let dir = path.parent().context("override path has no directory")?;
    fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let tmp = dir.join(format!(
        ".{}.tmp-{}",
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        std::process::id()
    ));
    fs::write(&tmp, content).with_context(|| format!("cannot write {}", tmp.display()))?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("cannot set permissions on {}", tmp.display()))?;
    // Owned like `.asc/` itself: the root daemon writing into a regular
    // user's app must not leave a file that user cannot read back.
    if let Ok(md) = fs::metadata(dir) {
        let _ = std::os::unix::fs::chown(&tmp, Some(md.uid()), Some(md.gid()));
    }
    Ok(tmp)
}

/// Atomically put a validated candidate in place of `path`.
pub fn commit(candidate: &Path, path: &Path) -> Result<()> {
    fs::rename(candidate, path).with_context(|| format!("cannot replace {}", path.display()))
}

/// Write `file` to `path` (rendered against `base`) without a validation
/// step — or remove `path` when nothing is left in it.
pub fn store(file: &OverrideFile, base: &ResolvedProject, path: &Path) -> Result<()> {
    if file.services.values().all(ServiceOverride::is_empty) {
        return match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("cannot remove {}", path.display())),
        };
    }
    let candidate = write_candidate(path, &file.render(base))?;
    commit(&candidate, path)
}

/// A scalar as text: strings as they are, numbers and booleans spelled out.
fn scalar_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => String::new(),
        other => serde_yaml::to_string(other)
            .unwrap_or_default()
            .trim()
            .to_string(),
    }
}

fn number_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// `$` → `$$`, so compose's interpolation leaves the value as typed.
fn escape(value: &str) -> String {
    value.replace('$', "$$")
}

/// The reverse of [`escape`].
fn unescape(value: &str) -> String {
    value.replace("$$", "$")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base(yaml: &str) -> ResolvedProject {
        serde_yaml::from_str(yaml).unwrap()
    }

    fn sample() -> OverrideFile {
        let mut file = OverrideFile::default();
        file.services.insert(
            "worker".into(),
            ServiceOverride {
                environment: BTreeMap::from([
                    ("PASSWORD".into(), "pa$word${HOME}".into()),
                    ("MODE".into(), "prod".into()),
                ]),
                cpus: Some(0.5),
                memory_bytes: Some(512 << 20),
                restart: Some("on-failure:3".into()),
            },
        );
        file
    }

    #[test]
    fn render_then_parse_round_trips_values_verbatim() {
        let yaml = sample().render(&ResolvedProject::default());
        assert!(yaml.starts_with("# Managed by asc"));
        // Doubled on disk, so compose does not interpolate them.
        assert!(yaml.contains("pa$$word$${HOME}"), "{yaml}");
        assert!(yaml.contains("deploy:"), "{yaml}");
        // Never anything that could mount or build.
        for key in ["volumes", "ports", "build", "env_file"] {
            assert!(!yaml.contains(key), "{key} in {yaml}");
        }
        assert_eq!(OverrideFile::parse(&yaml).unwrap(), sample());
    }

    #[test]
    fn legacy_limit_keys_follow_the_package_file() {
        let legacy = base(
            "services:\n  worker:\n    image: x\n    cpus: 1.0\n    mem_limit: \"1073741824\"\n",
        );
        let yaml = sample().render(&legacy);
        assert!(!yaml.contains("deploy:"), "{yaml}");
        assert!(yaml.contains("cpus: 0.5"), "{yaml}");
        assert!(yaml.contains("mem_limit: '536870912'"), "{yaml}");
        assert_eq!(OverrideFile::parse(&yaml).unwrap(), sample());
    }

    #[test]
    fn prune_sets_vanished_services_aside() {
        let mut file = sample();
        file.services.insert(
            "gone".into(),
            ServiceOverride {
                restart: Some("always".into()),
                ..Default::default()
            },
        );
        let orphaned = file.prune(&["worker".into(), "web".into()]);
        assert_eq!(file.services.keys().collect::<Vec<_>>(), ["worker"]);
        assert_eq!(orphaned.services.keys().collect::<Vec<_>>(), ["gone"]);
        // Nothing to prune: nothing set aside.
        assert!(file.prune(&["worker".into()]).services.is_empty());
    }

    #[test]
    fn a_missing_or_empty_file_is_an_empty_override() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("compose.override.yaml");
        assert_eq!(OverrideFile::load(&path).unwrap(), OverrideFile::default());
        assert_eq!(
            OverrideFile::parse("services: {}\n").unwrap(),
            OverrideFile::default()
        );
        assert_eq!(OverrideFile::parse("").unwrap(), OverrideFile::default());
    }

    #[test]
    fn store_writes_privately_and_removes_an_empty_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".asc/compose.override.yaml");
        store(&sample(), &ResolvedProject::default(), &path).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(OverrideFile::load(&path).unwrap(), sample());
        // No temporary file left behind.
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);

        store(&OverrideFile::default(), &ResolvedProject::default(), &path).unwrap();
        assert!(!path.exists());
    }
}
