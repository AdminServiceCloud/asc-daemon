//! CLI-side client of the daemon's local unix-socket API (DMN-042).
//!
//! A blocking facade over a minimal HTTP/1 JSON client: one connection per
//! request, no TLS, no pooling — the peer is a local daemon over a unix
//! socket, not a network service. Identity travels out-of-band: the daemon
//! reads the caller's uid from SO_PEERCRED, so there is no token to present.
//! Under `sudo` the CLI forwards `SUDO_UID`/`SUDO_USER` as attribution-hint
//! headers, which the daemon honors only for a root peer.
//!
//! The typed install errors ([`pkg::LicenseRequired`], [`pkg::AmbiguousPackage`],
//! [`pkg::auth::AuthRequired`]) are reconstructed from the structured REST
//! payloads, so the CLI's interactive recoveries (license consent, source
//! pick, auth setup for a private repository) work identically whether the
//! install runs in-process or through the daemon.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::header::{CONTENT_TYPE, HOST};
use hyper::{Method, Request, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use tokio::net::UnixStream;

use crate::daemon::api::uds::{SUDO_UID_HEADER, SUDO_USER_HEADER};
use crate::daemon::apps::disk::DiskUsage;
use crate::daemon::config::Config;
use crate::daemon::docker::PublishedPort;
use crate::daemon::i18n::{Msg, tf};
use crate::daemon::pkg;
use crate::daemon::pkg::settings::{SettingValues, SettingsFile};

/// How long a connection attempt may take before the daemon is declared
/// unreachable. Local socket: a healthy daemon answers instantly.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// One installed app as the API reports it (`AppJson` in the REST layer).
#[derive(Debug, serde::Deserialize)]
pub struct RemoteApp {
    pub id: String,
    /// Stable instance identity (DMN-044); absent for pre-DMN-044 installs.
    #[serde(default)]
    pub uuid: Option<String>,
    pub name: String,
    pub kind: String,
    pub state: String,
    pub version: Option<String>,
    pub source: Option<String>,
    pub owner: String,
    /// Package title when the app carries a custom name.
    pub title: Option<String>,
    /// Registry install spec (`name` or `stack/app`) — the app's stack
    /// membership for `asc stacks`.
    #[serde(default)]
    pub package: Option<String>,
    pub quota: Option<crate::daemon::apps::meta::Quota>,
}

impl RemoteApp {
    pub fn running(&self) -> bool {
        self.state == "running"
    }
}

/// The app a per-app report is about: the daemon resolves the reference the
/// caller passed (an id or a custom name) and names the app it answered for.
#[derive(Debug, serde::Deserialize)]
pub struct RemoteAppRef {
    pub id: String,
    /// The name to show: the user's custom name, else the package title.
    pub name: String,
}

/// One app's line in the daemon's apps-wide disk report (DMN-053).
#[derive(Debug, serde::Deserialize)]
pub struct RemoteDiskRow {
    pub id: String,
    pub name: String,
    pub owner: String,
    pub bytes: u64,
}

/// `asc disk` with no app, as the daemon reports it: every visible app's
/// footprint plus the capacity of the filesystem holding the app store.
#[derive(Debug, serde::Deserialize)]
pub struct RemoteDiskSummary {
    pub fs_total: Option<u64>,
    pub apps: Vec<RemoteDiskRow>,
}

/// One app and the ports it publishes (DMN-049).
#[derive(Debug, serde::Deserialize)]
pub struct RemotePortsRow {
    pub id: String,
    pub name: String,
    pub owner: String,
    pub ports: Vec<PublishedPort>,
}

/// One app's resource counters, mirroring `AppStats` of the in-process path.
#[derive(Debug, serde::Deserialize)]
pub struct RemoteStats {
    pub id: String,
    pub kind: String,
    pub owner: String,
    pub cpu_percent: Option<f64>,
    pub memory_bytes: Option<u64>,
    pub disk_bytes: u64,
    pub quota_disk_bytes: Option<u64>,
    pub disk_read_bytes: Option<u64>,
    pub disk_write_bytes: Option<u64>,
    pub net_rx_bytes: Option<u64>,
    pub net_tx_bytes: Option<u64>,
    pub disk_read_rate: Option<f64>,
    pub disk_write_rate: Option<f64>,
    pub net_rx_rate: Option<f64>,
    pub net_tx_rate: Option<f64>,
}

/// Result of creating an app backup through the local daemon API.
#[derive(Debug, serde::Deserialize)]
pub struct RemoteBackup {
    pub name: String,
    pub storage: String,
    pub bytes: u64,
}

/// Blocking client of the daemon's unix-socket API.
pub struct Daemon {
    socket: PathBuf,
    rt: tokio::runtime::Runtime,
}

impl Daemon {
    /// Connect to the daemon socket from the config: `Ok(None)` when no
    /// socket file exists (no daemon on this host — the CLI works
    /// in-process, DMN-041), `Err` when the socket exists but the daemon
    /// does not answer (stopped or hung service, stale file).
    pub fn connect(config: &Config) -> Result<Option<Self>> {
        let socket = config.api.socket.clone();
        if !socket.exists() {
            return Ok(None);
        }
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("cannot build the client runtime")?;
        let client = Self { socket, rt };
        // One cheap round-trip up front: every later error is then a real
        // operation error, not a lazily-discovered connection problem.
        client
            .request(Method::GET, "/v1/status", None)
            .map_err(|_| anyhow!(tf(Msg::DaemonUnreachable, client.socket.display())))?;
        Ok(Some(client))
    }

    /// `(daemon version, apps running, apps total)`.
    pub fn status(&self) -> Result<(String, u64, u64)> {
        let json = self.request(Method::GET, "/v1/status", None)?;
        Ok((
            json["version"].as_str().unwrap_or("-").to_string(),
            json["apps_running"].as_u64().unwrap_or(0),
            json["apps_total"].as_u64().unwrap_or(0),
        ))
    }

    pub fn list(&self) -> Result<Vec<RemoteApp>> {
        let json = self.request(Method::GET, "/v1/apps", None)?;
        serde_json::from_value(json["apps"].clone()).context("malformed app list from the daemon")
    }

    pub fn info(&self, id: &str) -> Result<RemoteApp> {
        let json = self.request(Method::GET, &format!("/v1/apps/{id}"), None)?;
        serde_json::from_value(json["app"].clone()).context("malformed app info from the daemon")
    }

    /// Disk usage of one app, broken down by image/repository/data/volumes,
    /// together with the resolved app (`reference` may be a custom name).
    pub fn app_disk(&self, reference: &str) -> Result<(RemoteAppRef, DiskUsage)> {
        let json = self.request(Method::GET, &format!("/v1/apps/{reference}/disk"), None)?;
        let app = serde_json::from_value(json.clone()).context("malformed app from the daemon")?;
        let usage = serde_json::from_value(json).context("malformed disk usage from the daemon")?;
        Ok((app, usage))
    }

    /// Space taken by every app the caller may see (DMN-053), largest first,
    /// with the capacity of the filesystem holding the app store.
    pub fn disk_summary(&self) -> Result<RemoteDiskSummary> {
        let json = self.request(Method::GET, "/v1/disk", None)?;
        serde_json::from_value(json).context("malformed disk report from the daemon")
    }

    /// The ports one app publishes, with the resolved app.
    pub fn app_ports(&self, reference: &str) -> Result<(RemoteAppRef, Vec<PublishedPort>)> {
        let json = self.request(Method::GET, &format!("/v1/apps/{reference}/ports"), None)?;
        let ports = serde_json::from_value(json["ports"].clone())
            .context("malformed port list from the daemon")?;
        let app = serde_json::from_value(json).context("malformed app from the daemon")?;
        Ok((app, ports))
    }

    pub fn ports_summary(&self) -> Result<Vec<RemotePortsRow>> {
        let json = self.request(Method::GET, "/v1/ports", None)?;
        serde_json::from_value(json["apps"].clone())
            .context("malformed port report from the daemon")
    }

    /// Resource consumption per app. The daemon samples twice ~500 ms apart,
    /// so this call takes about that long.
    pub fn stats(&self) -> Result<Vec<RemoteStats>> {
        let json = self.request(Method::GET, "/v1/stats", None)?;
        serde_json::from_value(json["apps"].clone()).context("malformed stats from the daemon")
    }

    /// Latest daemon system sample. Kept as JSON because the MCP tool returns
    /// the API's complete, forward-compatible metric document.
    pub fn metrics(&self) -> Result<Value> {
        self.request(Method::GET, "/v1/metrics", None)
    }

    /// Upgrade an app (DMN-053): `reference` is its id or custom name,
    /// `version` an explicit tag (`None` — the newest one, or the tracked
    /// branch of a direct repository install). The daemon clones with its own
    /// git credentials, so there is no interactive auth setup on this path.
    pub fn upgrade(&self, reference: &str, version: Option<&str>) -> Result<pkg::UpgradeOutcome> {
        let body = serde_json::json!({ "version": version });
        let json = self.request(
            Method::POST,
            &format!("/v1/apps/{reference}/upgrade"),
            Some(body),
        )?;
        let id = json["id"].as_str().unwrap_or(reference).to_string();
        if json["up_to_date"].as_bool().unwrap_or(false) {
            return Ok(pkg::UpgradeOutcome::UpToDate {
                id,
                version: json["version"].as_str().unwrap_or_default().to_string(),
            });
        }
        Ok(pkg::UpgradeOutcome::Upgraded {
            id,
            from: json["from"].as_str().map(str::to_string),
            to: json["to"].as_str().unwrap_or_default().to_string(),
            // Absent from an older daemon's answer (DMN-056): the CLI then
            // prints the versions alone, as it did before.
            from_commit: json["from_commit"].as_str().map(str::to_string),
            to_commit: json["to_commit"].as_str().map(str::to_string),
        })
    }

    /// `true` when the app was already running (idempotent call).
    pub fn start(&self, id: &str) -> Result<bool> {
        let json = self.request(Method::POST, &format!("/v1/apps/{id}/start"), None)?;
        Ok(json["already_running"].as_bool().unwrap_or(false))
    }

    /// `true` when the app was already stopped.
    pub fn stop(&self, id: &str) -> Result<bool> {
        let json = self.request(Method::POST, &format!("/v1/apps/{id}/stop"), None)?;
        Ok(json["already_stopped"].as_bool().unwrap_or(false))
    }

    pub fn restart(&self, id: &str) -> Result<()> {
        self.request(Method::POST, &format!("/v1/apps/{id}/restart"), None)?;
        Ok(())
    }

    pub fn logs(&self, id: &str, tail: usize) -> Result<String> {
        let json = self.request(
            Method::GET,
            &format!("/v1/apps/{id}/logs?tail={tail}"),
            None,
        )?;
        Ok(json["logs"].as_str().unwrap_or_default().to_string())
    }

    pub fn remove(&self, id: &str) -> Result<()> {
        self.request(Method::DELETE, &format!("/v1/apps/{id}"), None)?;
        Ok(())
    }

    /// Local-only, UDS peer-authorized backup operations used by `asc mcp`.
    pub fn backup_list(&self, id: &str) -> Result<Vec<String>> {
        let json = self.request(Method::GET, &format!("/v1/local/apps/{id}/backups"), None)?;
        serde_json::from_value(json["backups"].clone())
            .context("malformed backup list from the daemon")
    }

    pub fn backup_create(&self, id: &str) -> Result<RemoteBackup> {
        let json = self.request(Method::POST, &format!("/v1/local/apps/{id}/backups"), None)?;
        serde_json::from_value(json).context("malformed backup response from the daemon")
    }

    pub fn backup_restore(&self, id: &str, name: &str) -> Result<()> {
        self.request(
            Method::POST,
            &format!("/v1/local/apps/{id}/backups/{name}"),
            None,
        )?;
        Ok(())
    }

    pub fn backup_prune(&self, id: &str, keep: u32) -> Result<Vec<String>> {
        let json = self.request(
            Method::DELETE,
            &format!("/v1/local/apps/{id}/backups"),
            Some(serde_json::json!({ "keep": keep })),
        )?;
        serde_json::from_value(json["removed"].clone())
            .context("malformed backup prune response from the daemon")
    }

    #[allow(clippy::too_many_arguments)]
    pub fn install(
        &self,
        spec: &str,
        source: Option<&str>,
        name: Option<&str>,
        branch: Option<&str>,
        tag: Option<&str>,
        license_ack: bool,
        image_choice: Option<crate::daemon::apps::ImageSource>,
    ) -> Result<pkg::InstallOutcome> {
        let body = serde_json::json!({
            "spec": spec,
            "source": source,
            "name": name,
            "branch": branch,
            "tag": tag,
            "license_ack": license_ack,
            "image_choice": image_choice,
        });
        let json = self.request(Method::POST, "/v1/apps", Some(body))?;
        let report = |v: &Value| pkg::InstallReport {
            id: v["id"].as_str().unwrap_or_default().to_string(),
            version: v["version"].as_str().unwrap_or_default().to_string(),
        };
        let apps = json["apps"].as_array().cloned().unwrap_or_default();
        let skipped: Vec<String> = json["skipped"]
            .as_array()
            .map(|s| {
                s.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        if apps.is_empty() && skipped.is_empty() {
            return Ok(pkg::InstallOutcome::App(report(&json)));
        }
        Ok(pkg::InstallOutcome::Stack {
            stack: json["id"].as_str().unwrap_or_default().to_string(),
            installed: apps.iter().map(report).collect(),
            skipped,
        })
    }

    /// An app's settings schema and current values (DMN-043). The editor
    /// then runs entirely in the CLI, exactly as it does in-process — only
    /// the reading and the writing move to the daemon, which is the half a
    /// user without access to the system app tree cannot do themselves.
    pub fn settings(&self, id: &str) -> Result<(Option<SettingsFile>, SettingValues)> {
        let json = self.request(Method::GET, &format!("/v1/apps/{id}/settings"), None)?;
        let file: Option<SettingsFile> = serde_json::from_value(json["settings"].clone())
            .context("malformed settings schema from the daemon")?;
        let values = match json["values"].clone() {
            Value::Object(map) => SettingValues::from_map(map),
            Value::Null => SettingValues::default(),
            other => bail!("malformed setting values from the daemon: {other}"),
        };
        Ok((file, values))
    }

    /// Persist the edited values; the daemon validates them against the
    /// app's own schema before writing.
    pub fn save_settings(&self, id: &str, values: &SettingValues) -> Result<()> {
        let body = serde_json::json!({ "values": values.as_map() });
        self.request(Method::PUT, &format!("/v1/apps/{id}/settings"), Some(body))?;
        Ok(())
    }

    /// A one-time console token for `id` (`"logs"` or `"attach"`). Issued
    /// only for an app the caller may manage, and consumed by the first
    /// `/v1/console` connection that presents it.
    pub fn console_token(&self, id: &str, session: &str) -> Result<String> {
        let body = serde_json::json!({ "session": session });
        let json = self.request(
            Method::POST,
            &format!("/v1/apps/{id}/console-token"),
            Some(body),
        )?;
        json["token"]
            .as_str()
            .map(str::to_string)
            .context("the daemon issued no console token")
    }

    /// Attach to an app's console through the daemon (DMN-043): the daemon
    /// holds the Docker connection, so this works for a user who is not in
    /// the `docker` group and cannot read the system app tree. The terminal's
    /// stdin goes to the app, the app's output to stdout — the same contract
    /// as the in-process attach, and the same shared session the browser
    /// console joins (all clients of one app see the same live output).
    ///
    /// Returns when the app stops, the socket closes or stdin reaches EOF.
    pub fn attach(&self, id: &str) -> Result<()> {
        let token = self.console_token(id, "attach")?;
        self.rt.block_on(self.attach_loop(&token))
    }

    async fn attach_loop(&self, token: &str) -> Result<()> {
        use futures_util::{SinkExt, StreamExt};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio_tungstenite::tungstenite::Message;

        let stream = tokio::time::timeout(CONNECT_TIMEOUT, UnixStream::connect(&self.socket))
            .await
            .context("connection timed out")?
            .with_context(|| format!("cannot reach the daemon at {}", self.socket.display()))?;
        // The host is meaningless over a unix socket, but the handshake
        // needs a syntactically valid ws:// URL to derive its headers from.
        let url = format!("ws://asc-daemon/v1/console?token={token}");
        let (mut socket, _) = tokio_tungstenite::client_async(url, stream)
            .await
            .context("cannot open the console session")?;

        let mut stdin = tokio::io::stdin();
        let mut stdout = tokio::io::stdout();
        let mut buf = [0u8; 4096];
        loop {
            tokio::select! {
                frame = socket.next() => match frame {
                    Some(Ok(Message::Binary(data))) => {
                        stdout.write_all(&data).await?;
                        stdout.flush().await?;
                    }
                    // The console reports its own failures as text before
                    // closing (app stopped, attach unsupported).
                    Some(Ok(Message::Text(text))) => {
                        if let Some(err) = text.strip_prefix("error: ") {
                            bail!("{err}");
                        }
                        stdout.write_all(text.as_bytes()).await?;
                        stdout.flush().await?;
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(err)) => return Err(err).context("console session failed"),
                },
                read = stdin.read(&mut buf) => match read? {
                    0 => break, // stdin closed: detach, the app keeps running
                    n => socket.send(Message::Binary(buf[..n].to_vec().into())).await?,
                },
            }
        }
        socket.close(None).await.ok();
        Ok(())
    }

    /// One request/response round-trip; non-2xx responses become errors,
    /// with the typed install errors reconstructed from their payloads.
    fn request(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value> {
        let (status, json) = self
            .rt
            .block_on(self.roundtrip(method, path, body))
            .with_context(|| format!("cannot reach the daemon at {}", self.socket.display()))?;
        if status.is_success() {
            return Ok(json);
        }
        Err(typed_error(&json))
    }

    async fn roundtrip(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<(StatusCode, Value)> {
        let stream = tokio::time::timeout(CONNECT_TIMEOUT, UnixStream::connect(&self.socket))
            .await
            .context("connection timed out")??;
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .context("HTTP handshake failed")?;
        // The connection task finishes when the response body is done.
        tokio::spawn(conn);

        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header(HOST, "asc-daemon");
        // Attribution hint for `sudo asc ...` — meaningful (and honored)
        // only when the peer is root; see UserContext::from_peer.
        if let Ok(uid) = std::env::var("SUDO_UID") {
            request = request.header(SUDO_UID_HEADER, uid);
        }
        if let Ok(user) = std::env::var("SUDO_USER") {
            request = request.header(SUDO_USER_HEADER, user);
        }
        let request = match body {
            Some(json) => request
                .header(CONTENT_TYPE, "application/json")
                .body(Full::new(Bytes::from(serde_json::to_vec(&json)?)))?,
            None => request.body(Full::new(Bytes::new()))?,
        };
        let response = sender.send_request(request).await?;
        let status = response.status();
        let bytes = response.into_body().collect().await?.to_bytes();
        let json = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).context("malformed JSON from the daemon")?
        };
        Ok((status, json))
    }
}

/// Rebuild the typed install errors from the structured REST payload, so
/// the CLI's downcast-based recoveries (license consent, source pick) see
/// the same error types as the in-process path. Everything else surfaces
/// as a plain error carrying the daemon's message.
fn typed_error(json: &Value) -> anyhow::Error {
    if let Some(required) = json.get("auth_required") {
        return anyhow::Error::new(pkg::auth::AuthRequired {
            url: required["url"].as_str().unwrap_or_default().to_string(),
        });
    }
    if let Some(license) = json.get("license_required") {
        return anyhow::Error::new(pkg::LicenseRequired {
            package: license["package"].as_str().unwrap_or_default().to_string(),
            source: license["source"].as_str().unwrap_or_default().to_string(),
            git: license["git"].as_str().unwrap_or_default().to_string(),
            license: license["license"].as_str().unwrap_or_default().to_string(),
        });
    }
    if let Some(ambiguous) = json.get("ambiguous") {
        return anyhow::Error::new(pkg::AmbiguousPackage {
            name: ambiguous["name"].as_str().unwrap_or_default().to_string(),
            candidates: ambiguous["candidates"]
                .as_array()
                .map(|list| {
                    list.iter()
                        .map(|c| {
                            (
                                c["source"].as_str().unwrap_or_default().to_string(),
                                c["git"].as_str().unwrap_or_default().to_string(),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default(),
        });
    }
    if let Some(choice) = json.get("version_choice") {
        let strings = |key: &str| {
            choice[key]
                .as_array()
                .map(|list| {
                    list.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        };
        return anyhow::Error::new(pkg::VersionChoiceRequired {
            package: choice["package"].as_str().unwrap_or_default().to_string(),
            source: choice["source"].as_str().map(str::to_string),
            tags: strings("tags"),
            branches: strings("branches"),
        });
    }
    if let Some(choice) = json.get("image_choice") {
        return anyhow::Error::new(pkg::ImageChoiceRequired {
            package: choice["package"].as_str().unwrap_or_default().to_string(),
            image: choice["image"].as_str().unwrap_or_default().to_string(),
            build: choice["build"].as_str().unwrap_or_default().to_string(),
        });
    }
    match json.get("error").and_then(|e| e.as_str()) {
        Some(msg) => anyhow!("{msg}"),
        None => anyhow!("daemon request failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_errors_are_reconstructed() {
        let err = typed_error(&serde_json::json!({
            "error": "license required",
            "license_required": {
                "package": "cs2", "source": "official",
                "git": "https://example.com/cs2", "license": "MIT",
            },
        }));
        let license = err.downcast_ref::<pkg::LicenseRequired>().unwrap();
        assert_eq!(license.package, "cs2");
        assert_eq!(license.license, "MIT");

        let err = typed_error(&serde_json::json!({
            "error": "ambiguous",
            "ambiguous": { "name": "nginx", "candidates": [
                { "source": "official", "git": "https://a" },
                { "source": "fork", "git": "https://b" },
            ]},
        }));
        let ambiguous = err.downcast_ref::<pkg::AmbiguousPackage>().unwrap();
        assert_eq!(ambiguous.candidates.len(), 2);
        assert_eq!(ambiguous.candidates[1].0, "fork");

        let err = typed_error(&serde_json::json!({
            "error": "pick a version",
            "version_choice": {
                "package": "nginx", "source": "official",
                "tags": ["v1.28.0", "v1.27.0"], "branches": ["main"],
            },
        }));
        let choice = err.downcast_ref::<pkg::VersionChoiceRequired>().unwrap();
        assert_eq!(choice.package, "nginx");
        assert_eq!(choice.source.as_deref(), Some("official"));
        assert_eq!(choice.tags, vec!["v1.28.0", "v1.27.0"]);
        assert_eq!(choice.branches, vec!["main"]);

        // DMN-062: a private repository reported by the daemon must arrive
        // as the same typed error the in-process clone raises, so the CLI's
        // interactive auth setup runs on both paths.
        let err = typed_error(&serde_json::json!({
            "error": "authorization required",
            "auth_required": { "url": "https://github.com/org/private" },
        }));
        let required = err.downcast_ref::<pkg::auth::AuthRequired>().unwrap();
        assert_eq!(required.url, "https://github.com/org/private");

        let err = typed_error(&serde_json::json!({ "error": "boom" }));
        assert_eq!(err.to_string(), "boom");
    }
}
