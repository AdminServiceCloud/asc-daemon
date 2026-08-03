//! End-to-end stdio MCP test: the real `asc mcp serve` child connects to a
//! real peer-authenticated UDS listener, then serves initialize/tools/list.

use std::io::{BufRead, BufReader, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

use asc_daemon::daemon::api::{self, ApiState};
use asc_daemon::daemon::apps::AppStore;
use asc_daemon::daemon::apps::meta::{AppMeta, DesiredState, Owner, Runtime};
use asc_daemon::daemon::config::Config;

fn meta(id: &str, uid: u32) -> AppMeta {
    AppMeta {
        id: id.into(),
        uuid: None,
        name: id.into(),
        custom_name: None,
        owner: Owner {
            uid,
            name: format!("user{uid}"),
        },
        version: None,
        source: None,
        branch: None,
        package: None,
        desired_state: DesiredState::Stopped,
        quota: None,
        runtime: Runtime::Process {
            command: "true".into(),
            args: vec![],
        },
    }
}

fn spawn_uds(state: Arc<ApiState>) -> tokio::sync::oneshot::Sender<()> {
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(api::uds::serve(state, async {
                let _ = stop_rx.await;
            }))
            .unwrap();
    });
    stop_tx
}

fn wait_for_socket(path: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("socket {} never came up", path.display());
}

fn send(stdin: &mut std::process::ChildStdin, request: serde_json::Value) {
    writeln!(stdin, "{request}").unwrap();
    stdin.flush().unwrap();
}

fn response(stdout: &mut BufReader<std::process::ChildStdout>) -> serde_json::Value {
    let mut line = String::new();
    assert!(
        stdout.read_line(&mut line).unwrap() > 0,
        "MCP child closed stdout"
    );
    serde_json::from_str(&line).unwrap()
}

#[test]
fn stdio_initializes_and_lists_scoped_tools() {
    let workspace = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.daemon.data_dir = workspace.path().join("data");
    config.daemon.apps_dir = workspace.path().join("apps");
    config.api.socket = workspace.path().join("asc.sock");
    let config_path = workspace.path().join("config.toml");
    config.save_to(&config_path).unwrap();

    // SAFETY: geteuid() has no preconditions and cannot fail.
    let uid = unsafe { libc::geteuid() };
    let store = AppStore::new(config.daemon.apps_dir.clone());
    store.save(&meta("mine", uid)).unwrap();
    store.save(&meta("foreign", uid + 1)).unwrap();
    let _stop = spawn_uds(ApiState::new(config.clone(), "unused-token".into()));
    wait_for_socket(&config.api.socket);

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_asc"))
        .args(["mcp", "serve"])
        .env("ASC_CONFIG", &config_path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

    send(
        &mut stdin,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "asc-test", "version": "1" }
            }
        }),
    );
    let initialized = response(&mut stdout);
    assert_eq!(initialized["id"], 1);
    assert_eq!(initialized["result"]["serverInfo"]["name"], "asc-daemon");

    send(
        &mut stdin,
        serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    );
    send(
        &mut stdin,
        serde_json::json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} }),
    );
    let tools = response(&mut stdout);
    assert_eq!(tools["id"], 2);
    let tools = tools["result"]["tools"].as_array().unwrap();
    assert!(tools.iter().any(|tool| tool["name"] == "app_list"));
    let remove = tools
        .iter()
        .find(|tool| tool["name"] == "app_remove")
        .unwrap();
    assert_eq!(remove["annotations"]["destructiveHint"], true);
    let info = tools
        .iter()
        .find(|tool| tool["name"] == "app_info")
        .unwrap();
    assert!(info["inputSchema"]["properties"].get("uid").is_none());
    assert!(info["inputSchema"]["properties"].get("user").is_none());

    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
}
