//! MCP front-end tests: the compiled server driven over stdio.
//!
//! The framing moved to rmcp behind the era guard in 0.2.0 (WS2 of
//! the MCP standardization program), so these tests drive the real
//! binary as a subprocess — the same surface a client sees.
//! Responses are correlated by JSON-RPC id, never positionally:
//! rmcp dispatches pipelined requests concurrently and may answer
//! out of order (legal JSON-RPC; the hand-rolled baseline answered
//! strictly sequentially).
//!
//! Validation: discover negotiation, tools/list shape, ping,
//! attention round trip with identity binding. Adversarial:
//! legacy contact, malformed lines, oversized lines, unknown
//! methods, calls without `_meta`, dead brokers, unknown tools,
//! and bad arguments — the server must answer in-band and stay
//! alive.

mod common;

use common::{Behavior, spawn_fake_editor, temp_socket};
use serde_json::{Value as JsonValue, json};
use std::collections::HashMap;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

struct Client {
    child: tokio::process::Child,
    pending: HashMap<JsonValue, JsonValue>,
    receiver: mpsc::UnboundedReceiver<JsonValue>,
    stdin: tokio::process::ChildStdin,
}

fn modern_meta(client_name: Option<&str>) -> JsonValue {
    let mut meta = json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {},
    });
    if let Some(name) = client_name {
        meta["io.modelcontextprotocol/clientInfo"] = json!({"name": name, "version": "test"});
    }
    meta
}

async fn spawn_server(extra_args: &[String]) -> Client {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_periscope"));
    command
        .arg("--workspace")
        .arg(std::env::temp_dir())
        .args(extra_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = command.spawn().expect("spawn periscope");
    let stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let (sender, receiver) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Ok(value) = serde_json::from_str::<JsonValue>(&line) {
                let _ = sender.send(value);
            }
        }
    });
    Client {
        child,
        pending: HashMap::new(),
        receiver,
        stdin,
    }
}

fn dead_broker_args() -> Vec<String> {
    vec![
        "--attach".to_string(),
        "/nonexistent/periscope-test.sock".to_string(),
    ]
}

impl Client {
    async fn send(&mut self, message: &JsonValue) {
        let mut text = message.to_string();
        text.push('\n');
        self.stdin.write_all(text.as_bytes()).await.expect("send");
        self.stdin.flush().await.expect("flush");
    }

    async fn send_raw(&mut self, text: &str) {
        self.stdin
            .write_all(text.as_bytes())
            .await
            .expect("send raw");
        self.stdin.flush().await.expect("flush raw");
    }

    /// Read responses until the one carrying `id` arrives; any
    /// other response is buffered for its own waiter.
    async fn receive_for(&mut self, id: &JsonValue) -> JsonValue {
        if let Some(response) = self.pending.remove(id) {
            return response;
        }
        loop {
            let response =
                tokio::time::timeout(std::time::Duration::from_secs(30), self.receiver.recv())
                    .await
                    .expect("response within deadline")
                    .expect("a response line");
            if response.get("id") == Some(id) {
                return response;
            }
            if let Some(other) = response.get("id").cloned() {
                self.pending.insert(other, response);
            }
        }
    }

    async fn discover(&mut self, id: u64) -> JsonValue {
        self.send(
            &json!({"jsonrpc": "2.0", "id": id, "method": "server/discover",
                          "params": {"_meta": modern_meta(Some("periscope-test"))}}),
        )
        .await;
        self.receive_for(&json!(id)).await
    }

    async fn call(&mut self, id: u64, name: &str, arguments: JsonValue) -> JsonValue {
        self.call_as(id, name, arguments, Some("periscope-test"))
            .await
    }

    async fn call_as(
        &mut self,
        id: u64,
        name: &str,
        arguments: JsonValue,
        client_name: Option<&str>,
    ) -> JsonValue {
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
                          "params": {"name": name, "arguments": arguments,
                                     "_meta": modern_meta(client_name)}}))
            .await;
        self.receive_for(&json!(id)).await
    }

    async fn finish(mut self) {
        drop(self.stdin);
        let status = tokio::time::timeout(std::time::Duration::from_secs(10), self.child.wait())
            .await
            .expect("server exits at EOF")
            .expect("wait");
        assert!(status.success(), "clean exit, got {status}");
    }
}

fn tool_text(response: &JsonValue) -> &str {
    response["result"]["content"][0]["text"]
        .as_str()
        .expect("text content")
}

#[tokio::test]
async fn discover_negotiates_and_identifies() {
    let mut client = spawn_server(&dead_broker_args()).await;
    let response = client.discover(1).await;
    assert_eq!(
        response["result"]["supportedVersions"],
        json!(["2026-07-28"])
    );
    // The modern discover result carries the server identity in
    // its `_meta`, not in a legacy `serverInfo` field.
    assert_eq!(
        response["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        json!("periscope")
    );
    client.finish().await;
}

#[tokio::test]
async fn legacy_initialize_is_refused_and_the_server_survives() {
    let mut client = spawn_server(&dead_broker_args()).await;
    for (id, version) in [(1, "2025-06-18"), (2, "2024-11-05"), (3, "2026-07-28")] {
        client
            .send(&json!({"jsonrpc": "2.0", "id": id, "method": "initialize",
                          "params": {"protocolVersion": version,
                                     "clientInfo": {"name": "legacy", "version": "0"}}}))
            .await;
        let response = client.receive_for(&json!(id)).await;
        assert_eq!(response["error"]["code"], json!(-32022), "frame {id}");
        assert_eq!(
            response["error"]["data"]["supported"],
            json!(["2026-07-28"])
        );
        assert_eq!(response["error"]["data"]["requested"], json!(version));
    }
    // The same process still serves the modern era afterwards.
    let response = client.discover(4).await;
    assert_eq!(
        response["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        json!("periscope")
    );
    client.finish().await;
}

#[tokio::test]
async fn tools_list_is_complete_and_sorted() {
    let mut client = spawn_server(&dead_broker_args()).await;
    client.discover(1).await;
    client
        .send(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list",
                      "params": {"_meta": modern_meta(Some("periscope-test"))}}))
        .await;
    let response = client.receive_for(&json!(2)).await;
    let tools = response["result"]["tools"].as_array().expect("tools array");
    let names: Vec<&str> = tools
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert_eq!(
        names,
        [
            "attention_alignment",
            "attention_decide",
            "attention_focus",
            "attention_publish",
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
async fn requests_without_meta_are_refused() {
    let mut client = spawn_server(&dead_broker_args()).await;
    client
        .send(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}}))
        .await;
    let response = client.receive_for(&json!(1)).await;
    assert_eq!(response["error"]["code"], json!(-32602));
    client.finish().await;
}

#[tokio::test]
async fn malformed_line_gets_parse_error_and_server_survives() {
    let mut client = spawn_server(&dead_broker_args()).await;
    client.send_raw("{not json\n").await;
    let response = client.receive_for(&JsonValue::Null).await;
    assert_eq!(response["error"]["code"], json!(-32700));
    assert_eq!(response["error"]["message"], json!("parse error"));
    client.discover(1).await;
    client
        .send(&json!({"jsonrpc": "2.0", "id": 7, "method": "ping",
                      "params": {"_meta": modern_meta(Some("periscope-test"))}}))
        .await;
    let response = client.receive_for(&json!(7)).await;
    assert_eq!(response["result"], json!({}));
    client.finish().await;
}

#[tokio::test]
async fn unknown_method_is_method_not_found() {
    let mut client = spawn_server(&dead_broker_args()).await;
    client.discover(1).await;
    client
        .send(
            &json!({"jsonrpc": "2.0", "id": 3, "method": "resources/list",
                      "params": {"_meta": modern_meta(Some("periscope-test"))}}),
        )
        .await;
    let response = client.receive_for(&json!(3)).await;
    assert_eq!(response["error"]["code"], json!(-32601));
    assert_eq!(
        response["error"]["message"],
        json!("method not found: resources/list")
    );
    client.finish().await;
}

#[tokio::test]
async fn notification_produces_no_response() {
    let mut client = spawn_server(&dead_broker_args()).await;
    client.discover(1).await;
    client
        .send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
        .await;
    client
        .send(&json!({"jsonrpc": "2.0", "id": 9, "method": "ping",
                      "params": {"_meta": modern_meta(Some("periscope-test"))}}))
        .await;
    let response = client.receive_for(&json!(9)).await;
    assert_eq!(response["result"], json!({}));
    client.finish().await;
}

#[tokio::test]
async fn nameless_call_is_an_inband_tool_error() {
    let mut client = spawn_server(&dead_broker_args()).await;
    client.discover(1).await;
    client
        .send(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                      "params": {"arguments": {}, "_meta": modern_meta(Some("periscope-test"))}}))
        .await;
    let response = client.receive_for(&json!(2)).await;
    assert_eq!(response["result"]["isError"], json!(true));
    assert_eq!(tool_text(&response), "tools/call requires a string `name`");
    client.finish().await;
}

#[tokio::test]
async fn tools_call_with_dead_broker_is_a_tool_error() {
    let mut client = spawn_server(&dead_broker_args()).await;
    client.discover(1).await;
    let response = client.call(4, "broker_status", json!({})).await;
    assert_eq!(response["result"]["isError"], json!(true));
    let text = tool_text(&response);
    assert!(text.contains("connecting to neovim failed"), "got: {text}");
    client.finish().await;
}

#[tokio::test]
async fn attention_round_trip_binds_to_the_caller_identity() {
    let mut client = spawn_server(&dead_broker_args()).await;
    client.discover(1).await;
    let focus = json!({"agent_id": "periscope-test", "snapshot_hash": "frame-a",
                       "seq": 1, "targets": ["src/a.rs", "src/b.rs"]});
    let response = client
        .call(2, "attention_publish", json!({"focus": focus}))
        .await;
    assert_eq!(response["result"]["isError"], json!(false));
    // Spoofing: the payload names someone else.
    let spoof = json!({"agent_id": "mallory", "snapshot_hash": "frame-a",
                       "seq": 1, "targets": ["src/a.rs"]});
    let response = client
        .call(3, "attention_publish", json!({"focus": spoof}))
        .await;
    assert_eq!(response["result"]["isError"], json!(true));
    assert!(tool_text(&response).contains("does not match the caller identity"));
    // Read back, align with self, decide against self.
    let response = client
        .call(4, "attention_focus", json!({"agent_id": "periscope-test"}))
        .await;
    assert!(tool_text(&response).contains("\"snapshot_hash\":\"frame-a\""));
    let response = client
        .call(
            5,
            "attention_alignment",
            json!({"agent_id": "periscope-test"}),
        )
        .await;
    assert!(tool_text(&response).contains("\"score\":1.0"));
    let response = client
        .call(6, "attention_decide", json!({"agent_id": "periscope-test"}))
        .await;
    assert!(tool_text(&response).contains("\"decision\":\"serialize\""));
    // No clientInfo in _meta: identity-bound tools fail closed.
    let response = client
        .call_as(
            7,
            "attention_focus",
            json!({"agent_id": "periscope-test"}),
            None,
        )
        .await;
    assert_eq!(response["result"]["isError"], json!(true));
    assert!(tool_text(&response).contains("require an identified client"));
    client.finish().await;
}

#[tokio::test]
async fn tools_call_unknown_tool_and_bad_arguments_are_tool_errors() {
    let socket = temp_socket("protocol-live");
    spawn_fake_editor(socket.clone(), Behavior::Normal);
    let mut client = spawn_server(&attach_args(&socket)).await;
    client.discover(1).await;
    let response = client.call(4, "no_such_tool", json!({})).await;
    assert_eq!(response["result"]["isError"], json!(true));
    let text = tool_text(&response);
    assert!(text.contains("unknown tool"), "got: {text}");
    let response = client.call(5, "get_hover", json!({})).await;
    assert_eq!(response["result"]["isError"], json!(true));
    let text = tool_text(&response);
    assert!(text.contains("path"), "got: {text}");
    client.finish().await;
}

fn attach_args(socket: &std::path::Path) -> Vec<String> {
    vec![
        "--attach".to_string(),
        socket.to_string_lossy().into_owned(),
    ]
}

#[tokio::test]
async fn run_tools_round_trip_against_fake_editor() {
    let socket = temp_socket("protocol-exec");
    spawn_fake_editor(socket.clone(), Behavior::Normal);
    let mut client = spawn_server(&attach_args(&socket)).await;
    client.discover(1).await;
    let file = std::env::temp_dir().join("periscope-protocol-target.txt");
    std::fs::write(&file, b"hello\n").expect("write target file");
    for (id, name) in [(4, "run_format"), (5, "run_lint")] {
        let response = client
            .call(id, name, json!({"path": file.to_string_lossy()}))
            .await;
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
    let mut client = spawn_server(&attach_args(&socket)).await;
    client.discover(1).await;
    // Each of these must fail argument validation in the server
    // itself, before any Lua is executed in the editor; a call that
    // got past validation would return the fake editor's echo
    // payload instead of a validation error.
    let cases: Vec<(u64, JsonValue, &str)> = vec![
        (4, json!({"path": "/nonexistent/x.rs"}), "path"),
        (
            5,
            json!({"path": "/etc/hostname", "formatters": []}),
            "empty",
        ),
        (
            6,
            json!({"path": "/etc/hostname", "formatters": ["ok"], "lsp": "only"}),
            "conflicts",
        ),
        (
            7,
            json!({"path": "/etc/hostname", "lsp": "sometimes"}),
            "lsp",
        ),
        (
            8,
            json!({"path": "/etc/hostname", "linters": ["bad name!"]}),
            "invalid name",
        ),
        (
            9,
            json!({"path": "/etc/hostname",
                   "linters": (0..33).map(|i| format!("linter{i}")).collect::<Vec<_>>() }),
            "at most",
        ),
    ];
    let names = [
        "run_format",
        "run_format",
        "run_format",
        "run_format",
        "run_lint",
        "run_lint",
    ];
    for ((id, arguments, needle), name) in cases.into_iter().zip(names) {
        let response = client.call(id, name, arguments).await;
        assert_eq!(
            response["result"]["isError"],
            json!(true),
            "case id {id}: {response}"
        );
        let text = tool_text(&response);
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
async fn pipelined_requests_are_correlated_by_id() {
    let mut client = spawn_server(&dead_broker_args()).await;
    client.discover(1).await;
    // Three requests in flight at once; rmcp may answer them out of
    // order — the ids, not the positions, carry the correlation.
    for (id, name) in [
        (10, "attention_focus"),
        (11, "attention_focus"),
        (12, "attention_focus"),
    ] {
        client
            .send(&json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
                          "params": {"name": name,
                                     "arguments": {"agent_id": "nobody"},
                                     "_meta": modern_meta(Some("periscope-test"))}}))
            .await;
    }
    for id in [12u64, 10, 11] {
        let response = client.receive_for(&json!(id)).await;
        assert_eq!(response["id"], json!(id));
        assert_eq!(response["result"]["isError"], json!(false));
    }
    client.finish().await;
}

#[tokio::test]
async fn oversized_line_is_rejected_and_server_survives() {
    let mut client = spawn_server(&dead_broker_args()).await;
    let oversized = "a".repeat(4 * 1024 * 1024 + 1);
    client.send_raw(&oversized).await;
    client.send_raw("\n").await;
    let response = client.receive_for(&JsonValue::Null).await;
    assert_eq!(response["error"]["code"], json!(-32700));
    assert_eq!(
        response["error"]["message"],
        json!("message exceeds size limit")
    );
    client.discover(1).await;
    client.finish().await;
}
