//! `asc.yaml` — the application manifest of a package repository.
//!
//! Mirrors `registry/schema/asc.schema.json` (the source of truth for the
//! format). Unknown fields are rejected so typos in manifests surface at
//! install time instead of being silently ignored.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// Root of `asc.yaml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    #[serde(rename = "type")]
    pub app_type: AppType,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub category: Option<String>,
    /// Relative path to asc.settings.yaml (applied in DMN-017).
    #[serde(default)]
    pub settings: Option<String>,
    #[serde(default)]
    pub runtime: RuntimeSpec,
    #[serde(default)]
    pub env: Vec<EnvVar>,
    #[serde(default)]
    pub ports: Vec<u16>,
    #[serde(default)]
    pub volumes: Vec<String>,
    #[serde(default)]
    pub database: Option<DatabaseSpec>,
    #[serde(default)]
    pub requirements: Option<Requirements>,
    #[serde(default)]
    pub healthcheck: Option<Healthcheck>,
    #[serde(default)]
    pub hooks: Option<Hooks>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AppType {
    Docker,
    Native,
    Utility,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSpec {
    /// Docker image (type: docker).
    #[serde(default)]
    pub image: Option<String>,
    /// Install commands (type: native/utility).
    #[serde(default)]
    pub install: Vec<String>,
    /// Start command (type: native).
    #[serde(default)]
    pub start: Option<String>,
    #[serde(default)]
    pub stop: Option<String>,
    #[serde(default)]
    pub uninstall: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvVar {
    pub name: String,
    #[serde(default)]
    pub default: Option<serde_yaml::Value>,
    #[serde(default)]
    pub secret: bool,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseSpec {
    pub engine: String,
    #[serde(default)]
    pub env_prefix: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Requirements {
    #[serde(default)]
    pub ram: Option<String>,
    #[serde(default)]
    pub disk: Option<String>,
    #[serde(default)]
    pub cpu: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Healthcheck {
    #[serde(default)]
    pub http: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub interval: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hooks {
    #[serde(default)]
    pub pre_backup: Option<String>,
    #[serde(default)]
    pub post_backup: Option<String>,
    #[serde(default)]
    pub post_install: Option<String>,
}

impl Manifest {
    pub const FILE: &'static str = "asc.yaml";

    /// Load and validate the manifest from a package directory.
    pub fn load(dir: &Path) -> Result<Self> {
        let path = dir.join(Self::FILE);
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("cannot read manifest {}", path.display()))?;
        let manifest: Manifest = serde_yaml::from_str(&raw)
            .with_context(|| format!("invalid manifest {}", path.display()))?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Consistency checks beyond what serde enforces.
    pub fn validate(&self) -> Result<()> {
        crate::daemon::apps::meta::validate_id(&self.name)
            .with_context(|| format!("invalid app name '{}' in manifest", self.name))?;
        match self.app_type {
            AppType::Docker if self.runtime.image.is_none() => {
                bail!(
                    "manifest '{}': type is docker but runtime.image is missing",
                    self.name
                )
            }
            AppType::Native if self.runtime.start.is_none() => {
                bail!(
                    "manifest '{}': type is native but runtime.start is missing",
                    self.name
                )
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_docker_manifest() {
        let yaml = r#"
name: nginx
version: 1.27.0
type: docker
description: "Web server"
runtime:
  image: nginx:1.27
env:
  - name: PORT
    default: 8080
ports: [8080]
volumes: [/data]
"#;
        let m: Manifest = serde_yaml::from_str(yaml).unwrap();
        m.validate().unwrap();
        assert_eq!(m.app_type, AppType::Docker);
        assert_eq!(m.runtime.image.as_deref(), Some("nginx:1.27"));
        assert_eq!(m.ports, [8080]);
    }

    #[test]
    fn docker_without_image_is_rejected() {
        let yaml = "name: broken\nversion: '1.0'\ntype: docker\n";
        let m: Manifest = serde_yaml::from_str(yaml).unwrap();
        assert!(m.validate().is_err());
    }

    #[test]
    fn native_needs_start_command() {
        let yaml = "name: tool\nversion: '1.0'\ntype: native\n";
        let m: Manifest = serde_yaml::from_str(yaml).unwrap();
        assert!(m.validate().is_err());
        let yaml = "name: tool\nversion: '1.0'\ntype: native\nruntime:\n  start: ./run.sh\n";
        let m: Manifest = serde_yaml::from_str(yaml).unwrap();
        m.validate().unwrap();
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let yaml = "name: x\nversion: '1'\ntype: utility\noops: true\n";
        assert!(serde_yaml::from_str::<Manifest>(yaml).is_err());
    }
}
