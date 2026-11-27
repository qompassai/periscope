//! stdio_session: modern-era MCP session against the periscope server - server/discover, tools/list, and one attention_publish call; prints a readable transcript summary (server name/version, tool names, and the call result text, truncated).
#![allow(dead_code)]

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

pub const PROTOCOL_VERSION: &str = "2026-07-28";

pub fn meta(client_name: &str) -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion": PROTOCOL_VERSION,
        "io.modelcontextprotocol/clientInfo": {"name": client_name, "version": "0"},
        "io.modelcontextprotocol/clientCapabilities": {}
    })
}

pub fn find_server_bin(name: &str) -> Result<PathBuf, String> {
    if let Ok(p) = std::env::var("BENCH_SERVER_BIN") {
        return Ok(PathBuf::from(p));
    }
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let mut dir = exe.parent().map(|p| p.to_path_buf());
    for _ in 0..6 {
        let Some(d) = dir else { break };
        let cand = d.join(name);
        if cand.is_file() {
            return Ok(cand);
        }
        dir = d.parent().map(|p| p.to_path_buf());
    }
    Err(format!(
        "server binary {name} not found near {}",
        exe.display()
    ))
}

pub struct Session {
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<ChildStdout>,
}

impl Session {
    pub fn spawn(
        bin: &PathBuf,
        args: &[String],
        envs: &[(String, String)],
    ) -> Result<Self, String> {
        let mut cmd = Command::new(bin);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for (k, v) in envs {
            cmd.env(k, v);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("spawn {}: {e}", bin.display()))?;
        let stdin = child.stdin.take().ok_or("child stdin unavailable")?;
        let stdout = child.stdout.take().ok_or("child stdout unavailable")?;
        Ok(Session {
            child,
            stdin,
            reader: BufReader::new(stdout),
        })
    }
    pub fn request(&mut self, frame: &Value) -> Result<Value, String> {
        let line = frame.to_string();
        self.stdin
            .write_all(line.as_bytes())
            .map_err(|e| format!("write: {e}"))?;
        self.stdin
            .write_all(b"\n")
            .map_err(|e| format!("write nl: {e}"))?;
        self.stdin.flush().map_err(|e| format!("flush: {e}"))?;
        let mut out = String::new();
        let n = self
            .reader
            .read_line(&mut out)
            .map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            return Err("server closed stdout".to_string());
        }
        serde_json::from_str(out.trim()).map_err(|e| format!("parse response: {e}"))
    }
    pub fn close(mut self) -> Result<(), String> {
        drop(self.stdin);
        let status = self.child.wait().map_err(|e| format!("wait: {e}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("server exit status {status}"))
        }
    }
}

pub fn discover(id: i64, client: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": "server/discover", "params": {"_meta": meta(client)}})
}
pub fn tools_list(id: i64, client: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": "tools/list", "params": {"_meta": meta(client)}})
}
pub fn tool_call(id: i64, client: &str, tool: &str, args: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
           "params": {"name": tool, "arguments": args, "_meta": meta(client)}})
}
pub fn result_text(resp: &Value) -> String {
    resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or("")
        .to_string()
}

type Fixture = (PathBuf, Vec<String>, Vec<(String, String)>);

fn setup_fixture() -> Result<Fixture, String> {
    let root = std::env::temp_dir().join(format!("periscope-bench-{}", std::process::id()));
    std::fs::create_dir_all(&root).map_err(|e| format!("mkdir: {e}"))?;
    // Attach mode with a nonexistent socket: the attention board is
    // in-process and never touches the editor, so no Neovim is needed.
    let args = vec![
        "--attach".to_string(),
        "/nonexistent/periscope-bench.sock".to_string(),
        "--workspace".to_string(),
        root.to_string_lossy().into_owned(),
    ];
    Ok((root, args, Vec::new()))
}
fn tool_args_for(client: &str, seq: i64) -> Value {
    json!({"focus": {"agent_id": client, "snapshot_hash": "bench-frame", "seq": seq, "targets": ["src/main.rs"]}})
}

const CLIENT: &str = "periscope-example";
const TOOL: &str = "attention_publish";
const MAX_PRINT_CHARS: usize = 600;

fn run() -> Result<(), String> {
    let bin = find_server_bin("periscope")?;
    let (fixture, args, envs) = setup_fixture()?;
    let mut session = Session::spawn(&bin, &args, &envs)?;
    let discover = session.request(&discover(0, CLIENT))?;
    println!("discover: {}", discover["result"]["supportedVersions"]);
    println!(
        "server: {}",
        discover["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]
    );
    let list = session.request(&tools_list(1, CLIENT))?;
    let tools = list["result"]["tools"]
        .as_array()
        .ok_or("tools/list: no tools array")?;
    println!("tools/list: {} tools", tools.len());
    for t in tools {
        println!("  - {}", t["name"].as_str().unwrap_or("?"));
    }
    let call = session.request(&tool_call(2, CLIENT, TOOL, tool_args_for(CLIENT, 1)))?;
    let text = result_text(&call);
    let shown: String = text.chars().take(MAX_PRINT_CHARS).collect();
    println!(
        "tools/call {TOOL}: isError={} text={}",
        call["result"]["isError"], shown
    );
    session.close()?;
    let _ = std::fs::remove_dir_all(&fixture);
    println!("stdio_session: ok");
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("stdio_session: error: {e}");
        std::process::exit(1);
    }
}
