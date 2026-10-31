//! The MCP surface, exercised through a real MCP client over an in-process
//! pipe.  No WezTerm is needed.
//!
//! These tests pin down what an agent sees on connect (server identity,
//! instructions, the tool list and its schemas, resource templates) and
//! that every failure mode is a clean, explanatory tool error rather than a
//! crash when no terminal is running.  The happy path against a live mux is
//! covered by `e2e_mux.rs`.

use rmcp::model::{CallToolRequestParams, CallToolResult, ReadResourceRequestParams};
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{json, Map, Value};
use std::sync::OnceLock;
use steg::{Backend, ConnectOptions, Steg};

/// Point wezterm at a socket that cannot exist, so that connecting fails
/// fast and deterministically whatever is installed on this machine.
fn no_terminal_anywhere() {
    static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
    let dir = DIR.get_or_init(|| tempfile::tempdir().expect("tempdir"));
    std::env::set_var(
        "WEZTERM_UNIX_SOCKET",
        dir.path().join("no-such-wezterm.sock"),
    );
}

type Client = RunningService<RoleClient, ()>;

/// Start a `Steg` server and an MCP client joined by an in-memory pipe.
async fn connect() -> (Client, tokio::task::JoinHandle<()>) {
    no_terminal_anywhere();

    let backend = Backend::new(
        ConnectOptions {
            class: "org.wezfurlong.wezterm".to_string(),
            prefer_mux: false,
            no_auto_start: true,
        },
        config::configuration(),
    );
    let server = Steg::new(backend);

    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let server_task = tokio::spawn(async move {
        let running = server.serve(server_io).await.expect("server handshake");
        running.waiting().await.expect("server task");
    });

    let client = ().serve(client_io).await.expect("client handshake");
    (client, server_task)
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
    client
        .call_tool(CallToolRequestParams::new(name).with_arguments(args(arguments)))
        .await
        .unwrap_or_else(|e| panic!("calling {name}: {e:#}"))
}

#[tokio::test]
async fn handshake_identifies_steg_and_teaches_the_flow() {
    let (client, server) = connect().await;

    let info = client.peer_info().expect("server info after initialize");
    let server_info = info.server_info.as_ref().expect("server_info");
    assert_eq!(server_info.name, "steg");
    assert!(info.capabilities.tools.is_some(), "tools capability");
    assert!(
        info.capabilities.resources.is_some(),
        "resources capability"
    );

    let instructions = info.instructions.as_deref().unwrap_or_default();
    for step in [
        "list_panes",
        "spawn_tab",
        "send_text",
        "wait_for_idle",
        "get_text",
    ] {
        assert!(
            instructions.contains(step),
            "instructions should mention {step}; got:\n{instructions}"
        );
    }

    client.cancel().await.expect("cancel");
    server.await.unwrap();
}

#[tokio::test]
async fn tool_list_is_complete_and_documented() {
    let (client, server) = connect().await;

    let tools = client.list_all_tools().await.expect("tools/list");
    let mut names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec![
            "activate_pane",
            "activate_tab",
            "get_text",
            "kill_pane",
            "list_panes",
            "send_key",
            "send_text",
            "set_tab_title",
            "spawn_tab",
            "split_pane",
            "wait_for_idle",
        ]
    );

    for tool in &tools {
        let description = tool.description.as_deref().unwrap_or_default();
        assert!(
            description.len() > 20,
            "{} needs a description an agent can act on",
            tool.name
        );
        assert_eq!(
            tool.input_schema.get("type"),
            Some(&json!("object")),
            "{} input schema must be an object",
            tool.name
        );
    }

    let find = |name: &str| tools.iter().find(|t| t.name == name).unwrap();

    // The schemas carry the field docs: that is what the agent reads.
    let spawn = find("spawn_tab");
    let props = spawn.input_schema["properties"].as_object().unwrap();
    for field in [
        "command",
        "cwd",
        "new_window",
        "window_id",
        "pane_id",
        "workspace",
        "domain",
    ] {
        assert!(props.contains_key(field), "spawn_tab.{field}");
    }
    assert!(
        props["command"]["description"]
            .as_str()
            .unwrap()
            .contains("argv"),
        "command is documented as argv"
    );

    let send_text = find("send_text");
    let required: Vec<&str> = send_text.input_schema["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(required.contains(&"pane_id") && required.contains(&"text"));
    assert!(
        !required.contains(&"enter"),
        "enter is optional and defaults to false"
    );

    // Structured results advertise an output schema.
    for name in ["list_panes", "spawn_tab", "split_pane", "wait_for_idle"] {
        assert!(
            find(name).output_schema.is_some(),
            "{name} has an output schema"
        );
    }

    client.cancel().await.expect("cancel");
    server.await.unwrap();
}

#[tokio::test]
async fn resource_templates_cover_screen_and_scrollback() {
    let (client, server) = connect().await;

    let templates = client
        .list_resource_templates(None)
        .await
        .expect("resources/templates/list")
        .resource_templates;
    let mut uris: Vec<&str> = templates.iter().map(|t| t.uri_template.as_str()).collect();
    uris.sort_unstable();
    assert_eq!(
        uris,
        vec![
            "wezterm://pane/{pane_id}/screen",
            "wezterm://pane/{pane_id}/scrollback"
        ]
    );
    for t in &templates {
        assert_eq!(t.mime_type.as_deref(), Some("text/plain"));
    }

    client.cancel().await.expect("cancel");
    server.await.unwrap();
}

#[tokio::test]
async fn tools_fail_cleanly_when_no_terminal_is_running() {
    let (client, server) = connect().await;

    for (name, arguments) in [
        ("list_panes", json!({})),
        ("spawn_tab", json!({ "command": ["bash"] })),
        ("get_text", json!({ "pane_id": 1 })),
        (
            "send_text",
            json!({ "pane_id": 1, "text": "hi", "enter": true }),
        ),
        ("wait_for_idle", json!({ "pane_id": 1, "timeout_ms": 500 })),
        ("kill_pane", json!({ "pane_id": 1 })),
        ("activate_tab", json!({ "tab_id": 1 })),
    ] {
        let result = call(&client, name, arguments).await;
        assert_eq!(result.is_error, Some(true), "{name} should report an error");
        let text = text_of(&result);
        assert!(
            text.contains("WezTerm"),
            "{name} should tell the agent to start WezTerm; got: {text}"
        );
    }

    // Listing resources needs the terminal too; that surfaces as a
    // protocol error carrying the same explanation.
    let err = client
        .list_resources(None)
        .await
        .expect_err("resources/list without a terminal");
    assert!(format!("{err:#}").contains("WezTerm"), "{err:#}");

    client.cancel().await.expect("cancel");
    server.await.unwrap();
}

#[tokio::test]
async fn bad_arguments_are_rejected_before_touching_the_terminal() {
    let (client, server) = connect().await;

    let result = call(
        &client,
        "send_key",
        json!({ "pane_id": 1, "key": "hyperspace" }),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    assert!(
        text_of(&result).contains("unknown key"),
        "{}",
        text_of(&result)
    );

    let result = call(
        &client,
        "send_key",
        json!({ "pane_id": 1, "key": "c", "mods": "TURBO" }),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    assert!(
        text_of(&result).contains("invalid modifier"),
        "{}",
        text_of(&result)
    );

    let result = call(
        &client,
        "split_pane",
        json!({ "pane_id": 1, "side": "diagonal" }),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    assert!(text_of(&result).contains("left, right, top or bottom"));

    let result = call(&client, "set_tab_title", json!({ "title": "x" })).await;
    assert_eq!(result.is_error, Some(true));
    assert!(text_of(&result).contains("tab_id or pane_id"));

    // A missing required field never panics the server: it is either a
    // protocol error or a tool error, and the connection stays usable.
    let outcome = client
        .call_tool(
            CallToolRequestParams::new("send_text").with_arguments(args(json!({ "text": "x" }))),
        )
        .await;
    match outcome {
        Ok(result) => assert_eq!(result.is_error, Some(true)),
        Err(err) => assert!(format!("{err:#}").contains("pane_id"), "{err:#}"),
    }
    let still_alive = client.list_all_tools().await.expect("server still answers");
    assert_eq!(still_alive.len(), 11);

    client.cancel().await.expect("cancel");
    server.await.unwrap();
}

#[tokio::test]
async fn unknown_resource_uris_are_not_found() {
    let (client, server) = connect().await;

    for uri in [
        "wezterm://tab/1/screen",
        "wezterm://pane/one/screen",
        "wezterm://pane/1/cursor",
        "file:///etc/passwd",
    ] {
        let err = client
            .read_resource(ReadResourceRequestParams::new(uri))
            .await
            .expect_err(uri);
        let msg = format!("{err:#}");
        assert!(msg.contains("unknown resource"), "{uri}: {msg}");
    }

    client.cancel().await.expect("cancel");
    server.await.unwrap();
}
