//! CLI-side client of the daemon's local unix-socket API (DMN-042).
//!
//! A blocking facade over a minimal HTTP/1 JSON client: one connection per
//! request, no TLS, no pooling — the peer is a local daemon over a unix
//! socket, not a network service. Identity travels out-of-band: the daemon
//! reads the caller's uid from SO_PEERCRED, so there is no token to present.
//! Under `sudo` the CLI forwards `SUDO_UID`/`SUDO_USER` as attribution-hint
//! headers, which the daemon honors only for a root peer.
//!
//! The typed install errors ([`pkg::LicenseRequired`], [`pkg::AmbiguousPackage`])
//! are reconstructed from the structured REST payloads, so the CLI's
//! interactive recoveries (license consent, source pick) work identically
//! whether the install runs in-process or through the daemon.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::header::{CONTENT_TYPE, HOST};
use hyper::{Method, Request, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use tokio::net::UnixStream;

use crate::daemon::api::uds::{SUDO_UID_HEADER, SUDO_USER_HEADER};
use crate::daemon::config::Config;
use crate::daemon::i18n::{Msg, tf};
use crate::daemon::pkg;

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
    pub quota: Option<crate::daemon::apps::meta::Quota>,
}

impl RemoteApp {
    pub fn running(&self) -> bool {
        self.state == "running"
    }
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

        let err = typed_error(&serde_json::json!({ "error": "boom" }));
        assert_eq!(err.to_string(), "boom");
    }
}
