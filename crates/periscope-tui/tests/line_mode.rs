//! End-to-end line-mode smoke tests against a scripted fake MCP
//! server (a small Python program speaking NDJSON JSON-RPC), plus
//! the missing-server failure path. No real periscope server and no
//! Neovim are involved.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};

const FAKE_SERVER: &str = r#"#!/usr/bin/env python3
import json, sys

def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    try:
        message = json.loads(line)
    except Exception:
        continue
    if "id" not in message:
        continue
    ident = message["id"]
    method = message.get("method")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": ident, "result": {
            "protocolVersion": "2025-03-26",
            "capabilities": {"tools": {"listChanged": False}},
            "serverInfo": {"name": "periscope", "version": "0.1.0"}}})
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": ident, "result": {"tools": [
            {"name": "broker_status"}, {"name": "get_diagnostics"}]}})
    elif method == "tools/call":
        name = message["params"]["name"]
        if name == "broker_status":
            payload = {"mode": "attached", "nvim_version": "0.11.4",
                       "config_root": "/cfg", "cwd": "/work",
                       "workspace": "/work"}
            send({"jsonrpc": "2.0", "id": ident, "result": {
                "content": [{"type": "text", "text": json.dumps(payload)}],
                "isError": False}})
        elif name == "get_diagnostics":
            payload = {"file": "/tmp/x.rs", "diagnostics": [], "count": 0}
            send({"jsonrpc": "2.0", "id": ident, "result": {
                "content": [{"type": "text", "text": json.dumps(payload)}],
                "isError": False}})
        else:
            send({"jsonrpc": "2.0", "id": ident, "result": {
                "content": [{"type": "text", "text": "unknown tool"}],
                "isError": True}})
"#;

fn write_fake_server() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("periscope-tui-smoke-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join("fake_server.py");
    std::fs::write(&path, FAKE_SERVER).expect("write fake server");
    let mut permissions = std::fs::metadata(&path).expect("stat").permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&path, permissions).expect("chmod fake server");
    path
}

#[test]
fn line_mode_against_fake_server_reports_attached_and_runs_plan() {
    let server = write_fake_server();
    let mut child = Command::new(env!("CARGO_BIN_EXE_periscope-tui"))
        .arg("--server")
        .arg(&server)
        .arg("--line")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn tui");
    let mut stdin = child.stdin.take().expect("stdin");
    stdin
        .write_all(b"status\ntarget /tmp/x.rs\nrun diagnostics\napprove\nquit\n")
        .expect("write commands");
    drop(stdin);
    let output = child.wait_with_output().expect("wait for tui");
    assert!(output.status.success(), "status: {:?}", output.status);
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("ATTACHED"), "stdout: {text}");
    assert!(text.contains("Neovim 0.11.4"), "stdout: {text}");
    assert!(text.contains("staged diagnostics"), "stdout: {text}");
    assert!(text.contains("DONE"), "stdout: {text}");
}

#[test]
fn missing_server_binary_fails_cleanly() {
    let output = Command::new(env!("CARGO_BIN_EXE_periscope-tui"))
        .args(["--server", "/nonexistent/periscope", "--line"])
        .stdin(Stdio::null())
        .output()
        .expect("run tui");
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cannot start the periscope server"),
        "stderr: {stderr}"
    );
}
