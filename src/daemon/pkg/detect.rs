//! Detects which installation methods a cloned repository supports (DMN-106):
//! ASC's own manifest/stack (already installable), Docker Compose, a bare
//! Dockerfile, Swarm, Kubernetes and Helm (detected, not yet installable —
//! see the per-method `supported` flag callers attach).
//!
//! Detection is deliberately permissive: a compose file is parsed into a
//! loose structure with no `#[serde(deny_unknown_fields)]` (unlike
//! [`super::manifest`], where that catches a typo in *our own* format — here
//! it would turn "this repository uses a compose feature we don't model"
//! into a false negative), and every read/parse failure along the way is
//! simply not counted as a match rather than aborting the whole detection.
//! The filesystem walk used for a nested `Dockerfile*` or a stray Kubernetes
//! manifest is bounded (depth and entry count) so a huge or adversarial
//! repository cannot make an inspect hang.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::manifest::{Manifest, StackManifest};

/// One way a repository could be installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum InstallMethod {
    AscManifest,
    AscStack,
    DockerCompose,
    Dockerfile,
    Swarm,
    Kubernetes,
    Helm,
}

impl InstallMethod {
    /// Whether `asc install`/the platform can actually install this method
    /// today. Swarm/Kubernetes/Helm are detected so the operator sees "found,
    /// not supported" instead of the repository looking unrecognized, but
    /// none of the three has an install path yet.
    pub fn supported(self) -> bool {
        matches!(self, InstallMethod::AscManifest | InstallMethod::AscStack)
    }
}

/// One detected method, with the file(s) that triggered it and whatever
/// summary items apply (compose/swarm service names, Kubernetes `kind`s).
#[derive(Debug, Clone)]
pub struct DetectedMethod {
    pub kind: InstallMethod,
    /// Paths relative to the package directory.
    pub files: Vec<String>,
    pub items: Vec<String>,
}

const WALK_MAX_DEPTH: usize = 3;
const WALK_MAX_ENTRIES: usize = 5_000;
const SKIP_DIRS: &[&str] = &[".git", "node_modules", "vendor", "target", "dist"];
const KUBERNETES_SIGNATURE_BYTES: usize = 4096;

const COMPOSE_NAMES: &[&str] = &[
    "compose.yml",
    "compose.yaml",
    "docker-compose.yml",
    "docker-compose.yaml",
];
const COMPOSE_SUBDIRS: &[&str] = &["deploy", "docker", ".docker"];
const KUBERNETES_DIRS: &[&str] = &["k8s", "kubernetes", "manifests", "deploy/k8s"];

/// Detect every install method `dir` (a package directory — the repository
/// root, or a monorepo subdirectory) supports. Order matches the table in
/// the feature doc: ASC's own formats first, then everything else.
pub fn detect(dir: &Path) -> Vec<DetectedMethod> {
    let mut methods = Vec::new();

    if dir.join(StackManifest::FILE).exists() {
        methods.push(DetectedMethod {
            kind: InstallMethod::AscStack,
            files: vec![StackManifest::FILE.to_string()],
            items: Vec::new(),
        });
    }
    if dir.join(Manifest::FILE).exists() {
        methods.push(DetectedMethod {
            kind: InstallMethod::AscManifest,
            files: vec![Manifest::FILE.to_string()],
            items: Vec::new(),
        });
    }

    let mut swarm_files: Vec<String> = Vec::new();
    let mut swarm_items: Vec<String> = Vec::new();
    if let Some(compose_path) = find_compose_file(dir) {
        let rel = relative(dir, &compose_path);
        let parsed = fs::read_to_string(&compose_path)
            .ok()
            .and_then(|raw| serde_yaml::from_str::<ComposeDoc>(&raw).ok());
        let services: Vec<String> = parsed
            .as_ref()
            .map(|doc| doc.services.keys().cloned().collect())
            .unwrap_or_default();
        methods.push(DetectedMethod {
            kind: InstallMethod::DockerCompose,
            files: vec![rel.clone()],
            items: services.clone(),
        });
        let is_swarm = parsed.as_ref().is_some_and(|doc| {
            doc.x_swarm.is_some() || doc.services.values().any(|s| s.deploy.is_some())
        });
        if is_swarm {
            swarm_files.push(rel);
            swarm_items = services;
        }
    }
    for name in ["docker-stack.yml", "docker-stack.yaml"] {
        let path = dir.join(name);
        if path.is_file() {
            swarm_files.push(name.to_string());
        }
    }
    if !swarm_files.is_empty() {
        methods.push(DetectedMethod {
            kind: InstallMethod::Swarm,
            files: swarm_files,
            items: swarm_items,
        });
    }

    // One bounded walk covers both a nested Dockerfile and stray Kubernetes
    // manifests, so the entry budget is shared rather than doubled.
    let root_dockerfile = dir.join("Dockerfile");
    let mut walk = WalkFindings::default();
    let mut budget = WALK_MAX_ENTRIES;
    walk_dir(dir, 0, &mut budget, root_dockerfile.is_file(), &mut walk);

    let dockerfile = if root_dockerfile.is_file() {
        Some(root_dockerfile)
    } else {
        walk.dockerfile
    };
    if let Some(path) = dockerfile {
        methods.push(DetectedMethod {
            kind: InstallMethod::Dockerfile,
            files: vec![relative(dir, &path)],
            items: Vec::new(),
        });
    }

    let mut k8s_files: Vec<String> = KUBERNETES_DIRS
        .iter()
        .filter(|rel| dir.join(rel).is_dir())
        .map(|rel| (*rel).to_string())
        .collect();
    let mut k8s_kinds: Vec<String> = Vec::new();
    for (path, kind) in &walk.kubernetes_yaml {
        k8s_files.push(relative(dir, path));
        if !k8s_kinds.contains(kind) {
            k8s_kinds.push(kind.clone());
        }
    }
    if !k8s_files.is_empty() {
        methods.push(DetectedMethod {
            kind: InstallMethod::Kubernetes,
            files: k8s_files,
            items: k8s_kinds,
        });
    }

    if let Some(chart) = find_helm_chart(dir) {
        methods.push(DetectedMethod {
            kind: InstallMethod::Helm,
            files: vec![relative(dir, &chart)],
            items: Vec::new(),
        });
    }

    methods
}

fn find_compose_file(dir: &Path) -> Option<PathBuf> {
    for name in COMPOSE_NAMES {
        let path = dir.join(name);
        if path.is_file() {
            return Some(path);
        }
    }
    for sub in COMPOSE_SUBDIRS {
        for name in COMPOSE_NAMES {
            let path = dir.join(sub).join(name);
            if path.is_file() {
                return Some(path);
            }
        }
    }
    None
}

fn find_helm_chart(dir: &Path) -> Option<PathBuf> {
    let chart = dir.join("Chart.yaml");
    let content = fs::read_to_string(&chart).ok()?;
    (content.contains("apiVersion:") && content.contains("name:")).then_some(chart)
}

#[derive(Default)]
struct WalkFindings {
    dockerfile: Option<PathBuf>,
    /// (path, `kind:` value) for every yaml file whose first few KiB look
    /// like a Kubernetes manifest.
    kubernetes_yaml: Vec<(PathBuf, String)>,
}

/// Depth-and-budget-bounded recursive scan. `depth` is the number of
/// directory levels below the package root; `budget` is shared across the
/// whole walk and decremented per entry visited, so a single huge directory
/// cannot exhaust it any faster than many small ones.
fn walk_dir(
    dir: &Path,
    depth: usize,
    budget: &mut usize,
    have_root_dockerfile: bool,
    out: &mut WalkFindings,
) {
    if depth > WALK_MAX_DEPTH || *budget == 0 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if *budget == 0 {
            return;
        }
        *budget -= 1;
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            if SKIP_DIRS.contains(&name.as_ref()) {
                continue;
            }
            walk_dir(&path, depth + 1, budget, have_root_dockerfile, out);
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        if out.dockerfile.is_none() && !have_root_dockerfile && name.starts_with("Dockerfile") {
            out.dockerfile = Some(path.clone());
            continue;
        }
        if (name.ends_with(".yml") || name.ends_with(".yaml"))
            && let Some(kind) = kubernetes_signature(&path)
        {
            out.kubernetes_yaml.push((path, kind));
        }
    }
}

/// Reads only the first [`KUBERNETES_SIGNATURE_BYTES`] of `path` — never the
/// whole file, which could be arbitrarily large — and, if both `apiVersion:`
/// and `kind:` show up in that prefix, extracts the `kind:` line's value.
/// A YAML manifest conventionally opens with exactly these two keys, so a
/// prefix check is enough without a full (and, for an arbitrary uploaded
/// repository, untrusted) YAML parse.
fn kubernetes_signature(path: &Path) -> Option<String> {
    let mut buf = vec![0u8; KUBERNETES_SIGNATURE_BYTES];
    let mut file = File::open(path).ok()?;
    let read = file.read(&mut buf).ok()?;
    buf.truncate(read);
    let text = String::from_utf8_lossy(&buf);
    if !text.contains("apiVersion:") || !text.contains("kind:") {
        return None;
    }
    text.lines().find_map(|line| {
        let trimmed = line.trim();
        trimmed
            .strip_prefix("kind:")
            .map(|value| value.trim().trim_matches(['"', '\'']).to_string())
            .filter(|value| !value.is_empty())
    })
}

fn relative(dir: &Path, path: &Path) -> String {
    path.strip_prefix(dir)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// Loose compose shape (DMN-106): only what detection needs, and nothing
/// marked `deny_unknown_fields` — compose is a large, foreign format, and a
/// feature this daemon doesn't model must not turn "detected" into "failed
/// to parse".
#[derive(Debug, Deserialize, Default)]
struct ComposeDoc {
    #[serde(default)]
    services: BTreeMap<String, ComposeService>,
    #[serde(default, rename = "x-swarm")]
    x_swarm: Option<serde_yaml::Value>,
}

#[derive(Debug, Deserialize, Default)]
struct ComposeService {
    #[serde(default)]
    deploy: Option<serde_yaml::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, rel: &str, content: &str) {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    #[test]
    fn detects_asc_manifest_only() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "asc.yaml", "name: app\nversion: '1.0'\n");
        let methods = detect(dir.path());
        assert_eq!(methods.len(), 1);
        assert_eq!(methods[0].kind, InstallMethod::AscManifest);
        assert!(methods[0].kind.supported());
    }

    #[test]
    fn detects_asc_stack_only() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "asc.stack.yaml", "name: stack\napps: []\n");
        let methods = detect(dir.path());
        assert_eq!(methods.len(), 1);
        assert_eq!(methods[0].kind, InstallMethod::AscStack);
    }

    #[test]
    fn plain_compose_is_detected_without_swarm() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "docker-compose.yml",
            "services:\n  web:\n    image: nginx\n  db:\n    image: postgres\n",
        );
        let methods = detect(dir.path());
        assert_eq!(methods.len(), 1);
        assert_eq!(methods[0].kind, InstallMethod::DockerCompose);
        assert!(!methods[0].kind.supported());
        let mut items = methods[0].items.clone();
        items.sort();
        assert_eq!(items, vec!["db", "web"]);
    }

    #[test]
    fn compose_with_deploy_reports_both_compose_and_swarm() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "compose.yaml",
            "services:\n  web:\n    image: nginx\n    deploy:\n      replicas: 3\n",
        );
        let methods = detect(dir.path());
        let kinds: Vec<_> = methods.iter().map(|m| m.kind).collect();
        assert!(kinds.contains(&InstallMethod::DockerCompose));
        assert!(kinds.contains(&InstallMethod::Swarm));
        // Same file backs both detections.
        assert_eq!(
            methods
                .iter()
                .find(|m| m.kind == InstallMethod::Swarm)
                .unwrap()
                .files,
            vec!["compose.yaml"]
        );
    }

    #[test]
    fn x_swarm_alone_triggers_swarm_on_a_plain_compose_file() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "docker-compose.yaml",
            "x-swarm:\n  enabled: true\nservices:\n  web:\n    image: nginx\n",
        );
        let methods = detect(dir.path());
        assert!(methods.iter().any(|m| m.kind == InstallMethod::Swarm));
    }

    #[test]
    fn docker_stack_file_is_its_own_swarm_trigger() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "docker-stack.yml",
            "services:\n  web:\n    image: nginx\n",
        );
        let methods = detect(dir.path());
        assert_eq!(methods.len(), 1);
        assert_eq!(methods[0].kind, InstallMethod::Swarm);
        assert_eq!(methods[0].files, vec!["docker-stack.yml"]);
    }

    #[test]
    fn root_dockerfile_is_found_without_a_walk() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Dockerfile", "FROM alpine\n");
        let methods = detect(dir.path());
        assert_eq!(methods.len(), 1);
        assert_eq!(methods[0].kind, InstallMethod::Dockerfile);
        assert_eq!(methods[0].files, vec!["Dockerfile"]);
    }

    #[test]
    fn nested_dockerfile_is_found_by_the_bounded_walk() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "docker/Dockerfile.prod", "FROM alpine\n");
        let methods = detect(dir.path());
        assert_eq!(methods.len(), 1);
        assert_eq!(methods[0].kind, InstallMethod::Dockerfile);
        assert_eq!(
            methods[0].files,
            vec!["docker/Dockerfile.prod".replace('/', std::path::MAIN_SEPARATOR_STR)]
        );
    }

    #[test]
    fn walk_skips_conventional_junk_directories() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "node_modules/pkg/Dockerfile", "FROM alpine\n");
        let methods = detect(dir.path());
        assert!(methods.is_empty());
    }

    #[test]
    fn kubernetes_directory_alone_is_a_trigger() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("k8s")).unwrap();
        let methods = detect(dir.path());
        assert_eq!(methods.len(), 1);
        assert_eq!(methods[0].kind, InstallMethod::Kubernetes);
        assert_eq!(methods[0].files, vec!["k8s"]);
        assert!(methods[0].items.is_empty());
    }

    #[test]
    fn a_loose_kubernetes_manifest_is_found_by_signature_and_reports_its_kind() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "deploy.yaml",
            "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: web\n",
        );
        let methods = detect(dir.path());
        assert_eq!(methods.len(), 1);
        assert_eq!(methods[0].kind, InstallMethod::Kubernetes);
        assert_eq!(methods[0].items, vec!["Deployment"]);
    }

    #[test]
    fn a_yaml_file_missing_either_marker_does_not_count() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "values.yaml",
            "replicaCount: 1\nimage:\n  repository: nginx\n",
        );
        let methods = detect(dir.path());
        assert!(methods.is_empty());
    }

    #[test]
    fn helm_chart_needs_both_markers() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "Chart.yaml",
            "apiVersion: v2\nname: mychart\nversion: 0.1.0\n",
        );
        let methods = detect(dir.path());
        assert_eq!(methods.len(), 1);
        assert_eq!(methods[0].kind, InstallMethod::Helm);
    }

    #[test]
    fn a_chart_yaml_without_a_name_is_not_helm() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Chart.yaml", "apiVersion: v2\n");
        let methods = detect(dir.path());
        assert!(methods.is_empty());
    }

    #[test]
    fn compose_with_deploy_block_and_a_manifest_reports_every_method() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "asc.yaml", "name: app\nversion: '1.0'\n");
        write(
            dir.path(),
            "docker-compose.yml",
            "services:\n  web:\n    image: nginx\n    deploy:\n      replicas: 1\n",
        );
        write(dir.path(), "Dockerfile", "FROM alpine\n");
        let methods = detect(dir.path());
        let kinds: Vec<_> = methods.iter().map(|m| m.kind).collect();
        assert!(kinds.contains(&InstallMethod::AscManifest));
        assert!(kinds.contains(&InstallMethod::DockerCompose));
        assert!(kinds.contains(&InstallMethod::Swarm));
        assert!(kinds.contains(&InstallMethod::Dockerfile));
    }
}
