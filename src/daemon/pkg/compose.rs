//! Installing from a bare `docker-compose.yml`/`compose.yml` (DMN-108): no
//! `asc.yaml`, no settings, no quota — the app is a `docker compose` project,
//! managed entirely through [`crate::daemon::compose`] rather than the usual
//! manifest-driven `provision()` path every other runtime kind goes through.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

use super::detect;

/// Locate the compose file a fresh compose-method install should use: the
/// same deterministic candidate [`detect::detect`] already showed the caller
/// during inspection. Re-run rather than trusted from the request, for the
/// same reason [`super::dockerfile::find`] does — a repository that changed
/// between inspect and install must not desync the daemon from what it is
/// about to run.
pub(super) fn find(manifest_dir: &Path) -> Result<String> {
    detect::detect(manifest_dir)
        .into_iter()
        .find(|m| m.kind == detect::InstallMethod::DockerCompose)
        .and_then(|m| m.files.into_iter().next())
        .ok_or_else(|| anyhow!("no compose file found in the package"))
}

#[derive(Debug, Deserialize, Default)]
struct ComposeDoc {
    #[serde(default)]
    services: BTreeMap<String, ComposeService>,
}

#[derive(Debug, Deserialize, Default)]
struct ComposeService {
    #[serde(default)]
    ports: Vec<serde_yaml::Value>,
    #[serde(default)]
    volumes: Vec<serde_yaml::Value>,
}

/// Ports a compose app publishes, read from the compose file itself rather
/// than the live Engine state — the only way a **stopped** compose app can
/// still report the ports it will bind on the next start, the same property
/// every other runtime kind already has via its settings. Only the common
/// short syntax (`"8080:80"`, `"80"`, `"127.0.0.1:8080:80"`, each optionally
/// `/udp`) is understood; an unrecognized shape (a port range, the long
/// mapping form) is simply not counted, the same permissive stance
/// [`detect`] already takes on the rest of a compose file.
pub(crate) fn published_ports(compose_path: &Path) -> Vec<crate::daemon::docker::PublishedPort> {
    let Ok(raw) = fs::read_to_string(compose_path) else {
        return Vec::new();
    };
    let Ok(doc) = serde_yaml::from_str::<ComposeDoc>(&raw) else {
        return Vec::new();
    };
    doc.services
        .values()
        .flat_map(|service| &service.ports)
        .filter_map(|entry| match entry {
            serde_yaml::Value::String(s) => parse_port_entry(s),
            serde_yaml::Value::Number(n) => parse_port_entry(&n.to_string()),
            _ => None,
        })
        .collect()
}

fn parse_port_entry(entry: &str) -> Option<crate::daemon::docker::PublishedPort> {
    use crate::daemon::docker::{PortProtocol, PublishedPort};
    let (spec, proto) = entry.rsplit_once('/').unwrap_or((entry, "tcp"));
    let protocol = if proto.eq_ignore_ascii_case("udp") {
        PortProtocol::Udp
    } else {
        PortProtocol::Tcp
    };
    let mut parts: Vec<&str> = spec.split(':').collect();
    let container: u16 = parts.pop()?.parse().ok()?;
    let host = parts
        .pop()
        .and_then(|h| h.parse().ok())
        .unwrap_or(container);
    Some(PublishedPort {
        host,
        container,
        protocol,
    })
}

/// Refuses a compose file where any service bind-mounts a host path outside
/// the package directory — the single most important check in this feature.
/// A compose file from an arbitrary repository is not audited the way an
/// `asc.yaml` package is, and nothing else stops it from mounting `/` into a
/// container. Deliberately conservative rather than exhaustively correct:
/// this is a static, install-time check against the compose file's own text,
/// not a runtime sandbox, and a legitimate but unusual mount is refused
/// clearly instead of silently allowed through a parsing gap.
///
/// Parsing follows [`detect`]'s own permissive stance (no
/// `deny_unknown_fields`, a parse failure counts as "no bind mounts found"
/// rather than aborting the install) — compose is a large foreign format,
/// and this check exists to catch a real danger, not to validate the file.
pub(super) fn check_bind_mounts(compose_path: &Path, package_dir: &Path) -> Result<()> {
    let raw = fs::read_to_string(compose_path)
        .with_context(|| format!("cannot read {}", compose_path.display()))?;
    let Ok(doc) = serde_yaml::from_str::<ComposeDoc>(&raw) else {
        return Ok(());
    };
    let compose_dir = compose_path.parent().unwrap_or(package_dir);
    for source in bind_mount_sources(&doc) {
        if escapes_package(&source, package_dir) {
            bail!(
                "compose service mounts host path '{source}' outside the installed package \
                 ({}) — refusing for safety",
                compose_dir.display()
            );
        }
    }
    Ok(())
}

/// Every bind-mount **source** across every service's `volumes:` — short
/// syntax (`./data:/data`, `/abs/path:/data`) and long syntax
/// (`type: bind, source: ./data`). A source with no `/`/`.`/`~` prefix is a
/// named volume (Docker-managed, no host path involved) and is not a bind
/// mount at all; a bare path with no `:` at all is an anonymous volume, same
/// reasoning.
fn bind_mount_sources(doc: &ComposeDoc) -> Vec<String> {
    let mut sources = Vec::new();
    for service in doc.services.values() {
        for volume in &service.volumes {
            match volume {
                serde_yaml::Value::String(entry) => {
                    if let Some((source, _target)) = entry.split_once(':')
                        && is_path_like(source)
                    {
                        sources.push(source.to_string());
                    }
                }
                serde_yaml::Value::Mapping(map) => {
                    let kind = map
                        .get(serde_yaml::Value::String("type".into()))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let source = map
                        .get(serde_yaml::Value::String("source".into()))
                        .and_then(|v| v.as_str());
                    if let Some(source) = source
                        && (kind == "bind" || (kind.is_empty() && is_path_like(source)))
                    {
                        sources.push(source.to_string());
                    }
                }
                _ => {}
            }
        }
    }
    sources
}

fn is_path_like(source: &str) -> bool {
    source.starts_with('.') || source.starts_with('/') || source.starts_with('~')
}

/// A relative source can only stay inside `package_dir` if it never climbs
/// out via `..` (the compose file's own directory is always somewhere under
/// `package_dir` to begin with, so a `..`-free relative path cannot escape
/// it either). An absolute or `~`-relative source is host-rooted by
/// definition and is refused unless — vanishingly unlikely in practice — it
/// literally names a path already inside the package directory.
fn escapes_package(source: &str, package_dir: &Path) -> bool {
    if let Some(rest) = source.strip_prefix('~') {
        let _ = rest;
        return true;
    }
    let path = Path::new(source);
    if path.is_absolute() {
        return !path.starts_with(package_dir);
    }
    path.components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn write(dir: &Path, rel: &str, content: &str) -> PathBuf {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn a_relative_bind_mount_inside_the_package_is_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let compose = write(
            dir.path(),
            "docker-compose.yml",
            "services:\n  web:\n    image: nginx\n    volumes:\n      - ./data:/data\n",
        );
        check_bind_mounts(&compose, dir.path()).unwrap();
    }

    #[test]
    fn a_named_volume_and_an_anonymous_volume_are_not_bind_mounts() {
        let dir = tempfile::tempdir().unwrap();
        let compose = write(
            dir.path(),
            "docker-compose.yml",
            "services:\n  db:\n    image: postgres\n    volumes:\n      - db-data:/var/lib/postgresql/data\n      - /var/lib/anon\n",
        );
        check_bind_mounts(&compose, dir.path()).unwrap();
    }

    #[test]
    fn an_absolute_host_bind_mount_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let compose = write(
            dir.path(),
            "docker-compose.yml",
            "services:\n  web:\n    image: nginx\n    volumes:\n      - /etc:/etc\n",
        );
        let err = check_bind_mounts(&compose, dir.path()).unwrap_err();
        assert!(format!("{err:#}").contains("/etc"), "got: {err:#}");
    }

    #[test]
    fn a_traversal_out_of_the_package_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let compose = write(
            dir.path(),
            "docker-compose.yml",
            "services:\n  web:\n    image: nginx\n    volumes:\n      - ../../etc:/etc\n",
        );
        let err = check_bind_mounts(&compose, dir.path()).unwrap_err();
        assert!(format!("{err:#}").contains("../../etc"), "got: {err:#}");
    }

    #[test]
    fn a_home_relative_bind_mount_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let compose = write(
            dir.path(),
            "docker-compose.yml",
            "services:\n  web:\n    image: nginx\n    volumes:\n      - ~/secrets:/secrets\n",
        );
        check_bind_mounts(&compose, dir.path()).unwrap_err();
    }

    #[test]
    fn the_long_bind_syntax_is_also_checked() {
        let dir = tempfile::tempdir().unwrap();
        let compose = write(
            dir.path(),
            "docker-compose.yml",
            "services:\n  web:\n    image: nginx\n    volumes:\n      - type: bind\n        source: /root\n        target: /mnt\n",
        );
        check_bind_mounts(&compose, dir.path()).unwrap_err();
    }

    #[test]
    fn the_long_volume_syntax_naming_a_named_volume_is_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let compose = write(
            dir.path(),
            "docker-compose.yml",
            "services:\n  db:\n    image: postgres\n    volumes:\n      - type: volume\n        source: db-data\n        target: /var/lib/postgresql/data\n",
        );
        check_bind_mounts(&compose, dir.path()).unwrap();
    }

    #[test]
    fn an_unparseable_compose_file_is_treated_as_having_no_bind_mounts() {
        let dir = tempfile::tempdir().unwrap();
        let compose = write(dir.path(), "docker-compose.yml", "not: [valid, {compose");
        check_bind_mounts(&compose, dir.path()).unwrap();
    }

    #[test]
    fn published_ports_reads_short_syntax_across_services() {
        let dir = tempfile::tempdir().unwrap();
        let compose = write(
            dir.path(),
            "docker-compose.yml",
            "services:\n  web:\n    image: nginx\n    ports:\n      - \"8080:80\"\n      - \"53:53/udp\"\n  db:\n    image: postgres\n    ports:\n      - \"5432\"\n      - \"127.0.0.1:9000:9000\"\n",
        );
        let mut ports = published_ports(&compose);
        ports.sort_by_key(|p| p.host);
        assert_eq!(ports.len(), 4);
        assert!(ports.iter().any(|p| p.host == 8080 && p.container == 80));
        assert!(ports.iter().any(|p| p.host == 53 && p.container == 53));
        assert!(ports.iter().any(|p| p.host == 5432 && p.container == 5432));
        assert!(ports.iter().any(|p| p.host == 9000 && p.container == 9000));
    }

    #[test]
    fn published_ports_on_an_unparseable_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let compose = write(dir.path(), "docker-compose.yml", "not: [valid, {compose");
        assert!(published_ports(&compose).is_empty());
    }
}
