//! Installing a bare Dockerfile with no `asc.yaml` (DMN-107): the manifest
//! and settings a normal install would read from the package repository are
//! synthesized in memory instead — `EXPOSE`/`VOLUME` directives become
//! `type: ports`/`type: volumes` settings, so the freshly installed app
//! actually publishes something instead of looking installed but
//! unreachable.
//!
//! Nothing synthesized is ever written to `repository/`: [`resolve_installed`]
//! re-synthesizes the same manifest on every later read (refresh, upgrade,
//! disk usage, ports, `asc app clone`) from the exact Dockerfile path pinned
//! in `AppMeta.install_method` at install time.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, anyhow};

use super::detect;
use super::manifest::{AppType, ImageBuild, Manifest, RuntimeSpec};
use super::settings::{ContainerPorts, SettingDef, SettingKind, SettingsFile};
use crate::daemon::apps::meta::{AppMeta, InstallMethod};
use crate::daemon::docker::PortProtocol;

/// Locate the Dockerfile a fresh Dockerfile-method install should use: the
/// same deterministic candidate [`detect::detect`] already showed the caller
/// during inspection. Re-run rather than trusted from the request, so a
/// repository that changed between inspect and install (or a caller that
/// skipped inspect) cannot desync the daemon from what it is about to build.
pub(super) fn find(manifest_dir: &Path) -> Result<String> {
    detect::detect(manifest_dir)
        .into_iter()
        .find(|m| m.kind == detect::InstallMethod::Dockerfile)
        .and_then(|m| m.files.into_iter().next())
        .ok_or_else(|| anyhow!("no Dockerfile found in the package"))
}

/// Re-synthesize the manifest and settings of an already-installed
/// Dockerfile app (DMN-107) — the read path every caller other than the
/// install itself must use, since `repository/asc.yaml` never exists for one.
pub fn resolve_installed(
    meta: &AppMeta,
    manifest_dir: &Path,
) -> Result<(Manifest, Option<SettingsFile>)> {
    match &meta.install_method {
        Some(InstallMethod::Dockerfile { dockerfile }) => {
            synthesize(manifest_dir, &meta.id, dockerfile)
        }
        None => {
            let manifest = Manifest::load(manifest_dir)?;
            let settings = SettingsFile::load_for(manifest_dir, &manifest)?;
            Ok((manifest, settings))
        }
    }
}

/// Build the in-memory manifest + settings for a Dockerfile install/re-read.
/// `dockerfile_rel` is relative to `manifest_dir` (the repository root, or
/// its monorepo subdirectory for a `repo_path` install).
pub(super) fn synthesize(
    manifest_dir: &Path,
    id: &str,
    dockerfile_rel: &str,
) -> Result<(Manifest, Option<SettingsFile>)> {
    let dockerfile_path = super::install::safe_join(manifest_dir, dockerfile_rel)?;
    let context_dir = dockerfile_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| manifest_dir.to_path_buf());
    let context_rel = context_dir
        .strip_prefix(manifest_dir)
        .unwrap_or(Path::new(""))
        .to_string_lossy()
        .into_owned();
    let dockerfile_name = dockerfile_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Dockerfile".to_string());

    let manifest = Manifest {
        name: id.to_string(),
        // A bare Dockerfile carries no version of its own — the real
        // version tracking for this app is `AppMeta.version`/`branch` (a git
        // tag or a followed branch); this is only a placeholder to satisfy
        // the manifest schema's required field.
        version: "0.0.0".to_string(),
        app_type: AppType::Docker,
        title: None,
        description: None,
        category: None,
        settings: None,
        runtime: RuntimeSpec {
            image: None,
            image_build: Some(ImageBuild {
                context: (!context_rel.is_empty()).then_some(context_rel),
                dockerfile: Some(dockerfile_name),
                args: Default::default(),
                tag: None,
            }),
            ..Default::default()
        },
        requirements: None,
        healthcheck: None,
        hooks: None,
    };
    manifest
        .validate()
        .context("synthesized Dockerfile manifest")?;

    let settings = synthesize_settings(&dockerfile_path)?;
    if let Some(settings) = &settings {
        settings
            .validate()
            .context("synthesized Dockerfile settings")?;
    }
    Ok((manifest, settings))
}

/// Naive `EXPOSE`/`VOLUME` scan (DMN-107): line-based, no `ARG`/`ENV`
/// expansion. Good enough to make a freshly installed Dockerfile app
/// reachable — the user can always refine the result from the settings form
/// afterward — and, like [`detect`], a directive this doesn't understand is
/// simply not counted rather than failing the install.
fn synthesize_settings(dockerfile: &Path) -> Result<Option<SettingsFile>> {
    let raw = fs::read_to_string(dockerfile)
        .with_context(|| format!("cannot read {}", dockerfile.display()))?;

    let mut tcp_ports: Vec<u16> = Vec::new();
    let mut udp_ports: Vec<u16> = Vec::new();
    let mut volumes: Vec<String> = Vec::new();

    for line in join_line_continuations(&raw) {
        let line = line.split('#').next().unwrap_or("").trim();
        let split = line.find(char::is_whitespace).unwrap_or(line.len());
        let (keyword, rest) = line.split_at(split);
        match keyword.to_ascii_uppercase().as_str() {
            "EXPOSE" => {
                for token in rest.split_whitespace() {
                    let (port_str, proto) = token.split_once('/').unwrap_or((token, "tcp"));
                    let Ok(port) = port_str.parse::<u16>() else {
                        continue;
                    };
                    if port == 0 {
                        continue;
                    }
                    let bucket = match proto.to_ascii_lowercase().as_str() {
                        "udp" => &mut udp_ports,
                        _ => &mut tcp_ports,
                    };
                    if !bucket.contains(&port) {
                        bucket.push(port);
                    }
                }
            }
            "VOLUME" => {
                for path in parse_volume_line(rest) {
                    if path.starts_with('/') && !volumes.contains(&path) {
                        volumes.push(path);
                    }
                }
            }
            _ => {}
        }
    }

    if tcp_ports.is_empty() && udp_ports.is_empty() && volumes.is_empty() {
        return Ok(None);
    }

    let mut settings = Vec::new();
    for (key, title, ports, protocol) in [
        ("ports", "Ports", &tcp_ports, PortProtocol::Tcp),
        ("ports_udp", "Ports (UDP)", &udp_ports, PortProtocol::Udp),
    ] {
        if ports.is_empty() {
            continue;
        }
        settings.push(SettingDef {
            key: key.to_string(),
            kind: SettingKind::Ports,
            title: Some(title.to_string()),
            description: Some("Detected from EXPOSE in the Dockerfile".to_string()),
            default: Some(serde_yaml::to_value(ports).expect("serializing a Vec<u16> cannot fail")),
            required: false,
            values: Vec::new(),
            allow_custom: false,
            limits: None,
            env: None,
            protocol: Some(protocol),
            container: Some(ContainerPorts::Many(ports.clone())),
        });
    }
    if !volumes.is_empty() {
        // A bare "/container/path" defaults to the app's shared `data/`
        // folder (see `install::parse_volume`) — fine for one volume, but
        // several would silently collide into that same folder, so anything
        // past the first gets its own disambiguating subfolder.
        let entries: Vec<String> = if volumes.len() == 1 {
            volumes.clone()
        } else {
            volumes
                .iter()
                .enumerate()
                .map(|(i, path)| format!("{path}:vol{}", i + 1))
                .collect()
        };
        settings.push(SettingDef {
            key: "volumes".to_string(),
            kind: SettingKind::Volumes,
            title: Some("Volumes".to_string()),
            description: Some("Detected from VOLUME in the Dockerfile".to_string()),
            default: Some(
                serde_yaml::to_value(&entries).expect("serializing a Vec<String> cannot fail"),
            ),
            required: false,
            values: Vec::new(),
            allow_custom: false,
            limits: None,
            env: None,
            protocol: None,
            container: None,
        });
    }

    Ok(Some(SettingsFile {
        quota: None,
        settings,
        start_command: None,
    }))
}

/// Docker's own line-continuation rule: a trailing (whitespace-trimmed) `\`
/// joins the next line on, with a space in between.
fn join_line_continuations(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::new();
    for line in raw.lines() {
        let trimmed_end = line.trim_end();
        match trimmed_end.strip_suffix('\\') {
            Some(part) => {
                buf.push_str(part);
                buf.push(' ');
            }
            None => {
                buf.push_str(line);
                out.push(std::mem::take(&mut buf));
            }
        }
    }
    if !buf.is_empty() {
        out.push(buf);
    }
    out
}

/// `VOLUME /data /logs` (shell form) or `VOLUME ["/data", "/logs"]` (exec/JSON
/// form) — both are valid Dockerfile syntax.
fn parse_volume_line(rest: &str) -> Vec<String> {
    let trimmed = rest.trim();
    if let Some(inner) = trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        inner
            .split(',')
            .map(|s| s.trim().trim_matches(['"', '\'']).to_string())
            .filter(|s| !s.is_empty())
            .collect()
    } else {
        trimmed.split_whitespace().map(str::to_string).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, rel: &str, content: &str) {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, content).unwrap();
    }

    #[test]
    fn root_dockerfile_with_no_expose_or_volume_yields_no_settings() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Dockerfile", "FROM alpine\nCMD [\"true\"]\n");
        let (manifest, settings) = synthesize(dir.path(), "myapp", "Dockerfile").unwrap();
        assert_eq!(manifest.name, "myapp");
        assert!(manifest.runtime.image.is_none());
        assert_eq!(
            manifest
                .runtime
                .image_build
                .as_ref()
                .unwrap()
                .dockerfile
                .as_deref(),
            Some("Dockerfile")
        );
        assert!(
            manifest
                .runtime
                .image_build
                .as_ref()
                .unwrap()
                .context
                .is_none()
        );
        manifest.validate().unwrap();
        assert!(settings.is_none());
    }

    #[test]
    fn expose_lines_become_a_ports_setting_paired_with_container_ports() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "Dockerfile",
            "FROM alpine\nEXPOSE 8080\nEXPOSE 53/udp 9000/tcp\n",
        );
        let (_, settings) = synthesize(dir.path(), "myapp", "Dockerfile").unwrap();
        let settings = settings.unwrap();
        let tcp = settings.settings.iter().find(|s| s.key == "ports").unwrap();
        assert_eq!(tcp.container_ports(), &[8080, 9000]);
        assert_eq!(tcp.port_protocol(), PortProtocol::Tcp);
        let udp = settings
            .settings
            .iter()
            .find(|s| s.key == "ports_udp")
            .unwrap();
        assert_eq!(udp.container_ports(), &[53]);
        assert_eq!(udp.port_protocol(), PortProtocol::Udp);
    }

    #[test]
    fn expose_ignores_port_zero_and_garbage_tokens() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "Dockerfile",
            "FROM alpine\nEXPOSE 0 not-a-port 80\n",
        );
        let (_, settings) = synthesize(dir.path(), "myapp", "Dockerfile").unwrap();
        let settings = settings.unwrap();
        let tcp = settings.settings.iter().find(|s| s.key == "ports").unwrap();
        assert_eq!(tcp.container_ports(), &[80]);
    }

    #[test]
    fn a_single_volume_defaults_to_the_shared_data_folder() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Dockerfile", "FROM alpine\nVOLUME /data\n");
        let (_, settings) = synthesize(dir.path(), "myapp", "Dockerfile").unwrap();
        let settings = settings.unwrap();
        let vol = settings
            .settings
            .iter()
            .find(|s| s.key == "volumes")
            .unwrap();
        assert_eq!(
            vol.default.as_ref().unwrap().as_sequence().unwrap()[0]
                .as_str()
                .unwrap(),
            "/data"
        );
    }

    #[test]
    fn several_volumes_get_disambiguating_subfolders() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "Dockerfile",
            "FROM alpine\nVOLUME [\"/data\", \"/logs\"]\n",
        );
        let (_, settings) = synthesize(dir.path(), "myapp", "Dockerfile").unwrap();
        let settings = settings.unwrap();
        let vol = settings
            .settings
            .iter()
            .find(|s| s.key == "volumes")
            .unwrap();
        let items: Vec<String> = vol
            .default
            .as_ref()
            .unwrap()
            .as_sequence()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(items, vec!["/data:vol1", "/logs:vol2"]);
        for item in &items {
            super::super::install::validate_volume(item).unwrap();
        }
    }

    #[test]
    fn a_line_continuation_is_joined_before_parsing() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "Dockerfile",
            "FROM alpine\nVOLUME /data \\\n      /logs\n",
        );
        let (_, settings) = synthesize(dir.path(), "myapp", "Dockerfile").unwrap();
        let settings = settings.unwrap();
        let vol = settings
            .settings
            .iter()
            .find(|s| s.key == "volumes")
            .unwrap();
        assert_eq!(
            vol.default.as_ref().unwrap().as_sequence().unwrap().len(),
            2
        );
    }

    #[test]
    fn a_nested_dockerfile_records_its_directory_as_the_build_context() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "docker/Dockerfile.prod", "FROM alpine\n");
        let dockerfile_rel = "docker/Dockerfile.prod".replace('/', std::path::MAIN_SEPARATOR_STR);
        let (manifest, _) = synthesize(dir.path(), "myapp", &dockerfile_rel).unwrap();
        let build = manifest.runtime.image_build.unwrap();
        assert_eq!(build.dockerfile.as_deref(), Some("Dockerfile.prod"));
        assert_eq!(build.context.as_deref(), Some("docker"));
    }

    #[test]
    fn resolve_installed_falls_back_to_a_normal_manifest_load_without_install_method() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "asc.yaml",
            "name: app\nversion: '1.0'\ntype: docker\nruntime:\n  image: app:1\n",
        );
        let meta = AppMeta {
            id: "app".to_string(),
            uuid: None,
            name: "app".to_string(),
            custom_name: None,
            owner: crate::daemon::apps::meta::Owner {
                uid: 0,
                name: "root".to_string(),
            },
            version: None,
            source: None,
            branch: None,
            repo_path: None,
            package: None,
            install_method: None,
            desired_state: crate::daemon::apps::meta::DesiredState::Stopped,
            quota: None,
            runtime: crate::daemon::apps::meta::Runtime::Docker {
                container: "asc-app".to_string(),
                image_source: None,
            },
        };
        let (manifest, settings) = resolve_installed(&meta, dir.path()).unwrap();
        assert_eq!(manifest.name, "app");
        assert!(settings.is_none());
    }
}
