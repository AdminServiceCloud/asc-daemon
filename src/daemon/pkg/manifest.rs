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
    /// Prebuilt Docker image to pull (type: docker). May be given alongside
    /// `image-build`, in which case the installer offers a choice (DMN-050).
    #[serde(default)]
    pub image: Option<String>,
    /// Build the Docker image locally from a Dockerfile in the package
    /// instead of (or as an alternative to) pulling a prebuilt `image`
    /// (type: docker, DMN-050).
    #[serde(default, rename = "image-build")]
    pub image_build: Option<ImageBuild>,
    /// Keep the container's stdin open (Engine `OpenStdin`, like `docker run
    /// -i`) so console input from `asc attach` reaches the app (type: docker).
    #[serde(default)]
    pub stdin: bool,
    /// Allocate a pseudo-TTY (Engine `Tty`, like `docker run -t`) — for apps
    /// whose console expects a terminal (type: docker).
    #[serde(default)]
    pub tty: bool,
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

/// Local image build (type: docker, DMN-050): the Engine builds the image
/// from a Dockerfile shipped in the package repository. Mirrors the
/// `image-build` object of `asc.schema.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageBuild {
    /// Build context directory, relative to the manifest (default `.`).
    #[serde(default)]
    pub context: Option<String>,
    /// Dockerfile path, relative to the build context (default `Dockerfile`).
    #[serde(default)]
    pub dockerfile: Option<String>,
    /// Build arguments passed to the Engine (`--build-arg`).
    #[serde(default)]
    pub args: std::collections::BTreeMap<String, String>,
    /// Tag for the built image (default `asc-local/<app>:latest`).
    #[serde(default)]
    pub tag: Option<String>,
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
            AppType::Docker
                if self.runtime.image.is_none() && self.runtime.image_build.is_none() =>
            {
                bail!(
                    "manifest '{}': type is docker but neither runtime.image nor runtime.image-build is set",
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
  stdin: true
  tty: true
"#;
        let m: Manifest = serde_yaml::from_str(yaml).unwrap();
        m.validate().unwrap();
        assert_eq!(m.app_type, AppType::Docker);
        assert_eq!(m.runtime.image.as_deref(), Some("nginx:1.27"));
        assert!(m.runtime.stdin && m.runtime.tty);
    }

    #[test]
    fn ports_and_volumes_in_manifest_are_rejected() {
        // Ports and volumes moved to asc.settings.yaml (setting types
        // `ports` / `volumes`) — a manifest still declaring them must fail
        // loudly, not silently ignore the sections.
        for extra in ["ports: [8080]", "volumes: [/data]"] {
            let yaml =
                format!("name: x\nversion: '1'\ntype: docker\nruntime:\n  image: i\n{extra}\n");
            assert!(serde_yaml::from_str::<Manifest>(&yaml).is_err(), "{extra}");
        }
    }

    #[test]
    fn docker_without_image_is_rejected() {
        // Neither runtime.image nor runtime.image-build — rejected.
        let yaml = "name: broken\nversion: '1.0'\ntype: docker\n";
        let m: Manifest = serde_yaml::from_str(yaml).unwrap();
        assert!(m.validate().is_err());
    }

    #[test]
    fn image_build_satisfies_docker_and_parses() {
        // image-build alone is enough for a docker manifest (no prebuilt image).
        let yaml = r#"
name: web
version: 1.0.0
type: docker
runtime:
  image-build:
    context: ./docker
    dockerfile: Dockerfile.prod
    args:
      VERSION: "1.0"
    tag: web:local
"#;
        let m: Manifest = serde_yaml::from_str(yaml).unwrap();
        m.validate().unwrap();
        let build = m.runtime.image_build.as_ref().unwrap();
        assert_eq!(build.context.as_deref(), Some("./docker"));
        assert_eq!(build.dockerfile.as_deref(), Some("Dockerfile.prod"));
        assert_eq!(build.tag.as_deref(), Some("web:local"));
        assert_eq!(build.args.get("VERSION").map(String::as_str), Some("1.0"));
        assert!(m.runtime.image.is_none());
    }

    #[test]
    fn image_and_image_build_may_coexist() {
        // Offering both is valid — the installer asks (or a flag decides).
        let yaml = r#"
name: web
version: 1.0.0
type: docker
runtime:
  image: nginx:1.27
  image-build:
    context: .
"#;
        let m: Manifest = serde_yaml::from_str(yaml).unwrap();
        m.validate().unwrap();
        assert!(m.runtime.image.is_some() && m.runtime.image_build.is_some());
    }

    #[test]
    fn image_build_rejects_unknown_fields() {
        let yaml =
            "name: x\nversion: '1'\ntype: docker\nruntime:\n  image-build:\n    oops: true\n";
        assert!(serde_yaml::from_str::<Manifest>(yaml).is_err());
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
"#,
        );
        let all: Vec<&str> = s
            .install_order(
                s.apps
                    .iter()
                    .filter(|a| !a.optional)
                    .map(|a| a.name.as_str()),
            )
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

        let yaml =
            "name: bad\nversion: '1'\napps:\n  - { name: a, path: ./a, depends_on: [ghost] }\n";
        let s: StackManifest = serde_yaml::from_str(yaml).unwrap();
        assert!(s.validate().is_err());
    }

    #[test]
    fn env_in_manifest_is_rejected() {
        // env moved to asc.settings.yaml (settings with `env:` keys) —
        // a manifest still declaring it must fail loudly, not silently.
        let yaml = "name: x\nversion: '1'\ntype: docker\nruntime:\n  image: i\nenv:\n  - name: PORT\n    default: 80\n";
        assert!(serde_yaml::from_str::<Manifest>(yaml).is_err());
    }
}
