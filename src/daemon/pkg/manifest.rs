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

/// Root of `asc.stack.yaml` — several applications shipped by one repository.
/// Mirrors `registry/schema/asc.stack.schema.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StackManifest {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub category: Option<String>,
    pub apps: Vec<StackApp>,
    /// Shared environment defaults for every app of the stack.
    #[serde(default)]
    pub env: Vec<EnvVar>,
}

/// One application of a stack.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StackApp {
    /// Name within the stack (`asc install <stack>/<name>`).
    pub name: String,
    /// Directory with the app's asc.yaml, relative to the stack root.
    pub path: String,
    /// Skipped on a whole-stack install unless requested explicitly.
    #[serde(default)]
    pub optional: bool,
    /// Apps of this stack that must be installed/started first.
    #[serde(default)]
    pub depends_on: Vec<String>,
}

impl StackManifest {
    pub const FILE: &'static str = "asc.stack.yaml";

    /// Load and validate the stack manifest from the stack root directory.
    pub fn load(dir: &Path) -> Result<Self> {
        let path = dir.join(Self::FILE);
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("cannot read stack manifest {}", path.display()))?;
        let stack: StackManifest = serde_yaml::from_str(&raw)
            .with_context(|| format!("invalid stack manifest {}", path.display()))?;
        stack.validate()?;
        Ok(stack)
    }

    pub fn validate(&self) -> Result<()> {
        if self.apps.is_empty() {
            bail!("stack '{}' declares no apps", self.name);
        }
        for app in &self.apps {
            crate::daemon::apps::meta::validate_id(&app.name)
                .with_context(|| format!("invalid app name '{}' in stack", app.name))?;
            for dep in &app.depends_on {
                if !self.apps.iter().any(|a| &a.name == dep) {
                    bail!(
                        "stack '{}': app '{}' depends on unknown app '{}'",
                        self.name,
                        app.name,
                        dep
                    );
                }
            }
        }
        // A cycle would make the install order undefined; fail at load time.
        self.install_order(self.apps.iter().map(|a| a.name.as_str()))?;
        Ok(())
    }

    pub fn app(&self, name: &str) -> Option<&StackApp> {
        self.apps.iter().find(|a| a.name == name)
    }

    /// Dependency-first install order for `wanted` apps plus everything they
    /// transitively depend on (dependencies win over `optional`).
    pub fn install_order<'a>(
        &self,
        wanted: impl IntoIterator<Item = &'a str>,
    ) -> Result<Vec<&StackApp>> {
        let mut order: Vec<&StackApp> = Vec::new();
        let mut state: std::collections::HashMap<&str, u8> = Default::default(); // 1 = visiting, 2 = done
        // Iterative DFS with an explicit stack; a gray→gray edge is a cycle.
        for name in wanted {
            let mut stack: Vec<(&str, usize)> = vec![(name, 0)];
            while let Some((current, next_dep)) = stack.pop() {
                let app = self
                    .app(current)
                    .with_context(|| format!("stack '{}' has no app '{current}'", self.name))?;
                if state.get(current) == Some(&2) {
                    continue;
                }
                state.insert(current, 1);
                match app.depends_on.get(next_dep) {
                    Some(dep) => {
                        if state.get(dep.as_str()) == Some(&1) {
                            bail!("stack '{}': dependency cycle through '{dep}'", self.name);
                        }
                        stack.push((current, next_dep + 1));
                        if state.get(dep.as_str()) != Some(&2) {
                            stack.push((dep, 0));
                        }
                    }
                    None => {
                        state.insert(current, 2);
                        order.push(app);
                    }
                }
            }
        }
        Ok(order)
    }
}

impl Manifest {
    pub const FILE: &'static str = "asc.yaml";

    /// Add stack-level env defaults the app does not define itself
    /// (the app's own declaration wins).
    pub fn merge_stack_env(&mut self, stack_env: &[EnvVar]) {
        for var in stack_env {
            if !self.env.iter().any(|e| e.name == var.name) {
                self.env.push(var.clone());
            }
        }
    }

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

    fn stack(yaml: &str) -> StackManifest {
        let stack: StackManifest = serde_yaml::from_str(yaml).unwrap();
        stack.validate().unwrap();
        stack
    }

    #[test]
    fn stack_install_order_puts_dependencies_first() {
        let s = stack(
            r#"
name: cs2
version: 1.0.0
apps:
  - { name: master, path: ./master }
  - { name: server, path: ./server, depends_on: [master] }
  - { name: extras, path: ./extras, optional: true }
env:
  - name: STEAM_APP_ID
    default: 730
"#,
        );
        let all: Vec<&str> = s
            .install_order(s.apps.iter().filter(|a| !a.optional).map(|a| a.name.as_str()))
            .unwrap()
            .iter()
            .map(|a| a.name.as_str())
            .collect();
        assert_eq!(all, ["master", "server"], "optional apps are skipped");

        let single: Vec<&str> = s
            .install_order(["server"])
            .unwrap()
            .iter()
            .map(|a| a.name.as_str())
            .collect();
        assert_eq!(single, ["master", "server"], "dependencies come along");
    }

    #[test]
    fn stack_rejects_cycles_and_unknown_deps() {
        let yaml = r#"
name: bad
version: '1'
apps:
  - { name: a, path: ./a, depends_on: [b] }
  - { name: b, path: ./b, depends_on: [a] }
"#;
        let s: StackManifest = serde_yaml::from_str(yaml).unwrap();
        assert!(s.validate().unwrap_err().to_string().contains("cycle"));

        let yaml = "name: bad\nversion: '1'\napps:\n  - { name: a, path: ./a, depends_on: [ghost] }\n";
        let s: StackManifest = serde_yaml::from_str(yaml).unwrap();
        assert!(s.validate().is_err());
    }

    #[test]
    fn stack_env_merges_without_overriding() {
        let mut m: Manifest =
            serde_yaml::from_str("name: web\nversion: '1'\ntype: docker\nruntime:\n  image: x\nenv:\n  - name: PORT\n    default: 80\n").unwrap();
        let stack_env: Vec<EnvVar> =
            serde_yaml::from_str("- name: PORT\n  default: 9999\n- name: TZ\n  default: UTC\n").unwrap();
        m.merge_stack_env(&stack_env);
        assert_eq!(m.env.len(), 2);
        let port = m.env.iter().find(|e| e.name == "PORT").unwrap();
        assert_eq!(port.default, Some(serde_yaml::Value::from(80)), "app wins");
        assert!(m.env.iter().any(|e| e.name == "TZ"), "stack default added");
    }
}
