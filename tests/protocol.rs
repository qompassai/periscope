//! MCP front-end tests: the server loop driven over in-memory
//! duplex streams.
//!
//! Validation: initialize negotiation, tools/list shape, ping.
//! Adversarial: malformed lines, oversized lines, unknown methods,
//! calls before initialize, dead brokers, unknown tools, and bad
//! arguments — the server must answer in-band and stay alive.

mod common;

use common::{Behavior, config_for, spawn_fake_editor, temp_socket};
use periscope::broker::BrokerConfig;
use serde_json::{Value as JsonValue, json};
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

struct Client {
    lines: tokio::io::Lines<BufReader<DuplexStream>>,
    task: tokio::task::JoinHandle<()>,
    writer: DuplexStream,
}

async fn spawn_server(config: BrokerConfig) -> Client {
    // Two one-directional pairs: dropping the client writer is an
    // unambiguous EOF for the server's reader.
    let (client_write, server_read) = tokio::io::duplex(256 * 1024);
    let (server_write, client_read) = tokio::io::duplex(256 * 1024);
    let task = tokio::spawn(periscope::mcp::run(server_read, server_write, config));
    Client {
        lines: BufReader::new(client_read).lines(),
        task,
        writer: client_write,
    }
}

impl Client {
    async fn send(&mut self, message: &JsonValue) {
        let mut text = message.to_string();
        text.push('\n');
        self.writer.write_all(text.as_bytes()).await.expect("send");
    }

    async fn send_raw(&mut self, text: &str) {
        self.writer
            .write_all(text.as_bytes())
            .await
            .expect("send raw");
    }

    async fn receive(&mut self) -> JsonValue {
        let line = tokio::time::timeout(std::time::Duration::from_secs(10), self.lines.next_line())
            .await
            .expect("response within deadline")
            .expect("stream readable")
            .expect("a response line");
        serde_json::from_str(&line).expect("response is JSON")
    }

    async fn finish(self) {
        drop(self.writer);
        tokio::time::timeout(std::time::Duration::from_secs(5), self.task)
            .await
            .expect("server exits at EOF")
            .expect("server task panics");
    }
}

fn dead_broker_config() -> BrokerConfig {
    BrokerConfig {
        attach_socket: Some(PathBuf::from("/nonexistent/periscope-test.sock")),
        nvim_bin: PathBuf::from("nvim"),
        workspace: std::env::temp_dir(),
    }
}

async fn initialize(client: &mut Client) {
    client
        .send(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                      "params": {"protocolVersion": "2025-03-26"}}))
        .await;
    let response = client.receive().await;
    assert_eq!(response["id"], json!(1));
}

#[tokio::test]
async fn initialize_negotiates_and_identifies() {
    let mut client = spawn_server(dead_broker_config()).await;
    initialize(&mut client).await;
    client
        .send(&json!({"jsonrpc": "2.0", "id": 2, "method": "initialize",
                      "params": {"protocolVersion": "2025-03-26"}}))
        .await;
    let response = client.receive().await;
    assert_eq!(response["result"]["protocolVersion"], json!("2025-03-26"));
    assert_eq!(response["result"]["serverInfo"]["name"], json!("periscope"));
    client.finish().await;
}

#[tokio::test]
async fn initialize_with_unknown_version_falls_back() {
    let mut client = spawn_server(dead_broker_config()).await;
    client
        .send(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                      "params": {"protocolVersion": "1999-01-01"}}))
        .await;
    let response = client.receive().await;
    assert_eq!(response["result"]["protocolVersion"], json!("2025-03-26"));
    client.finish().await;
}

#[tokio::test]
async fn tools_list_is_complete_and_sorted() {
    let mut client = spawn_server(dead_broker_config()).await;
    initialize(&mut client).await;
    client
        .send(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}))
        .await;
    let response = client.receive().await;
    let tools = response["result"]["tools"].as_array().expect("tools array");
    let names: Vec<&str> = tools
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert_eq!(
        names,
        [
            "broker_status",
            "get_definition",
            "get_diagnostics",
            "get_document_symbols",
            "get_hover",
            "get_references",
            "list_dap",
            "list_formatters",
            "list_linters",
            "list_servers",
            "run_format",
            "run_lint",
        ]
    );
    for tool in tools {
        assert!(
            tool["inputSchema"].is_object(),
            "schema for {}",
            tool["name"]
        );
        assert!(tool["description"].is_string());
    }
    client.finish().await;
}

#[tokio::test]
async fn tools_before_initialize_are_refused() {
    let mut client = spawn_server(dead_broker_config()).await;
    client
        .send(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}))
        .await;
    let response = client.receive().await;
    assert_eq!(response["error"]["code"], json!(-32002));
    client.finish().await;
}

#[tokio::test]
async fn malformed_line_gets_parse_error_and_server_survives() {
    let mut client = spawn_server(dead_broker_config()).await;
    client.send_raw("{not json\n").await;
    let response = client.receive().await;
    assert_eq!(response["error"]["code"], json!(-32700));
    initialize(&mut client).await;
    client
        .send(&json!({"jsonrpc": "2.0", "id": 7, "method": "ping"}))
        .await;
    let response = client.receive().await;
    assert_eq!(response["id"], json!(7));
    assert_eq!(response["result"], json!({}));
    client.finish().await;
}

#[tokio::test]
async fn unknown_method_is_method_not_found() {
    let mut client = spawn_server(dead_broker_config()).await;
    initialize(&mut client).await;
    client
        .send(&json!({"jsonrpc": "2.0", "id": 3, "method": "resources/list"}))
        .await;
    let response = client.receive().await;
    assert_eq!(response["error"]["code"], json!(-32601));
    client.finish().await;
}

#[tokio::test]
async fn notification_produces_no_response() {
    let mut client = spawn_server(dead_broker_config()).await;
    initialize(&mut client).await;
    client
        .send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
        .await;
    client
        .send(&json!({"jsonrpc": "2.0", "id": 9, "method": "ping"}))
        .await;
    let response = client.receive().await;
    assert_eq!(
        response["id"],
        json!(9),
        "the first answer must be the ping's"
    );
    client.finish().await;
}

#[tokio::test]
async fn tools_call_with_dead_broker_is_a_tool_error() {
    let mut client = spawn_server(dead_broker_config()).await;
    initialize(&mut client).await;
    client
        .send(&json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call",
                      "params": {"name": "broker_status", "arguments": {}}}))
        .await;
    let response = client.receive().await;
    assert_eq!(response["result"]["isError"], json!(true));
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .expect("text");
    assert!(text.contains("connecting to neovim failed"), "got: {text}");
    client.finish().await;
}

#[tokio::test]
async fn tools_call_unknown_tool_and_bad_arguments_are_tool_errors() {
    let socket = temp_socket("protocol-live");
    spawn_fake_editor(socket.clone(), Behavior::Normal);
    let mut client = spawn_server(config_for(socket)).await;
    initialize(&mut client).await;
    client
        .send(&json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call",
                      "params": {"name": "no_such_tool", "arguments": {}}}))
        .await;
    let response = client.receive().await;
    assert_eq!(response["result"]["isError"], json!(true));
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .expect("text");
    assert!(text.contains("unknown tool"), "got: {text}");
    client
        .send(&json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call",
                      "params": {"name": "get_hover", "arguments": {}}}))
        .await;
    let response = client.receive().await;
    assert_eq!(response["result"]["isError"], json!(true));
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .expect("text");
    assert!(text.contains("path"), "got: {text}");
    client.finish().await;
}

#[tokio::test]
async fn run_tools_round_trip_against_fake_editor() {
    let socket = temp_socket("protocol-exec");
    spawn_fake_editor(socket.clone(), Behavior::Normal);
    let mut client = spawn_server(config_for(socket)).await;
    initialize(&mut client).await;
    let file = std::env::temp_dir().join("periscope-protocol-target.txt");
    std::fs::write(&file, b"hello\n").expect("write target file");
    for (id, name) in [(4, "run_format"), (5, "run_lint")] {
        client
            .send(&json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
                          "params": {"name": name,
                                     "arguments": {"path": file.to_string_lossy()}}}))
            .await;
        let response = client.receive().await;
        assert_eq!(
            response["result"]["isError"],
            json!(false),
            "{name} against the fake editor: {response}"
        );
    }
    client.finish().await;
}

#[tokio::test]
async fn run_tools_reject_bad_arguments_before_any_editor_work() {
    let socket = temp_socket("protocol-exec-validation");
    spawn_fake_editor(socket.clone(), Behavior::Normal);
    let mut client = spawn_server(config_for(socket)).await;
    initialize(&mut client).await;
    // Each of these must fail argument validation in the server
    // itself, before any Lua is executed in the editor; a call that
    // got past validation would return the fake editor's echo
    // payload instead of a validation error.
    let cases: Vec<(u64, JsonValue, &str)> = vec![
        (
            4,
            json!({"name": "run_format", "arguments": {"path": "/nonexistent/x.rs"}}),
            "path",
        ),
        (
            5,
            json!({"name": "run_format", "arguments": {
                "path": "/etc/hostname", "formatters": []}}),
            "empty",
        ),
        (
            6,
            json!({"name": "run_format", "arguments": {
                "path": "/etc/hostname", "formatters": ["ok"], "lsp": "only"}}),
            "conflicts",
        ),
        (
            7,
            json!({"name": "run_format", "arguments": {
                "path": "/etc/hostname", "lsp": "sometimes"}}),
            "lsp",
        ),
        (
            8,
            json!({"name": "run_lint", "arguments": {
                "path": "/etc/hostname", "linters": ["bad name!"]}}),
            "invalid name",
        ),
        (
            9,
            json!({"name": "run_lint", "arguments": {
                "path": "/etc/hostname",
                "linters": (0..33).map(|i| format!("linter{i}")).collect::<Vec<_>>()}}),
            "at most",
        ),
    ];
    for (id, params, needle) in cases {
        client
            .send(&json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": params}))
            .await;
        let response = client.receive().await;
        assert_eq!(
            response["result"]["isError"],
            json!(true),
            "case id {id}: {response}"
        );
        let text = response["result"]["content"][0]["text"]
            .as_str()
            .expect("text");
        assert!(
            text.contains(needle),
            "case id {id} should mention {needle:?}, got: {text}"
        );
        assert!(
            !text.contains("echo"),
            "case id {id} reached the editor: {text}"
        );
    }
    client.finish().await;
}

#[tokio::test]
async fn oversized_line_is_rejected_and_server_survives() {
    let mut client = spawn_server(dead_broker_config()).await;
    let oversized = "a".repeat(4 * 1024 * 1024 + 1);
    client.send_raw(&oversized).await;
    client.send_raw("\n").await;
    let response = client.receive().await;
    assert_eq!(response["error"]["code"], json!(-32700));
    initialize(&mut client).await;
    client.finish().await;
}
