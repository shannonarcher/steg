//! End-to-end: the real `steg` binary driving a real, headless
//! `wezterm-mux-server` through the real mux protocol.
//!
//! This is the same code path as driving `wezterm-gui`; only the process
//! on the other end of the socket differs, so it is as close to the GUI
//! experience as a test without a display can get.
//!
//! The test needs the `wezterm-mux-server` binary.  It looks for it next
//! to the `steg` test binary (so `cargo build -p wezterm-mux-server` first)
//! or at `$STEG_MUX_SERVER`, and skips with a note when neither exists, so
//! a plain `cargo test -p steg` still passes everywhere.

#![cfg(unix)]

use rmcp::model::{
    CallToolRequestParams, CallToolResult, ReadResourceRequestParams, ResourceContents,
};
use rmcp::service::RunningService;
use rmcp::transport::TokioChildProcess;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

type Client = RunningService<RoleClient, ()>;

fn mux_server_binary() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("STEG_MUX_SERVER") {
        return Some(PathBuf::from(p));
    }
    let steg = Path::new(env!("CARGO_BIN_EXE_steg"));
    let candidate = steg.with_file_name("wezterm-mux-server");
    candidate.exists().then_some(candidate)
}

/// A headless mux server listening on a socket in a temp dir, killed on drop.
struct MuxServer {
    child: std::process::Child,
    _dir: tempfile::TempDir,
    socket: PathBuf,
    config_file: PathBuf,
}

impl MuxServer {
    fn start(binary: &Path) -> MuxServer {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("steg-e2e.sock");
        let config_file = dir.path().join("wezterm.lua");
        std::fs::write(
            &config_file,
            format!(
                r#"
local config = {{}}
config.unix_domains = {{ {{ name = "steg-e2e", socket_path = "{sock}" }} }}
config.default_prog = {{ "/bin/bash", "--noprofile", "--norc" }}
config.check_for_updates = false
config.automatically_reload_config = false
config.initial_rows = 24
config.initial_cols = 100
return config
"#,
                sock = socket.display()
            ),
        )
        .expect("write wezterm.lua");

        let child = std::process::Command::new(binary)
            .arg("--config-file")
            .arg(&config_file)
            .env("HOME", dir.path())
            .env("XDG_RUNTIME_DIR", dir.path())
            .env_remove("WEZTERM_UNIX_SOCKET")
            .env_remove("WEZTERM_PANE")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn wezterm-mux-server");

        let started = Instant::now();
        while !socket.exists() {
            assert!(
                started.elapsed() < Duration::from_secs(20),
                "mux server did not create {}",
                socket.display()
            );
            std::thread::sleep(Duration::from_millis(100));
        }

        MuxServer {
            child,
            _dir: dir,
            socket,
            config_file,
        }
    }
}

impl Drop for MuxServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

async fn connect_steg(mux: &MuxServer) -> Client {
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_steg"));
    cmd.arg("--config-file")
        .arg(&mux.config_file)
        .arg("--prefer-mux")
        .env("WEZTERM_UNIX_SOCKET", &mux.socket)
        .env_remove("WEZTERM_PANE")
        .stderr(Stdio::inherit());
    ().serve(TokioChildProcess::new(cmd).expect("spawn steg"))
        .await
        .expect("steg handshake")
}

fn args(v: Value) -> Map<String, Value> {
    v.as_object().expect("arguments must be an object").clone()
}

fn text_of(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

async fn call(client: &Client, name: &'static str, arguments: Value) -> CallToolResult {
    let result = client
        .call_tool(CallToolRequestParams::new(name).with_arguments(args(arguments)))
        .await
        .unwrap_or_else(|e| panic!("calling {name}: {e:#}"));
    assert_ne!(
        result.is_error,
        Some(true),
        "{name} failed: {}",
        text_of(&result)
    );
    result
}

fn structured(result: &CallToolResult) -> Value {
    result
        .structured_content
        .clone()
        .unwrap_or_else(|| serde_json::from_str(&text_of(result)).expect("json tool result"))
}

async fn panes(client: &Client) -> Vec<Value> {
    structured(&call(client, "list_panes", json!({})).await)["panes"]
        .as_array()
        .cloned()
        .expect("list_panes returns {panes: [...]}")
}

#[tokio::test]
async fn drive_a_shell_through_the_mux() {
    let Some(binary) = mux_server_binary() else {
        eprintln!(
            "skipping e2e_mux: wezterm-mux-server not found; \
             run `cargo build -p wezterm-mux-server` or set STEG_MUX_SERVER"
        );
        return;
    };

    let mux = MuxServer::start(&binary);
    let client = connect_steg(&mux).await;

    // The server identifies itself and, with a terminal present, lists
    // whatever the mux server started with.
    let info = client.peer_info().expect("peer info");
    assert_eq!(info.server_info.as_ref().unwrap().name, "steg");
    let before = panes(&client).await;

    // Spawn a tab running a bare bash in a fresh window.
    let spawned = structured(
        &call(
            &client,
            "spawn_tab",
            json!({
                "command": ["/bin/bash", "--noprofile", "--norc"],
                "new_window": true,
                "cwd": "/"
            }),
        )
        .await,
    );
    let pane_id = spawned["pane_id"].as_u64().expect("pane_id") as usize;
    let tab_id = spawned["tab_id"].as_u64().expect("tab_id");

    let after = panes(&client).await;
    assert_eq!(after.len(), before.len() + 1, "one new pane: {after:#?}");
    let me = after
        .iter()
        .find(|p| p["pane_id"].as_u64() == Some(pane_id as u64))
        .expect("our pane is listed");
    assert_eq!(me["tab_id"].as_u64(), Some(tab_id));
    assert_eq!(me["is_active"], json!(true));
    assert_eq!(me["rows"], json!(24));
    assert_eq!(me["cols"], json!(100));

    // Type a command, press Enter, wait for the shell to settle, read it back.
    call(
        &client,
        "send_text",
        json!({ "pane_id": pane_id, "text": "echo steg-e2e-$((40+2))", "enter": true }),
    )
    .await;
    let idle = structured(
        &call(
            &client,
            "wait_for_idle",
            json!({ "pane_id": pane_id, "quiet_ms": 600, "timeout_ms": 20000 }),
        )
        .await,
    );
    assert_eq!(idle["idle"], json!(true), "{idle:#?}");
    let screen = idle["screen"].as_str().unwrap();
    assert!(screen.contains("steg-e2e-42"), "screen:\n{screen}");

    let text = text_of(&call(&client, "get_text", json!({ "pane_id": pane_id })).await);
    assert!(text.contains("steg-e2e-42"), "get_text:\n{text}");
    assert!(!text.ends_with('\n'), "trailing blank rows are trimmed");

    // The same screen is readable as a resource.
    let res = client
        .read_resource(ReadResourceRequestParams::new(format!(
            "wezterm://pane/{pane_id}/screen"
        )))
        .await
        .expect("read screen resource");
    let resource_text = res
        .contents
        .iter()
        .find_map(|c| match c {
            ResourceContents::TextResourceContents { text, .. } => Some(text.clone()),
            _ => None,
        })
        .expect("text resource");
    assert!(resource_text.contains("steg-e2e-42"));

    // Scrollback: push a lot of output off the screen, then read it back.
    call(
        &client,
        "send_text",
        json!({ "pane_id": pane_id, "text": "for i in $(seq 1 60); do echo line-$i; done", "enter": true }),
    )
    .await;
    call(
        &client,
        "wait_for_idle",
        json!({ "pane_id": pane_id, "quiet_ms": 600, "timeout_ms": 20000 }),
    )
    .await;
    let visible = text_of(&call(&client, "get_text", json!({ "pane_id": pane_id })).await);
    assert!(
        !visible.contains("line-1\n"),
        "line-1 should have scrolled off a 24-row screen:\n{visible}"
    );
    let history = text_of(
        &call(
            &client,
            "get_text",
            json!({ "pane_id": pane_id, "start_line": -80 }),
        )
        .await,
    );
    assert!(history.contains("line-1\n"), "scrollback:\n{history}");
    assert!(history.contains("steg-e2e-42"), "scrollback:\n{history}");
    let scrollback_res = client
        .read_resource(ReadResourceRequestParams::new(format!(
            "wezterm://pane/{pane_id}/scrollback"
        )))
        .await
        .expect("read scrollback resource");
    assert!(matches!(
        &scrollback_res.contents[0],
        ResourceContents::TextResourceContents { text, .. } if text.contains("line-1\n")
    ));

    // Keys: Ctrl-C interrupts a sleep, and the shell is responsive again.
    call(
        &client,
        "send_text",
        json!({ "pane_id": pane_id, "text": "sleep 30", "enter": true }),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    call(
        &client,
        "send_key",
        json!({ "pane_id": pane_id, "key": "c", "mods": "CTRL" }),
    )
    .await;
    call(
        &client,
        "send_text",
        json!({ "pane_id": pane_id, "text": "echo after-interrupt", "enter": true }),
    )
    .await;
    let idle = structured(
        &call(
            &client,
            "wait_for_idle",
            json!({ "pane_id": pane_id, "quiet_ms": 600, "timeout_ms": 20000 }),
        )
        .await,
    );
    assert!(
        idle["screen"].as_str().unwrap().contains("after-interrupt"),
        "{idle:#?}"
    );

    // Tab title round-trips through list_panes.
    call(
        &client,
        "set_tab_title",
        json!({ "pane_id": pane_id, "title": "steg e2e" }),
    )
    .await;
    let me = panes(&client)
        .await
        .into_iter()
        .find(|p| p["pane_id"].as_u64() == Some(pane_id as u64))
        .unwrap();
    assert_eq!(me["tab_title"], json!("steg e2e"));

    // Split, then both panes share the tab.
    let split = structured(
        &call(
            &client,
            "split_pane",
            json!({
                "pane_id": pane_id,
                "side": "right",
                "percent": 40,
                "command": ["/bin/bash", "--noprofile", "--norc"]
            }),
        )
        .await,
    );
    let right_id = split["pane_id"].as_u64().expect("split pane_id") as usize;
    assert_ne!(right_id, pane_id);
    assert_eq!(split["tab_id"].as_u64(), Some(tab_id));
    let in_tab: Vec<usize> = panes(&client)
        .await
        .iter()
        .filter(|p| p["tab_id"].as_u64() == Some(tab_id))
        .map(|p| p["pane_id"].as_u64().unwrap() as usize)
        .collect();
    assert_eq!(in_tab.len(), 2, "{in_tab:?}");

    // Focus moves to whichever pane we activate.
    call(&client, "activate_pane", json!({ "pane_id": pane_id })).await;
    let me = panes(&client)
        .await
        .into_iter()
        .find(|p| p["pane_id"].as_u64() == Some(pane_id as u64))
        .unwrap();
    assert_eq!(me["is_active"], json!(true));

    // Kill both; they disappear from the listing.
    call(&client, "kill_pane", json!({ "pane_id": right_id })).await;
    call(&client, "kill_pane", json!({ "pane_id": pane_id })).await;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = panes(&client).await;
        let gone = !remaining
            .iter()
            .any(|p| matches!(p["pane_id"].as_u64(), Some(id) if id as usize == pane_id || id as usize == right_id));
        if gone {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "panes still listed: {remaining:#?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    client.cancel().await.expect("cancel");
}

#[tokio::test]
async fn reports_a_dead_pane_clearly() {
    let Some(binary) = mux_server_binary() else {
        eprintln!("skipping e2e_mux: wezterm-mux-server not found");
        return;
    };

    let mux = MuxServer::start(&binary);
    let client = connect_steg(&mux).await;

    let result = client
        .call_tool(
            CallToolRequestParams::new("get_text")
                .with_arguments(args(json!({ "pane_id": 999_999 }))),
        )
        .await
        .expect("call completes");
    assert_eq!(result.is_error, Some(true));
    assert!(
        text_of(&result).contains("999999"),
        "error names the pane: {}",
        text_of(&result)
    );

    let result = client
        .call_tool(
            CallToolRequestParams::new("activate_tab")
                .with_arguments(args(json!({ "tab_id": 999_999 }))),
        )
        .await
        .expect("call completes");
    assert_eq!(result.is_error, Some(true));
    assert!(text_of(&result).contains("list_panes"));

    client.cancel().await.expect("cancel");
}
