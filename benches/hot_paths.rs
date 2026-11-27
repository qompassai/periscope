//! hot_paths: cold start to discover, tools/list latency, and attention_publish latency for the periscope server (hand-rolled, std::time only).
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

const CLIENT: &str = "periscope-bench";
const TOOL: &str = "attention_publish";
const COLD_START_RUNS: usize = 15;
const WARMUP_CALLS: i64 = 20;
const LIST_CALLS: i64 = 200;
const TOOL_CALLS: i64 = 200;

fn report(name: &str, elapsed: std::time::Duration, n: i64) {
    let ns = elapsed.as_nanos() as f64 / n as f64;
    println!(
        "{name}: {ns:.0} ns/op, {:.1} ops/s (n={n})",
        1_000_000_000.0 / ns
    );
}

fn run() -> Result<(), String> {
    let bin = find_server_bin("periscope")?;
    let (fixture, args, envs) = setup_fixture()?;
    // Cold start: spawn + discover, per run.
    let t0 = std::time::Instant::now();
    for _ in 0..COLD_START_RUNS {
        let mut s = Session::spawn(&bin, &args, &envs)?;
        let r = s.request(&discover(0, CLIENT))?;
        if r["result"]["supportedVersions"].is_null() {
            return Err("discover: missing supportedVersions".into());
        }
        s.close()?;
    }
    report("cold_start_discover", t0.elapsed(), COLD_START_RUNS as i64);
    // Warm session for list + tool workloads.
    let mut s = Session::spawn(&bin, &args, &envs)?;
    s.request(&discover(0, CLIENT))?;
    for i in 0..WARMUP_CALLS {
        s.request(&tools_list(100 + i, CLIENT))?;
        s.request(&tool_call(
            200 + i,
            CLIENT,
            TOOL,
            tool_args_for(CLIENT, i + 1),
        ))?;
    }
    let t1 = std::time::Instant::now();
    for i in 0..LIST_CALLS {
        let r = s.request(&tools_list(1000 + i, CLIENT))?;
        if r["result"]["tools"].is_null() {
            return Err("tools/list: missing tools".into());
        }
    }
    report("tools_list", t1.elapsed(), LIST_CALLS);
    let t2 = std::time::Instant::now();
    for i in 0..TOOL_CALLS {
        let r = s.request(&tool_call(
            2000 + i,
            CLIENT,
            TOOL,
            tool_args_for(CLIENT, 10_000 + i),
        ))?;
        if r["result"]["content"].is_null() {
            return Err("tools/call: missing content".into());
        }
    }
    report(TOOL, t2.elapsed(), TOOL_CALLS);
    s.close()?;
    let _ = std::fs::remove_dir_all(&fixture);
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("hot_paths: error: {e}");
        std::process::exit(1);
    }
}
