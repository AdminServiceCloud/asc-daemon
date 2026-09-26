//! Driving nginx itself (DMN-122): install or adopt it, test a
//! configuration, reload, report its state — in `system` mode (package +
//! systemd) or `docker` mode (the `asc-webserver` container on the host
//! network). Blocking; the manager calls this from worker threads.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;

use anyhow::{Context, Result, bail};

use super::model::{HostProfile, Mode, Settings};
use super::render::Paths;
use crate::daemon::config::DockerConfig;
use crate::daemon::docker;

pub const CONTAINER: &str = "asc-webserver";
const SYSTEM_CONF: &str = "/etc/nginx/nginx.conf";
const BACKUP_SUFFIX: &str = ".asc-orig";

const APT_KEYRING: &str = "/etc/apt/keyrings/nginx-asc.asc";
const APT_LIST: &str = "/etc/apt/sources.list.d/nginx-asc.list";
const APT_PIN: &str = "/etc/apt/preferences.d/99nginx-asc";
const YUM_REPO: &str = "/etc/yum.repos.d/nginx-asc.repo";
const NGINX_KEY_URL: &str = "https://nginx.org/keys/nginx_signing.key";

/// Progress sink for long operations: one human line at a time.
pub type Progress<'a> = &'a mut dyn FnMut(&str);

/// Where the main `nginx.conf` lives for a mode.
pub fn main_conf_path(mode: Mode, paths: &Paths) -> PathBuf {
    match mode {
        Mode::Docker => paths.root.join(super::render::NGINX_CONF),
        _ => PathBuf::from(SYSTEM_CONF),
    }
}

// ── Host facts ──────────────────────────────────────────────────────────────

/// The nginx binary on this host, if any.
pub fn nginx_binary() -> Option<PathBuf> {
    let from_path = std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join("nginx"))
            .find(|candidate| candidate.is_file())
    });
    from_path.or_else(|| {
        [
            "/usr/sbin/nginx",
            "/usr/local/sbin/nginx",
            "/usr/local/nginx/sbin/nginx",
        ]
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
    })
}

/// `nginx -v` prints `nginx version: nginx/1.24.0 (Ubuntu)` to stderr.
pub fn parse_version_output(output: &str) -> Option<String> {
    let rest = output.split("nginx/").nth(1)?;
    let version: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    (!version.is_empty()).then_some(version)
}

/// IPv6 is usable when the kernel has any inet6 address at all.
pub fn host_has_ipv6() -> bool {
    std::fs::read_to_string("/proc/net/if_inet6").is_ok_and(|s| !s.trim().is_empty())
}

#[derive(Debug, Default, Clone)]
struct OsRelease {
    id: String,
    id_like: String,
    codename: String,
}

fn os_release() -> OsRelease {
    let text = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
    let mut out = OsRelease::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches('"').to_ascii_lowercase();
        match key.trim() {
            "ID" => out.id = value,
            "ID_LIKE" => out.id_like = value,
            "VERSION_CODENAME" => out.codename = value,
            "UBUNTU_CODENAME" if out.codename.is_empty() => out.codename = value,
            _ => {}
        }
    }
    out
}

impl OsRelease {
    fn is(&self, family: &str) -> bool {
        self.id == family || self.id_like.split_whitespace().any(|f| f == family)
    }
}

/// Reads what the generated `nginx.conf` must keep from the one the
/// distribution shipped: worker user, pid file, dynamic modules, the
/// operator's include directories — and whether those already declare a
/// `default_server`, in which case ours would collide.
pub fn host_profile(original_conf: &str) -> HostProfile {
    let directive = |name: &str| {
        original_conf.lines().find_map(|line| {
            let line = line.trim();
            let rest = line.strip_prefix(name)?;
            rest.starts_with(char::is_whitespace).then(|| {
                rest.trim()
                    .trim_end_matches(';')
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_string()
            })
        })
    };
    let user = directive("user").unwrap_or_else(|| {
        if user_exists("nginx") {
            "nginx".into()
        } else {
            "www-data".into()
        }
    });
    let pid = directive("pid").unwrap_or_else(|| "/run/nginx.pid".into());
    let dir_has_entries =
        |dir: &str| std::fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_some());
    let include_conf_d = Path::new("/etc/nginx/conf.d").is_dir();
    let include_sites_enabled = Path::new("/etc/nginx/sites-enabled").is_dir();
    let mut foreign_default_http = false;
    let mut foreign_default_https = false;
    let mut scan = |dir: &str| {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(text) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            let (http, https) = default_server_ports(&text);
            foreign_default_http |= http;
            foreign_default_https |= https;
        }
    };
    if include_conf_d {
        scan("/etc/nginx/conf.d");
    }
    if include_sites_enabled {
        scan("/etc/nginx/sites-enabled");
    }
    HostProfile {
        user,
        pid,
        include_modules: dir_has_entries("/etc/nginx/modules-enabled"),
        include_conf_d,
        include_sites_enabled,
        foreign_default_http,
        foreign_default_https,
        adopted: false,
        nginx_org_repo: false,
    }
}

/// Whether a config declares `listen … default_server` on 80 and/or 443.
pub fn default_server_ports(text: &str) -> (bool, bool) {
    let mut http = false;
    let mut https = false;
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if !line.starts_with("listen") || !line.contains("default_server") {
            continue;
        }
        let addr = line
            .trim_start_matches("listen")
            .split_whitespace()
            .next()
            .unwrap_or("");
        let port = addr.rsplit(':').next().unwrap_or(addr);
        match port {
            "80" => http = true,
            "443" => https = true,
            _ => {}
        }
    }
    (http, https)
}

fn user_exists(name: &str) -> bool {
    std::fs::read_to_string("/etc/passwd")
        .is_ok_and(|p| p.lines().any(|l| l.split(':').next() == Some(name)))
}

// ── Commands ────────────────────────────────────────────────────────────────

/// Runs a command, forwarding every output line to `progress`.
fn run_streaming(cmd: &str, args: &[&str], progress: Progress<'_>) -> Result<()> {
    progress(&format!("$ {cmd} {}", args.join(" ")));
    let mut child = Command::new(cmd)
        .args(args)
        .env("DEBIAN_FRONTEND", "noninteractive")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("cannot run {cmd}"))?;
    let (tx, rx) = mpsc::channel::<String>();
    let mut readers = Vec::new();
    if let Some(out) = child.stdout.take() {
        let tx = tx.clone();
        readers.push(std::thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        }));
    }
    if let Some(err) = child.stderr.take() {
        let tx = tx.clone();
        readers.push(std::thread::spawn(move || {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        }));
    }
    drop(tx);
    for line in rx {
        if !line.trim().is_empty() {
            progress(&line);
        }
    }
    for reader in readers {
        let _ = reader.join();
    }
    let status = child
        .wait()
        .with_context(|| format!("{cmd} did not finish"))?;
    if !status.success() {
        bail!("{cmd} {} failed with {status}", args.join(" "));
    }
    Ok(())
}

/// Runs a command and returns (success, stdout+stderr).
fn run_captured(cmd: &str, args: &[&str]) -> Result<(bool, String)> {
    let out = Command::new(cmd)
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("cannot run {cmd}"))?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    Ok((out.status.success(), text))
}

fn has_command(name: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(name).is_file()))
}

// ── Engine ──────────────────────────────────────────────────────────────────

/// The nginx of one mode.
pub struct Engine<'a> {
    pub mode: Mode,
    pub docker: &'a DockerConfig,
    pub paths: &'a Paths,
}

/// What the engine reports about the running server.
#[derive(Debug, Clone, Default)]
pub struct EngineState {
    pub version: Option<String>,
    pub running: bool,
}

impl Engine<'_> {
    pub fn state(&self) -> EngineState {
        match self.mode {
            Mode::None => EngineState::default(),
            Mode::System => {
                let version = nginx_binary().and_then(|bin| {
                    run_captured(&bin.display().to_string(), &["-v"])
                        .ok()
                        .and_then(|(_, out)| parse_version_output(&out))
                });
                let running = run_captured("systemctl", &["is-active", "--quiet", "nginx"])
                    .is_ok_and(|(ok, _)| ok);
                EngineState { version, running }
            }
            Mode::Docker => {
                let running = docker::running(self.docker, CONTAINER).unwrap_or(false);
                let version = running
                    .then(|| {
                        docker::exec_collect(self.docker, CONTAINER, &["nginx".into(), "-v".into()])
                            .ok()
                            .and_then(|(_, out)| parse_version_output(&out))
                    })
                    .flatten();
                EngineState { version, running }
            }
        }
    }

    /// `nginx -t -c <conf>`: Ok(output) when nginx accepts it, Err(output)
    /// with nginx's own words otherwise.
    pub fn test(&self, conf: &Path) -> Result<std::result::Result<String, String>> {
        let conf = conf.display().to_string();
        let (ok, output) = match self.mode {
            Mode::None => bail!("the web server is not installed"),
            Mode::System => {
                let bin = nginx_binary().context("nginx is not installed")?;
                run_captured(&bin.display().to_string(), &["-t", "-c", &conf])?
            }
            Mode::Docker => {
                if !docker::running(self.docker, CONTAINER).unwrap_or(false) {
                    docker::start(self.docker, CONTAINER)?;
                }
                let (code, output) = docker::exec_collect(
                    self.docker,
                    CONTAINER,
                    &["nginx".into(), "-t".into(), "-c".into(), conf],
                )?;
                (code == 0, output)
            }
        };
        Ok(if ok { Ok(output) } else { Err(output) })
    }

    /// Graceful reload; starts the server when it is not running.
    pub fn reload(&self) -> Result<()> {
        match self.mode {
            Mode::None => bail!("the web server is not installed"),
            Mode::System => {
                let (ok, out) = run_captured("systemctl", &["reload-or-restart", "nginx"])?;
                if !ok {
                    bail!("systemctl reload-or-restart nginx failed: {}", out.trim());
                }
                Ok(())
            }
            Mode::Docker => {
                if docker::running(self.docker, CONTAINER).unwrap_or(false) {
                    docker::signal(self.docker, CONTAINER, "HUP")
                } else {
                    docker::start(self.docker, CONTAINER)
                }
            }
        }
    }

    /// Installs (or adopts) nginx. The generated configuration must already
    /// be in place for docker mode — the container starts on it.
    pub fn install(&self, settings: &mut Settings, progress: Progress<'_>) -> Result<()> {
        match self.mode {
            Mode::None => bail!("choose a mode: system or docker"),
            Mode::System => self.install_system(settings, progress),
            Mode::Docker => self.install_docker(settings, progress),
        }
    }

    fn install_system(&self, settings: &mut Settings, progress: Progress<'_>) -> Result<()> {
        let adopted = nginx_binary().is_some();
        let mut nginx_org_repo = false;
        if adopted {
            progress("nginx is already installed — adopting it");
        } else {
            let os = os_release();
            if os.is("debian") || os.is("ubuntu") {
                nginx_org_repo = install_apt(&os, progress)?;
            } else if os.is("rhel") || os.is("centos") || os.is("fedora") {
                nginx_org_repo = install_dnf(&os, progress)?;
            } else {
                bail!(
                    "unsupported distribution {:?}: install nginx with the system package \
                     manager, then run the install again to adopt it",
                    os.id
                );
            }
        }
        let conf = Path::new(SYSTEM_CONF);
        let original = std::fs::read_to_string(conf).unwrap_or_default();
        let backup = PathBuf::from(format!("{SYSTEM_CONF}{BACKUP_SUFFIX}"));
        if !backup.exists() && !original.contains("Generated by asc-daemon") {
            std::fs::write(&backup, &original)
                .with_context(|| format!("cannot back up {SYSTEM_CONF}"))?;
            progress(&format!(
                "saved the original configuration to {}",
                backup.display()
            ));
        }
        let source = if original.contains("Generated by asc-daemon") {
            std::fs::read_to_string(&backup).unwrap_or_default()
        } else {
            original
        };
        let mut profile = host_profile(&source);
        profile.adopted = adopted;
        profile.nginx_org_repo = nginx_org_repo;
        settings.host = profile;
        selinux_allow_proxy(progress);
        firewall_hint(progress);
        let (ok, out) = run_captured("systemctl", &["enable", "nginx"])?;
        if !ok {
            progress(&format!("warning: systemctl enable nginx: {}", out.trim()));
        }
        Ok(())
    }

    fn install_docker(&self, settings: &mut Settings, progress: Progress<'_>) -> Result<()> {
        if !docker::available(self.docker) {
            bail!(
                "Docker is not reachable at {}",
                self.docker.socket.display()
            );
        }
        docker::remove(self.docker, CONTAINER)?;
        for port in [80u16, 443] {
            if std::net::TcpListener::bind(("0.0.0.0", port)).is_err() {
                bail!(
                    "port {port} is already in use on this host — stop whatever listens there first"
                );
            }
        }
        let image = settings.image().to_string();
        progress(&format!("creating container {CONTAINER} from {image}"));
        let conf = main_conf_path(Mode::Docker, self.paths)
            .display()
            .to_string();
        let acme_certs = self.paths.state.join("acme").join("certs");
        std::fs::create_dir_all(&acme_certs)
            .with_context(|| format!("cannot create {}", acme_certs.display()))?;
        let bind = |p: &Path| format!("{0}:{0}:ro", p.display());
        let spec = docker::ServiceContainerSpec {
            name: CONTAINER,
            image: &image,
            // nginx itself as PID 1: the image's entrypoint scripts rewrite
            // a default.conf nobody reads here, and a SIGHUP arriving while
            // they still run would hit the shell instead of nginx.
            entrypoint: Some(vec!["nginx".into()]),
            cmd: vec!["-g".into(), "daemon off;".into(), "-c".into(), conf],
            binds: vec![
                bind(&self.paths.root),
                bind(&self.paths.webroot),
                bind(&acme_certs),
            ],
            labels: HashMap::from([("asc.managed".to_string(), "webserver".to_string())]),
        };
        docker::create_host_service(self.docker, &spec)?;
        docker::start(self.docker, CONTAINER)?;
        settings.host = HostProfile::default();
        progress("container started");
        Ok(())
    }

    /// Removes what install added. An adopted nginx gets its original
    /// configuration back and stays installed.
    pub fn uninstall(&self, settings: &Settings, progress: Progress<'_>) -> Result<()> {
        match self.mode {
            Mode::None => Ok(()),
            Mode::Docker => {
                docker::remove(self.docker, CONTAINER)?;
                progress("container removed");
                Ok(())
            }
            Mode::System => {
                let backup = PathBuf::from(format!("{SYSTEM_CONF}{BACKUP_SUFFIX}"));
                if backup.exists() {
                    std::fs::rename(&backup, SYSTEM_CONF)
                        .with_context(|| format!("cannot restore {SYSTEM_CONF}"))?;
                    progress("restored the original nginx.conf");
                }
                if settings.host.adopted {
                    let _ = run_captured("systemctl", &["reload-or-restart", "nginx"]);
                    return Ok(());
                }
                let _ = run_captured("systemctl", &["disable", "--now", "nginx"]);
                if has_command("apt-get") {
                    run_streaming("apt-get", &["remove", "-y", "nginx"], progress)?;
                } else if has_command("dnf") {
                    run_streaming("dnf", &["remove", "-y", "nginx"], progress)?;
                }
                for file in [APT_LIST, APT_PIN, APT_KEYRING, YUM_REPO] {
                    let _ = std::fs::remove_file(file);
                }
                Ok(())
            }
        }
    }
}

/// apt: the nginx.org repository first (current stable, `http2 on`,
/// `ssl_reject_handshake`), the distribution's package when nginx.org has
/// no build for this release. Returns whether nginx.org was used.
fn install_apt(os: &OsRelease, progress: Progress<'_>) -> Result<bool> {
    let flavour = if os.is("ubuntu") { "ubuntu" } else { "debian" };
    let mut used_repo = false;
    if !os.codename.is_empty() {
        match add_apt_repo(flavour, &os.codename, progress) {
            Ok(()) => match run_streaming("apt-get", &["update"], progress) {
                Ok(()) => used_repo = true,
                Err(err) => {
                    progress(&format!(
                        "nginx.org has no packages for {flavour} {} ({err:#}); using the distribution package",
                        os.codename
                    ));
                    for file in [APT_LIST, APT_PIN, APT_KEYRING] {
                        let _ = std::fs::remove_file(file);
                    }
                }
            },
            Err(err) => progress(&format!(
                "cannot add the nginx.org repository ({err:#}); using the distribution package"
            )),
        }
    }
    if !used_repo {
        run_streaming("apt-get", &["update"], progress)?;
    }
    run_streaming(
        "apt-get",
        &[
            "install",
            "-y",
            "-o",
            "Dpkg::Options::=--force-confold",
            "nginx",
        ],
        progress,
    )?;
    Ok(used_repo)
}

fn add_apt_repo(flavour: &str, codename: &str, progress: Progress<'_>) -> Result<()> {
    progress("adding the nginx.org repository");
    let key = download_text(NGINX_KEY_URL)?;
    if !key.contains("BEGIN PGP PUBLIC KEY BLOCK") {
        bail!("unexpected signing key response");
    }
    std::fs::create_dir_all("/etc/apt/keyrings").context("cannot create /etc/apt/keyrings")?;
    std::fs::write(APT_KEYRING, key).context("cannot write the nginx.org signing key")?;
    std::fs::write(
        APT_LIST,
        format!(
            "# Added by asc-daemon (web server).\ndeb [signed-by={APT_KEYRING}] https://nginx.org/packages/{flavour} {codename} nginx\n"
        ),
    )
    .context("cannot write the apt source")?;
    std::fs::write(
        APT_PIN,
        "Package: *\nPin: origin nginx.org\nPin: release o=nginx\nPin-Priority: 900\n",
    )
    .context("cannot write the apt pin")?;
    Ok(())
}

fn install_dnf(os: &OsRelease, progress: Progress<'_>) -> Result<bool> {
    let manager = if has_command("dnf") { "dnf" } else { "yum" };
    // nginx.org builds for the RHEL family, not for Fedora.
    if !os.is("fedora") || os.is("rhel") {
        std::fs::write(
            YUM_REPO,
            format!(
                "# Added by asc-daemon (web server).\n[nginx-asc-stable]\nname=nginx stable (asc)\n\
                 baseurl=https://nginx.org/packages/centos/$releasever/$basearch/\ngpgcheck=1\nenabled=1\n\
                 gpgkey={NGINX_KEY_URL}\nmodule_hotfixes=true\n"
            ),
        )
        .context("cannot write the nginx.org repository")?;
        match run_streaming(manager, &["install", "-y", "nginx"], progress) {
            Ok(()) => return Ok(true),
            Err(err) => {
                progress(&format!(
                    "nginx.org install failed ({err:#}); using the distribution package"
                ));
                let _ = std::fs::remove_file(YUM_REPO);
            }
        }
    }
    run_streaming(manager, &["install", "-y", "nginx"], progress)?;
    Ok(false)
}

fn download_text(url: &str) -> Result<String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(30)))
        .build()
        .into();
    agent
        .get(url)
        .call()
        .with_context(|| format!("cannot fetch {url}"))?
        .body_mut()
        .with_config()
        .limit(256 * 1024)
        .read_to_string()
        .with_context(|| format!("cannot read {url}"))
}

/// SELinux blocks nginx from connecting to upstream ports by default.
fn selinux_allow_proxy(progress: Progress<'_>) {
    if !has_command("getenforce") {
        return;
    }
    let enforcing = run_captured("getenforce", &[]).is_ok_and(|(_, out)| out.trim() == "Enforcing");
    if enforcing && has_command("setsebool") {
        match run_captured("setsebool", &["-P", "httpd_can_network_connect", "1"]) {
            Ok((true, _)) => progress("SELinux: allowed nginx to connect to upstreams"),
            _ => progress("warning: could not set SELinux httpd_can_network_connect"),
        }
    }
}

/// A host firewall silently blocks ACME and visitors; say so instead of
/// changing the operator's firewall behind their back.
fn firewall_hint(progress: Progress<'_>) {
    if has_command("ufw")
        && run_captured("ufw", &["status"]).is_ok_and(|(_, out)| out.contains("Status: active"))
    {
        progress("note: ufw is active — open the ports with `ufw allow 80,443/tcp`");
    }
    if has_command("firewall-cmd")
        && run_captured("firewall-cmd", &["--state"]).is_ok_and(|(ok, _)| ok)
    {
        progress(
            "note: firewalld is running — open the ports with `firewall-cmd --permanent --add-service=http --add-service=https && firewall-cmd --reload`",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_output() {
        assert_eq!(
            parse_version_output("nginx version: nginx/1.24.0 (Ubuntu)\n").as_deref(),
            Some("1.24.0")
        );
        assert_eq!(parse_version_output("nope"), None);
    }

    #[test]
    fn default_servers_are_detected() {
        let debian = "server {\n\tlisten 80 default_server;\n\tlisten [::]:80 default_server;\n}";
        assert_eq!(default_server_ports(debian), (true, false));
        assert_eq!(
            default_server_ports("listen 443 ssl default_server; # x"),
            (false, true)
        );
        assert_eq!(
            default_server_ports("# listen 80 default_server;"),
            (false, false)
        );
        assert_eq!(
            default_server_ports("listen 8080 default_server;"),
            (false, false)
        );
    }

    #[test]
    fn profile_reads_user_and_pid() {
        let conf = "user  www-data;\nworker_processes auto;\npid /run/nginx.pid;\n";
        let profile = host_profile(conf);
        assert_eq!(profile.user, "www-data");
        assert_eq!(profile.pid, "/run/nginx.pid");
    }
}
