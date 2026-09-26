//! Applying configuration (DMN-122/DMN-123): resolve every site, render a
//! complete staging copy, let `nginx -t` judge it, then swap the live files
//! and reload.
//!
//! One bad site must not take the others down. When `nginx -t` names a
//! site's file, that site alone is set aside — its previous live file is
//! kept if it had one, otherwise it is left out — and the check runs again.
//! Only a failure nobody can be blamed for (the node-wide configuration)
//! aborts the whole apply, leaving every live file untouched.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

use super::model::{Mode, Settings, Site, SiteState, Target, TlsMode, TlsState};
use super::render::{self, Features, Paths, Resolved};
use super::{StatusFile, WebServer, cloudflare, engine, unix_now, write_atomic};
use crate::daemon::apps::{AppManager, UserContext};

/// How far an apply goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Render and `nginx -t` only.
    TestOnly,
    /// Write the live files without testing or reloading — the docker
    /// install, where nginx does not run yet.
    WriteOnly,
    /// Test, swap, reload.
    Full,
}

/// One site's rendered output.
struct Rendered {
    id: String,
    file: String,
    /// Provided certificate files (relative path → content).
    certs: Vec<(PathBuf, String)>,
    addresses: Vec<String>,
}

fn root_ctx() -> UserContext {
    UserContext {
        uid: 0,
        name: "root".into(),
        is_root: true,
    }
}

/// Which site, if any, `nginx -t` output blames — by the path of its file
/// or of its certificate directory under `base`.
pub fn blamed_site(output: &str, base: &Path, candidates: &[String]) -> Option<String> {
    let sites = base.join("sites").display().to_string();
    let certs = base.join("certs").display().to_string();
    candidates
        .iter()
        .find(|id| {
            output.contains(&format!("{sites}/{id}.conf"))
                || output.contains(&format!("{certs}/{id}/"))
        })
        .cloned()
}

fn trim_output(output: &str) -> String {
    let lines: Vec<&str> = output
        .lines()
        .filter(|l| !l.contains("the configuration file") || l.contains("failed"))
        .collect();
    let text = lines.join("\n");
    if text.len() > 4000 {
        format!("{}…", &text[..text.floor_char_boundary(4000)])
    } else {
        text
    }
}

impl WebServer {
    pub(super) fn features(&self, settings: &Settings) -> Features {
        let version = self.engine(settings.mode).state().version;
        Features::for_version(version.as_deref(), engine::host_has_ipv6())
    }

    /// Upstream addresses and certificate paths of one site.
    pub(super) fn resolve<'a>(&self, site: &'a Site, _status: &StatusFile) -> Result<Resolved<'a>> {
        let manager = AppManager::new(&self.config);
        let ctx = root_ctx();
        let mut addresses = Vec::with_capacity(site.upstream.servers.len());
        if site.raw_config.is_none() {
            for server in &site.upstream.servers {
                addresses.push(match &server.target {
                    Target::Address { address } => address.clone(),
                    Target::App { app, port } => {
                        let meta = match manager.get_authorized(&ctx, app) {
                            Ok(meta) => meta,
                            Err(_) => manager
                                .store()
                                .list()?
                                .into_iter()
                                .find(|m| m.uuid.as_deref() == Some(app.as_str()))
                                .ok_or_else(|| {
                                    anyhow!("app {app:?} is not installed on this node")
                                })?,
                        };
                        let ports = crate::daemon::apps::ports::published(
                            &self.config,
                            manager.store(),
                            &meta,
                        )?;
                        let host = ports
                            .iter()
                            .find(|p| p.container == *port)
                            .or_else(|| ports.iter().find(|p| p.host == *port))
                            .map(|p| p.host)
                            .ok_or_else(|| anyhow!("app {app:?} does not publish port {port}"))?;
                        format!("127.0.0.1:{host}")
                    }
                });
            }
        }
        let cert = match site.tls.mode {
            TlsMode::None => None,
            TlsMode::Provided => {
                let (c, k) = render::provided_cert_files(&site.id);
                Some((c, k))
            }
            TlsMode::Acme => self.acme_material(&site.id),
        };
        let down = super::health::down_mask(&self.health, &site.id, &addresses);
        Ok(Resolved {
            site,
            addresses,
            cert,
            down,
        })
    }

    /// See the module docs. On return `status` reflects every site; the
    /// caller saves it. `Ok` carries the `nginx -t` output.
    pub(super) fn apply_locked(
        &self,
        settings: &Settings,
        sites: &[Site],
        status: &mut StatusFile,
        stage: Stage,
    ) -> Result<String> {
        if settings.mode == Mode::None {
            bail!("the web server is not installed");
        }
        let docker = settings.mode == Mode::Docker;
        let features = self.features(settings);
        let ranges = cloudflare::Ranges::load(&self.paths.join_state_cloudflare());
        let live_root = self.paths.root.clone();
        let staging = self.paths.staging();
        let now = unix_now();

        // Resolve. Local sites claim their names first, then the rest in id
        // order, so a clash always rejects the same site.
        let mut ordered: Vec<&Site> = sites.iter().collect();
        ordered.sort_by(|a, b| (!a.is_local(), &a.id).cmp(&(!b.is_local(), &b.id)));
        let mut claimed: HashMap<String, String> = HashMap::new();
        let mut candidates: Vec<(Resolved<'_>, bool)> = Vec::new();
        for site in ordered {
            let st = status.sites.entry(site.id.clone()).or_default();
            if site.disabled {
                st.state = SiteState::Disabled;
                st.message.clear();
                continue;
            }
            if let Some(owner) = site
                .server_names
                .iter()
                .find_map(|n| claimed.get(n).map(|o| (n, o)))
            {
                st.state = SiteState::Error;
                st.message = format!("{} is already served by site {}", owner.0, owner.1);
                continue;
            }
            match self.resolve(site, status) {
                Ok(resolved) => {
                    for name in &site.server_names {
                        claimed.insert(name.clone(), site.id.clone());
                    }
                    let st = status.sites.entry(site.id.clone()).or_default();
                    match site.tls.mode {
                        TlsMode::None => st.tls = Default::default(),
                        TlsMode::Provided => {
                            match super::cert::inspect(&site.tls.certificate_pem) {
                                Ok(info) => {
                                    st.tls.state = if info.not_after <= now {
                                        TlsState::Error
                                    } else {
                                        TlsState::Active
                                    };
                                    st.tls.last_error = if info.not_after <= now {
                                        "the provided certificate has expired".into()
                                    } else {
                                        String::new()
                                    };
                                    st.tls.not_after = info.not_after;
                                    st.tls.issuer = info.issuer;
                                    st.tls.names = info.names;
                                }
                                Err(err) => {
                                    st.tls.state = TlsState::Error;
                                    st.tls.last_error = format!("{err:#}");
                                }
                            }
                        }
                        TlsMode::Acme => {
                            if resolved.cert.is_none() && st.tls.state == TlsState::None {
                                st.tls.state = TlsState::PendingDns;
                            }
                        }
                    }
                    candidates.push((resolved, false));
                }
                Err(err) => {
                    let st = status.sites.entry(site.id.clone()).or_default();
                    st.state = SiteState::Error;
                    st.message = format!("{err:#}");
                }
            }
        }

        let live_sites: HashMap<String, String> = candidates
            .iter()
            .filter_map(|(r, _)| {
                std::fs::read_to_string(live_root.join(render::site_file(&r.site.id)))
                    .ok()
                    .map(|text| (r.site.id.clone(), text))
            })
            .collect();

        // Render → test, setting blamed sites aside until nginx agrees.
        let global =
            render::render_global(settings, features, &ranges, &self.paths, &staging, docker);
        let mut rejected: HashSet<String> = HashSet::new();
        let mut kept_live: HashSet<String> = HashSet::new();
        let mut messages: HashMap<String, String> = HashMap::new();
        let test_output = loop {
            let rendered = render_sites(&candidates, &rejected, settings, features, &staging);
            write_tree(&staging, &global, &rendered, &kept_live, &live_sites)?;
            if stage == Stage::WriteOnly {
                break String::new();
            }
            let conf = staging.join(render::NGINX_CONF);
            match self.engine(settings.mode).test(&conf)? {
                Ok(output) => break output,
                Err(output) => {
                    let pending: Vec<String> = candidates
                        .iter()
                        .map(|(r, _)| r.site.id.clone())
                        .filter(|id| !rejected.contains(id))
                        .collect();
                    match blamed_site(&output, &staging, &pending) {
                        Some(id) => {
                            // The first rejection falls back to the live
                            // file; a live file that fails too is dropped.
                            if live_sites.contains_key(&id) && !kept_live.contains(&id) {
                                kept_live.insert(id.clone());
                            } else {
                                kept_live.remove(&id);
                                rejected.insert(id.clone());
                            }
                            messages.entry(id).or_insert_with(|| trim_output(&output));
                        }
                        None => {
                            let _ = std::fs::remove_dir_all(&staging);
                            status.last_error = trim_output(&output);
                            bail!(
                                "nginx rejected the configuration:\n{}",
                                trim_output(&output)
                            );
                        }
                    }
                }
            }
        };

        if stage == Stage::TestOnly {
            let _ = std::fs::remove_dir_all(&staging);
            return Ok(test_output);
        }

        // Swap: render once more against the live root and write it.
        let global_live =
            render::render_global(settings, features, &ranges, &self.paths, &live_root, docker);
        let rendered_live = render_sites(&candidates, &rejected, settings, features, &live_root);
        let main_conf = engine::main_conf_path(settings.mode, &self.paths);
        for (rel, content) in &global_live {
            let target = if rel.as_path() == Path::new(render::NGINX_CONF) {
                main_conf.clone()
            } else {
                live_root.join(rel)
            };
            write_atomic(&target, content.as_bytes(), 0o644)?;
        }
        let mut keep_files: HashSet<PathBuf> = HashSet::new();
        let mut keep_cert_dirs: HashSet<String> = HashSet::new();
        for r in &rendered_live {
            if kept_live.contains(&r.id) {
                keep_files.insert(live_root.join(render::site_file(&r.id)));
                keep_cert_dirs.insert(r.id.clone());
                continue;
            }
            for (rel, pem) in &r.certs {
                let mode = if rel.ends_with("privkey.pem") {
                    0o600
                } else {
                    0o644
                };
                write_atomic(&live_root.join(rel), pem.as_bytes(), mode)?;
            }
            if !r.certs.is_empty() {
                keep_cert_dirs.insert(r.id.clone());
            }
            let path = live_root.join(render::site_file(&r.id));
            write_atomic(&path, r.file.as_bytes(), 0o644)?;
            keep_files.insert(path);
        }
        prune(&live_root.join("sites"), |p| keep_files.contains(p));
        prune(&live_root.join("certs"), |p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| keep_cert_dirs.contains(n))
        });
        let _ = std::fs::remove_dir_all(&staging);

        if stage == Stage::Full
            && let Err(err) = self.engine(settings.mode).reload()
        {
            status.last_error = format!("{err:#}");
            return Err(err);
        }

        // Statuses.
        let applied: HashMap<&str, &Rendered> =
            rendered_live.iter().map(|r| (r.id.as_str(), r)).collect();
        for (resolved, _) in &candidates {
            let id = resolved.site.id.as_str();
            let st = status.sites.entry(id.to_string()).or_default();
            let message = messages.get(id).cloned();
            if rejected.contains(id) {
                st.state = SiteState::Error;
                st.message = message.unwrap_or_default();
            } else if kept_live.contains(id) {
                st.state = SiteState::Error;
                st.message = format!(
                    "nginx rejected the new configuration; the previous one keeps serving:\n{}",
                    message.unwrap_or_default()
                );
            } else if let Some(r) = applied.get(id) {
                st.state = SiteState::Applied;
                st.message.clear();
                st.applied_at = now;
                st.upstream_addresses = r.addresses.clone();
            }
        }
        status.last_error.clear();
        status.last_applied = now;
        Ok(test_output)
    }
}

impl Paths {
    fn join_state_cloudflare(&self) -> PathBuf {
        self.state.join(super::CLOUDFLARE_FILE)
    }
}

fn render_sites(
    candidates: &[(Resolved<'_>, bool)],
    rejected: &HashSet<String>,
    settings: &Settings,
    features: Features,
    base: &Path,
) -> Vec<Rendered> {
    candidates
        .iter()
        .filter(|(r, _)| !rejected.contains(&r.site.id))
        .map(|(r, _)| {
            let site = r.site;
            let mut resolved = r.clone();
            let mut certs = Vec::new();
            if site.tls.mode == TlsMode::Provided {
                let (c, k) = render::provided_cert_files(&site.id);
                certs.push((c.clone(), site.tls.certificate_pem.clone()));
                certs.push((k.clone(), site.tls.private_key_pem.clone()));
                resolved.cert = Some((base.join(c), base.join(k)));
            }
            Rendered {
                id: site.id.clone(),
                file: render::render_site(&resolved, settings, features, base),
                certs,
                addresses: r.addresses.clone(),
            }
        })
        .collect()
}

/// Writes a complete staging tree. Sites kept at their live version are
/// staged from the live file (which refers to live paths — those exist).
fn write_tree(
    base: &Path,
    global: &render::Files,
    sites: &[Rendered],
    kept_live: &HashSet<String>,
    live_sites: &HashMap<String, String>,
) -> Result<()> {
    let _ = std::fs::remove_dir_all(base);
    std::fs::create_dir_all(base.join("sites"))
        .with_context(|| format!("cannot create {}", base.display()))?;
    for (rel, content) in global {
        write_atomic(&base.join(rel), content.as_bytes(), 0o644)?;
    }
    for site in sites {
        let file = if kept_live.contains(&site.id) {
            live_sites.get(&site.id).cloned().unwrap_or_default()
        } else {
            for (rel, pem) in &site.certs {
                write_atomic(&base.join(rel), pem.as_bytes(), 0o600)?;
            }
            site.file.clone()
        };
        write_atomic(
            &base.join(render::site_file(&site.id)),
            file.as_bytes(),
            0o644,
        )?;
    }
    Ok(())
}

/// Removes entries of `dir` that `keep` rejects.
fn prune(dir: &Path, keep: impl Fn(&Path) -> bool) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if keep(&path) {
            continue;
        }
        let _ = if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blame_follows_file_paths() {
        let base = Path::new("/etc/asc/webserver/.staging");
        let ids = vec!["a".to_string(), "b".to_string()];
        let out = "nginx: [emerg] unknown directive \"foo\" in /etc/asc/webserver/.staging/sites/b.conf:12";
        assert_eq!(blamed_site(out, base, &ids).as_deref(), Some("b"));
        let out = "nginx: [emerg] cannot load certificate \"/etc/asc/webserver/.staging/certs/a/fullchain.pem\"";
        assert_eq!(blamed_site(out, base, &ids).as_deref(), Some("a"));
        let out =
            "nginx: [emerg] unknown directive \"x\" in /etc/asc/webserver/.staging/nginx.conf:3";
        assert_eq!(blamed_site(out, base, &ids), None);
    }

    #[test]
    fn output_is_trimmed() {
        let out = "nginx: the configuration file /x syntax is ok\nnginx: [emerg] boom";
        assert_eq!(trim_output(out), "nginx: [emerg] boom");
    }
}
