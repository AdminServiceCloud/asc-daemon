//! Local stdio Model Context Protocol server (DMN-013).
//!
//! `asc mcp serve` is not part of the daemon process. It talks to the daemon
//! through the existing Unix socket, so the kernel supplies the MCP process's
//! UID to every request (`SO_PEERCRED`). This is the authorization boundary:
//! no MCP argument can select a different user or bypass app ownership.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::schemars::JsonSchema;
use rmcp::{ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::daemon::apps::ImageSource;
use crate::daemon::client::{Daemon, RemoteApp, RemoteStats};
use crate::daemon::config::Config;

const DEFAULT_LOG_TAIL: usize = 200;
const MAX_LOG_TAIL: usize = 10_000;
const DEFAULT_EXEC_TIMEOUT_SECS: u64 = 60;
const MAX_EXEC_TIMEOUT_SECS: u64 = 300;
const MAX_EXEC_OUTPUT_BYTES: usize = 1024 * 1024;

/// Blocking UDS client behind the async MCP handlers.
///
/// The daemon client owns a small Tokio runtime, therefore all of its calls
/// run in `spawn_blocking`; calling it directly from an MCP handler would try
/// to enter a Tokio runtime from inside another runtime.
#[derive(Clone)]
pub struct McpBackend {
    daemon: Arc<Mutex<Daemon>>,
}

impl McpBackend {
    pub fn connect(config: &Config) -> Result<Self> {
        let daemon = Daemon::connect(config)?.ok_or_else(|| {
            anyhow!("ASC daemon is not running; start it with 'asc service start'")
        })?;
        Ok(Self {
            daemon: Arc::new(Mutex::new(daemon)),
        })
    }

    async fn call<T, F>(&self, call: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Daemon) -> Result<T> + Send + 'static,
    {
        let daemon = Arc::clone(&self.daemon);
        tokio::task::spawn_blocking(move || {
            let daemon = daemon
                .lock()
                .map_err(|_| anyhow!("daemon client lock poisoned"))?;
            call(&daemon)
        })
        .await
        .context("MCP daemon operation panicked")?
    }
}

#[derive(Clone)]
pub struct McpServer {
    backend: McpBackend,
    // The rmcp macro reads this field from generated methods, which rustc's
    // dead-code analysis cannot see directly.
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

impl McpServer {
    pub fn new(backend: McpBackend) -> Self {
        Self {
            backend,
            tool_router: Self::tool_router(),
        }
    }
}

/// Start the stdio server and wait until the MCP client closes its streams.
pub async fn serve(config: Config) -> Result<()> {
    // `Daemon::connect` performs its health probe through the client's own
    // Tokio runtime, so it must not run on this MCP runtime thread.
    let backend = tokio::task::spawn_blocking(move || McpBackend::connect(&config))
        .await
        .context("MCP daemon connection task panicked")??;
    let server = McpServer::new(backend)
        .serve(rmcp::transport::stdio())
        .await
        .context("cannot start MCP stdio server")?;
    server.waiting().await.context("MCP stdio server failed")?;
    Ok(())
}

fn result<T: serde::Serialize>(value: T) -> CallToolResult {
    match serde_json::to_value(value) {
        Ok(value) => CallToolResult::structured(value),
        Err(error) => tool_error(error),
    }
}

fn tool_error(error: impl std::fmt::Display) -> CallToolResult {
    CallToolResult::error(vec![rmcp::model::ContentBlock::text(error.to_string())])
}

fn app_json(app: RemoteApp) -> Value {
    json!({
        "id": app.id,
        "uuid": app.uuid,
        "name": app.name,
        "kind": app.kind,
        "state": app.state,
        "version": app.version,
        "source": app.source,
        "owner": app.owner,
        "title": app.title,
        "package": app.package,
        "quota": app.quota,
    })
}

fn stats_json(stats: Vec<RemoteStats>) -> Value {
    Value::Array(
        stats
            .into_iter()
            .map(|stat| {
                json!({
                    "id": stat.id,
                    "kind": stat.kind,
                    "owner": stat.owner,
                    "cpu_percent": stat.cpu_percent,
                    "memory_bytes": stat.memory_bytes,
                    "disk_bytes": stat.disk_bytes,
                    "quota_disk_bytes": stat.quota_disk_bytes,
                    "disk_read_bytes": stat.disk_read_bytes,
                    "disk_write_bytes": stat.disk_write_bytes,
                    "net_rx_bytes": stat.net_rx_bytes,
                    "net_tx_bytes": stat.net_tx_bytes,
                    "disk_read_rate": stat.disk_read_rate,
                    "disk_write_rate": stat.disk_write_rate,
                    "net_rx_rate": stat.net_rx_rate,
                    "net_tx_rate": stat.net_tx_rate,
                })
            })
            .collect(),
    )
}

#[derive(Debug, Deserialize, JsonSchema)]
struct AppInput {
    /// Application id or custom name. The daemon authorizes this reference.
    app: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct LogsInput {
    /// Application id or custom name.
    app: String,
    /// Number of final log lines; defaults to 200 and is capped at 10,000.
    tail: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct InstallInput {
    /// Registry package/stack spec or a direct git URL.
    spec: String,
    source: Option<String>,
    name: Option<String>,
    branch: Option<String>,
    tag: Option<String>,
    #[serde(default)]
    license_ack: bool,
    /// `prebuilt` or `build` when the package offers both image sources.
    image_choice: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct UpgradeInput {
    app: String,
    /// Explicit package version/tag; omitted selects the daemon's normal latest version.
    version: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ControlInput {
    app: String,
    /// One of `start`, `stop`, or `restart`.
    action: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SettingsPatchInput {
    app: String,
    /// Merge patch: `null` removes a value; all resulting settings are validated by daemon.
    values: serde_json::Map<String, Value>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RestoreInput {
    app: String,
    backup: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PruneInput {
    app: String,
    /// Number of newest backups to retain.
    keep: u32,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ExecInput {
    /// Command passed to `/bin/sh -lc` under the MCP process's OS UID.
    command: String,
    /// Optional existing working directory.
    cwd: Option<String>,
    /// Timeout in seconds; defaults to 60 and cannot exceed 300.
    timeout_secs: Option<u64>,
}

#[tool_router]
impl McpServer {
    #[tool(
        description = "Get ASC daemon version and the number of visible applications.",
        annotations(read_only_hint = true)
    )]
    async fn system_info(&self) -> CallToolResult {
        match self.backend.call(|daemon| daemon.status()).await {
            Ok((version, running, total)) => result(json!({
                "version": version,
                "apps_running": running,
                "apps_total": total,
            })),
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Get the latest system metrics and visible application resource metrics.",
        annotations(read_only_hint = true)
    )]
    async fn metrics_get(&self) -> CallToolResult {
        let metrics = self.backend.call(|daemon| daemon.metrics()).await;
        let stats = self.backend.call(|daemon| daemon.stats()).await;
        match (metrics, stats) {
            (Ok(metrics), Ok(stats)) => {
                result(json!({ "system": metrics, "apps": stats_json(stats) }))
            }
            (Err(error), _) | (_, Err(error)) => tool_error(error),
        }
    }

    #[tool(
        description = "List applications visible to the current MCP process user.",
        annotations(read_only_hint = true)
    )]
    async fn app_list(&self) -> CallToolResult {
        match self.backend.call(|daemon| daemon.list()).await {
            Ok(apps) => result(Value::Array(apps.into_iter().map(app_json).collect())),
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Get one authorized application's details.",
        annotations(read_only_hint = true)
    )]
    async fn app_info(&self, Parameters(input): Parameters<AppInput>) -> CallToolResult {
        match self
            .backend
            .call(move |daemon| daemon.info(&input.app))
            .await
        {
            Ok(app) => result(app_json(app)),
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Read final lines of an authorized application's logs.",
        annotations(read_only_hint = true)
    )]
    async fn logs_read(&self, Parameters(input): Parameters<LogsInput>) -> CallToolResult {
        let tail = input.tail.unwrap_or(DEFAULT_LOG_TAIL).min(MAX_LOG_TAIL);
        match self
            .backend
            .call(move |daemon| daemon.logs(&input.app, tail))
            .await
        {
            Ok(logs) => result(json!({ "logs": logs, "tail": tail })),
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Get an application's settings schema and current values.",
        annotations(read_only_hint = true)
    )]
    async fn app_settings_get(&self, Parameters(input): Parameters<AppInput>) -> CallToolResult {
        match self
            .backend
            .call(move |daemon| daemon.settings(&input.app))
            .await
        {
            Ok((schema, values)) => result(json!({ "schema": schema, "values": values.as_map() })),
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Install an application through the authorized local daemon.",
        annotations(open_world_hint = true)
    )]
    async fn app_install(&self, Parameters(input): Parameters<InstallInput>) -> CallToolResult {
        let image_choice = match input.image_choice.as_deref() {
            None => Ok(None),
            Some("prebuilt") => Ok(Some(ImageSource::Prebuilt)),
            Some("build") => Ok(Some(ImageSource::Build)),
            Some(other) => Err(anyhow!(
                "image_choice must be 'prebuilt' or 'build', got '{other}'"
            )),
        };
        let image_choice = match image_choice {
            Ok(value) => value,
            Err(error) => return tool_error(error),
        };
        match self
            .backend
            .call(move |daemon| {
                daemon.install(
                    &input.spec,
                    input.source.as_deref(),
                    input.name.as_deref(),
                    input.branch.as_deref(),
                    input.tag.as_deref(),
                    input.license_ack,
                    image_choice,
                )
            })
            .await
        {
            Ok(outcome) => result(json!({ "outcome": format!("{outcome:?}") })),
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Upgrade a stopped, authorized application.",
        annotations(open_world_hint = true)
    )]
    async fn app_upgrade(&self, Parameters(input): Parameters<UpgradeInput>) -> CallToolResult {
        match self
            .backend
            .call(move |daemon| daemon.upgrade(&input.app, input.version.as_deref()))
            .await
        {
            Ok(outcome) => result(json!({ "outcome": format!("{outcome:?}") })),
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Start, stop, or restart an authorized application.",
        annotations(idempotent_hint = true)
    )]
    async fn app_control(&self, Parameters(input): Parameters<ControlInput>) -> CallToolResult {
        let action = input.action.clone();
        let app = input.app.clone();
        let output = match action.as_str() {
            "start" => {
                self.backend
                    .call(move |daemon| {
                        daemon
                            .start(&app)
                            .map(|already| json!({ "already_running": already }))
                    })
                    .await
            }
            "stop" => {
                self.backend
                    .call(move |daemon| {
                        daemon
                            .stop(&app)
                            .map(|already| json!({ "already_stopped": already }))
                    })
                    .await
            }
            "restart" => {
                self.backend
                    .call(move |daemon| daemon.restart(&app).map(|()| json!({ "restarted": true })))
                    .await
            }
            _ => return tool_error("action must be 'start', 'stop', or 'restart'"),
        };
        match output {
            Ok(value) => result(value),
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Merge-patch settings for an authorized application; daemon validates the full result."
    )]
    async fn app_settings_update(
        &self,
        Parameters(input): Parameters<SettingsPatchInput>,
    ) -> CallToolResult {
        match self
            .backend
            .call(move |daemon| {
                let (_, current) = daemon.settings(&input.app)?;
                let mut values = current.as_map().clone();
                for (key, value) in input.values {
                    if value.is_null() {
                        values.remove(&key);
                    } else {
                        values.insert(key, value);
                    }
                }
                let values = crate::daemon::pkg::settings::SettingValues::from_map(values);
                daemon.save_settings(&input.app, &values)
            })
            .await
        {
            Ok(()) => result(json!({ "updated": true })),
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "List backups of an authorized application from built-in local storage.",
        annotations(read_only_hint = true)
    )]
    async fn backup_list(&self, Parameters(input): Parameters<AppInput>) -> CallToolResult {
        match self
            .backend
            .call(move |daemon| daemon.backup_list(&input.app))
            .await
        {
            Ok(backups) => result(json!({ "backups": backups, "storage": "local" })),
            Err(error) => tool_error(error),
        }
    }

    #[tool(description = "Create a backup of an authorized application in built-in local storage.")]
    async fn backup_create(&self, Parameters(input): Parameters<AppInput>) -> CallToolResult {
        match self
            .backend
            .call(move |daemon| daemon.backup_create(&input.app))
            .await
        {
            Ok(backup) => result(
                json!({ "name": backup.name, "storage": backup.storage, "bytes": backup.bytes }),
            ),
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Restore a stopped authorized application from a local backup.",
        annotations(destructive_hint = true)
    )]
    async fn backup_restore(&self, Parameters(input): Parameters<RestoreInput>) -> CallToolResult {
        match self
            .backend
            .call(move |daemon| daemon.backup_restore(&input.app, &input.backup))
            .await
        {
            Ok(()) => result(json!({ "restored": true })),
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Delete oldest local backups beyond the requested retention count.",
        annotations(destructive_hint = true)
    )]
    async fn backup_prune(&self, Parameters(input): Parameters<PruneInput>) -> CallToolResult {
        match self
            .backend
            .call(move |daemon| daemon.backup_prune(&input.app, input.keep))
            .await
        {
            Ok(removed) => result(json!({ "removed": removed })),
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Permanently remove an authorized application.",
        annotations(destructive_hint = true)
    )]
    async fn app_remove(&self, Parameters(input): Parameters<AppInput>) -> CallToolResult {
        match self
            .backend
            .call(move |daemon| daemon.remove(&input.app))
            .await
        {
            Ok(()) => result(json!({ "removed": true })),
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Run a shell command as the MCP process user, never as the daemon user.",
        annotations(destructive_hint = true, open_world_hint = true)
    )]
    async fn exec_command(&self, Parameters(input): Parameters<ExecInput>) -> CallToolResult {
        match execute(input).await {
            Ok(value) => result(value),
            Err(error) => tool_error(error),
        }
    }
}

#[tool_handler(
    name = "asc-daemon",
    version = "0.9.0",
    instructions = "Manage only applications visible to this MCP process user. Destructive tools should be confirmed before calling."
)]
impl ServerHandler for McpServer {}

async fn execute(input: ExecInput) -> Result<Value> {
    if input.command.trim().is_empty() {
        anyhow::bail!("command must not be empty");
    }
    if let Some(cwd) = input.cwd.as_deref()
        && !Path::new(cwd).is_dir()
    {
        anyhow::bail!("working directory does not exist: {cwd}");
    }
    let timeout_secs = input
        .timeout_secs
        .unwrap_or(DEFAULT_EXEC_TIMEOUT_SECS)
        .min(MAX_EXEC_TIMEOUT_SECS);
    let mut command = tokio::process::Command::new("/bin/sh");
    command.arg("-lc").arg(&input.command);
    if let Some(cwd) = input.cwd {
        command.current_dir(cwd);
    }
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());
    let mut child = command.spawn().context("cannot spawn command")?;
    let stdout = child
        .stdout
        .take()
        .context("command stdout was not captured")?;
    let stderr = child
        .stderr
        .take()
        .context("command stderr was not captured")?;
    let stdout_task = tokio::spawn(read_limited(stdout));
    let stderr_task = tokio::spawn(read_limited(stderr));
    let (status, timed_out) =
        match tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait()).await {
            Ok(status) => (status.context("cannot wait for command")?, false),
            Err(_) => {
                child
                    .kill()
                    .await
                    .context("cannot terminate timed-out command")?;
                (
                    child
                        .wait()
                        .await
                        .context("cannot reap timed-out command")?,
                    true,
                )
            }
        };
    let (stdout, stdout_truncated) = stdout_task.await.context("stdout reader panicked")??;
    let (stderr, stderr_truncated) = stderr_task.await.context("stderr reader panicked")??;
    Ok(json!({
        "exit_code": status.code(),
        "stdout": String::from_utf8_lossy(&stdout),
        "stderr": String::from_utf8_lossy(&stderr),
        "timed_out": timed_out,
        "stdout_truncated": stdout_truncated,
        "stderr_truncated": stderr_truncated,
    }))
}

async fn read_limited<R: AsyncRead + Unpin>(mut reader: R) -> Result<(Vec<u8>, bool)> {
    let mut result = Vec::new();
    let mut buffer = [0_u8; 8192];
    let mut truncated = false;
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let remaining = MAX_EXEC_OUTPUT_BYTES.saturating_sub(result.len());
        let copied = remaining.min(read);
        result.extend_from_slice(&buffer[..copied]);
        truncated |= copied < read;
    }
    Ok((result, truncated))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_limit_is_one_mebibyte() {
        assert_eq!(MAX_EXEC_OUTPUT_BYTES, 1024 * 1024);
    }

    #[test]
    fn destructive_and_scoped_tools_advertise_their_contract() {
        let remove = McpServer::app_remove_tool_attr();
        assert_eq!(
            remove
                .annotations
                .as_ref()
                .and_then(|annotations| annotations.destructive_hint),
            Some(true)
        );
        let info = McpServer::app_info_tool_attr();
        assert!(
            info.input_schema
                .get("properties")
                .unwrap()
                .get("uid")
                .is_none()
        );
        assert!(
            info.input_schema
                .get("properties")
                .unwrap()
                .get("user")
                .is_none()
        );
    }

    #[tokio::test]
    async fn exec_rejects_missing_directory() {
        let error = execute(ExecInput {
            command: "true".into(),
            cwd: Some("/definitely/not/an/asc-directory".into()),
            timeout_secs: None,
        })
        .await
        .unwrap_err();
        assert!(error.to_string().contains("working directory"));
    }

    #[tokio::test]
    async fn exec_uses_the_process_uid_and_honors_timeout() {
        let uid = execute(ExecInput {
            command: "id -u".into(),
            cwd: None,
            timeout_secs: Some(1),
        })
        .await
        .unwrap();
        assert_eq!(
            uid["stdout"].as_str().unwrap().trim(),
            unsafe { libc::geteuid() }.to_string()
        );

        let timed_out = execute(ExecInput {
            command: "sleep 1".into(),
            cwd: None,
            timeout_secs: Some(0),
        })
        .await
        .unwrap();
        assert_eq!(timed_out["timed_out"], true);
    }

    #[tokio::test]
    async fn exec_caps_stdout_without_blocking_the_child() {
        let output = execute(ExecInput {
            command: "yes x | head -c 1100000".into(),
            cwd: None,
            timeout_secs: Some(10),
        })
        .await
        .unwrap();
        assert_eq!(
            output["stdout"].as_str().unwrap().len(),
            MAX_EXEC_OUTPUT_BYTES
        );
        assert_eq!(output["stdout_truncated"], true);
    }
}
