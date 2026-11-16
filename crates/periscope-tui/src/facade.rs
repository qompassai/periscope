//! The facade the UI programs against, and its two implementations.
//!
//! [`Facade`] is deliberately narrow: broker status, the server's own
//! tool list, and one generic tool call — all in `serde_json::Value`,
//! mirroring the phlow-tui `RuntimeFacade` pattern. [`FakeFacade`] is
//! the scripted in-memory double used by every test; it records each
//! call so plan mode can *prove* no call happened before approval.
//! [`McpStdioFacade`] is the real wiring: an MCP stdio client that
//! spawns the member's own `periscope` server binary and speaks
//! NDJSON JSON-RPC to it, the same transport the server documents.
//!
//! Failure classes are typed. `Detached` means the server answered
//! that it cannot reach an editor; `Stale` means the editor/broker
//! connection died mid-session; both invalidate cached state in the
//! app. `Tool` is an in-band tool error (bad arguments and the like)
//! that says nothing about editor health.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::Duration;

use serde_json::{Value, json};

use crate::{RESPONSE_DEADLINE_MS, RESPONSE_LINE_MAX_BYTES, TOOLS_MAX, truncate_chars};

/// Every failure the facade can report to the app.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FacadeError {
    /// The server cannot reach any editor (its broker connect failed).
    Detached(String),
    /// The editor connection died after having worked: state unknown.
    Stale(String),
    /// The server process or its stdio transport failed.
    Transport(String),
    /// The peer sent something outside the MCP shapes this client
    /// accepts (malformed JSON, missing fields, oversized line).
    Protocol(String),
    /// The tool itself reported an in-band error (`isError: true`).
    Tool(String),
    /// No response arrived inside RESPONSE_DEADLINE_MS.
    Timeout,
}

impl fmt::Display for FacadeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FacadeError::Detached(detail) => write!(f, "editor detached: {detail}"),
            FacadeError::Stale(detail) => write!(f, "editor state stale: {detail}"),
            FacadeError::Transport(detail) => write!(f, "transport failure: {detail}"),
            FacadeError::Protocol(detail) => write!(f, "protocol failure: {detail}"),
            FacadeError::Tool(detail) => write!(f, "tool error: {detail}"),
            FacadeError::Timeout => {
                write!(f, "no response within {RESPONSE_DEADLINE_MS} ms")
            }
        }
    }
}

impl std::error::Error for FacadeError {}

/// The narrow surface the UI programs against.
pub trait Facade {
    /// Broker status (`broker_status`): mode, editor version, config
    /// root, working directory. `Detached` when no editor is reached.
    fn status(&mut self) -> Result<Value, FacadeError>;
    /// The server's own MCP tool names, bounded to TOOLS_MAX.
    fn tools(&mut self) -> Result<Vec<String>, FacadeError>;
    /// Call one tool with JSON arguments; returns its JSON result.
    fn call(&mut self, tool: &str, args: Value) -> Result<Value, FacadeError>;
}

/// Script for one fake call sequence: results are consumed in order.
type CallScript = VecDeque<Result<Value, FacadeError>>;

/// In-memory scripted facade for tests. Records every call.
pub struct FakeFacade {
    status_script: CallScript,
    tools_result: Result<Vec<String>, FacadeError>,
    call_scripts: HashMap<String, CallScript>,
    call_defaults: HashMap<String, Result<Value, FacadeError>>,
    recorded: RefCell<Vec<(String, Value)>>,
}

impl FakeFacade {
    /// A fake whose editor is attached and healthy, answering with
    /// payloads shaped exactly like the real Lua-side results.
    pub fn attached() -> FakeFacade {
        let mut fake = FakeFacade::bare();
        fake.status_script.push_back(Ok(sample_status()));
        fake.call_defaults
            .insert("broker_status".to_string(), Ok(sample_status()));
        fake.call_defaults
            .insert("list_servers".to_string(), Ok(sample_servers()));
        fake.call_defaults
            .insert("list_formatters".to_string(), Ok(sample_formatters()));
        fake.call_defaults
            .insert("list_linters".to_string(), Ok(sample_linters()));
        fake.call_defaults
            .insert("list_dap".to_string(), Ok(sample_dap()));
        fake.call_defaults.insert(
            "get_diagnostics".to_string(),
            Ok(json!({"file": "/tmp/x.rs", "diagnostics": [], "count": 0})),
        );
        fake.call_defaults.insert(
            "run_format".to_string(),
            Ok(json!({"changed": false, "status": "ok", "written": false})),
        );
        fake.call_defaults.insert(
            "run_lint".to_string(),
            Ok(json!({"results": [], "status": "ok"})),
        );
        fake.call_defaults.insert(
            "get_document_symbols".to_string(),
            Ok(json!({"file": "/tmp/x.rs", "symbols": []})),
        );
        fake
    }

    /// A fake whose server can never reach an editor: every status
    /// and editor call fails with `Detached`, like the real server's
    /// "connecting to neovim failed" in-band error.
    pub fn detached() -> FakeFacade {
        let mut fake = FakeFacade::bare();
        let error = || FacadeError::Detached("connecting to neovim failed".to_string());
        fake.status_script.push_back(Err(error()));
        for tool in [
            "broker_status",
            "list_servers",
            "list_formatters",
            "list_linters",
            "list_dap",
            "get_diagnostics",
            "run_format",
            "run_lint",
            "get_document_symbols",
        ] {
            fake.call_defaults.insert(tool.to_string(), Err(error()));
        }
        fake
    }

    fn bare() -> FakeFacade {
        FakeFacade {
            status_script: VecDeque::new(),
            tools_result: Ok(sample_tool_names()),
            call_scripts: HashMap::new(),
            call_defaults: HashMap::new(),
            recorded: RefCell::new(Vec::new()),
        }
    }

    /// Queue the next status result (consumed before the default).
    pub fn push_status(&mut self, result: Result<Value, FacadeError>) {
        self.status_script.push_back(result);
    }

    /// Queue the next result for one tool (consumed before defaults).
    pub fn push_call(&mut self, tool: &str, result: Result<Value, FacadeError>) {
        self.call_scripts
            .entry(tool.to_string())
            .or_default()
            .push_back(result);
    }

    /// Set the standing result for one tool.
    pub fn set_call_default(&mut self, tool: &str, result: Result<Value, FacadeError>) {
        self.call_defaults.insert(tool.to_string(), result);
    }

    /// Every recorded (tool, args) call, in order.
    pub fn recorded_calls(&self) -> Vec<(String, Value)> {
        self.recorded.borrow().clone()
    }

    /// How many calls have been recorded in total.
    pub fn call_count(&self) -> usize {
        self.recorded.borrow().len()
    }
}

impl Facade for FakeFacade {
    fn status(&mut self) -> Result<Value, FacadeError> {
        self.recorded
            .borrow_mut()
            .push(("broker_status".to_string(), json!({})));
        if let Some(result) = self.status_script.pop_front() {
            return result;
        }
        if let Some(script) = self.call_scripts.get_mut("broker_status")
            && let Some(result) = script.pop_front()
        {
            return result;
        }
        self.call_defaults
            .get("broker_status")
            .cloned()
            .unwrap_or_else(|| {
                Err(FacadeError::Tool(
                    "fake has no script for broker_status".to_string(),
                ))
            })
    }

    fn tools(&mut self) -> Result<Vec<String>, FacadeError> {
        self.tools_result.clone()
    }

    fn call(&mut self, tool: &str, args: Value) -> Result<Value, FacadeError> {
        self.recorded.borrow_mut().push((tool.to_string(), args));
        if let Some(script) = self.call_scripts.get_mut(tool)
            && let Some(result) = script.pop_front()
        {
            return result;
        }
        self.call_defaults
            .get(tool)
            .cloned()
            .unwrap_or_else(|| Err(FacadeError::Tool(format!("fake has no script for {tool}"))))
    }
}

/// The real facade: an MCP stdio client owning one `periscope`
/// server child process. Dropping it kills and reaps the child.
pub struct McpStdioFacade {
    child: Child,
    stdin: ChildStdin,
    responses: Receiver<Result<String, FacadeError>>,
    next_id: u64,
}

impl McpStdioFacade {
    /// Spawn `server_bin` with `server_args`, run the MCP initialize
    /// handshake, and return the ready facade. Spawning the server
    /// does not touch any editor: the server connects its broker
    /// lazily, on the first tool call.
    pub fn spawn(server_bin: PathBuf, server_args: &[String]) -> Result<Self, FacadeError> {
        let mut child = Command::new(&server_bin)
            .args(server_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| {
                FacadeError::Transport(format!("spawning {}: {error}", server_bin.display()))
            })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| FacadeError::Transport("server child has no stdin".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| FacadeError::Transport("server child has no stdout".to_string()))?;
        let (sender, responses) = channel::<Result<String, FacadeError>>();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => {
                        let _ = sender
                            .send(Err(FacadeError::Stale("server stdout closed".to_string())));
                        return;
                    }
                    Ok(_) => {
                        if line.len() > RESPONSE_LINE_MAX_BYTES {
                            let _ = sender.send(Err(FacadeError::Protocol(format!(
                                "response line exceeds {RESPONSE_LINE_MAX_BYTES} bytes"
                            ))));
                            return;
                        }
                        if sender.send(Ok(line)).is_err() {
                            return;
                        }
                    }
                    Err(error) => {
                        let _ = sender.send(Err(FacadeError::Transport(format!(
                            "reading server stdout: {error}"
                        ))));
                        return;
                    }
                }
            }
        });
        let mut facade = McpStdioFacade {
            child,
            stdin,
            responses,
            next_id: 0,
        };
        facade.request(
            "initialize",
            json!({
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": "periscope-tui", "version": env!("CARGO_PKG_VERSION")},
            }),
        )?;
        facade.notify("notifications/initialized")?;
        Ok(facade)
    }

    fn notify(&mut self, method: &str) -> Result<(), FacadeError> {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": {}}))
    }

    fn send(&mut self, message: &Value) -> Result<(), FacadeError> {
        let mut text = serde_json::to_string(message)
            .map_err(|error| FacadeError::Protocol(format!("encoding request: {error}")))?;
        text.push('\n');
        self.stdin
            .write_all(text.as_bytes())
            .and_then(|()| self.stdin.flush())
            .map_err(|error| FacadeError::Transport(format!("writing to server: {error}")))
    }

    /// One request/response round trip. Notifications and responses
    /// to other ids are skipped; the wait is deadline-bounded.
    fn request(&mut self, method: &str, params: Value) -> Result<Value, FacadeError> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params,
        }))?;
        let deadline = Duration::from_millis(RESPONSE_DEADLINE_MS);
        loop {
            let line = match self.responses.recv_timeout(deadline) {
                Ok(Ok(line)) => line,
                Ok(Err(error)) => return Err(error),
                Err(RecvTimeoutError::Timeout) => return Err(FacadeError::Timeout),
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(FacadeError::Stale(
                        "server response channel closed".to_string(),
                    ));
                }
            };
            let message: Value = serde_json::from_str(&line).map_err(|error| {
                FacadeError::Protocol(format!("malformed response line: {error}"))
            })?;
            if message.get("id").and_then(Value::as_u64) != Some(id) {
                continue; // A notification or another response: not ours.
            }
            if let Some(error) = message.get("error") {
                let text = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown protocol error");
                return Err(FacadeError::Protocol(truncate_chars(text, 256)));
            }
            return message
                .get("result")
                .cloned()
                .ok_or_else(|| FacadeError::Protocol("response carries no result".to_string()));
        }
    }
}

impl Facade for McpStdioFacade {
    fn status(&mut self) -> Result<Value, FacadeError> {
        self.call("broker_status", json!({}))
    }

    fn tools(&mut self) -> Result<Vec<String>, FacadeError> {
        let result = self.request("tools/list", json!({}))?;
        let entries = result
            .get("tools")
            .and_then(Value::as_array)
            .ok_or_else(|| FacadeError::Protocol("tools/list has no tools array".into()))?;
        let mut names = Vec::new();
        for entry in entries {
            if names.len() >= TOOLS_MAX {
                break;
            }
            if let Some(name) = entry.get("name").and_then(Value::as_str) {
                names.push(truncate_chars(name, 64));
            }
        }
        Ok(names)
    }

    fn call(&mut self, tool: &str, args: Value) -> Result<Value, FacadeError> {
        let result = self.request("tools/call", json!({"name": tool, "arguments": args}))?;
        tool_result_value(&result)
    }
}

impl Drop for McpStdioFacade {
    fn drop(&mut self) {
        // The facade owns the server child: kill and reap it so no
        // broker process outlives the console.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Interpret one MCP tools/call result: in-band errors map to typed
/// failures (editor-connect failures are `Detached`, a closed broker
/// is `Stale`), success text parses back into JSON.
pub fn tool_result_value(result: &Value) -> Result<Value, FacadeError> {
    let is_error = result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let text = result
        .get("content")
        .and_then(Value::as_array)
        .and_then(|content| content.first())
        .and_then(|first| first.get("text"))
        .and_then(Value::as_str)
        .ok_or_else(|| FacadeError::Protocol("tool result has no text content".to_string()))?;
    if is_error {
        let detail = truncate_chars(text, 256);
        if detail.contains("connecting to neovim failed") {
            return Err(FacadeError::Detached(detail));
        }
        if detail.contains("connection closed") {
            return Err(FacadeError::Stale(detail));
        }
        return Err(FacadeError::Tool(detail));
    }
    serde_json::from_str(text)
        .map_err(|error| FacadeError::Protocol(format!("tool result is not JSON: {error}")))
}

/// Sample `broker_status` payload, shaped like the real Lua result
/// plus the server's own `mode`/`workspace` insertions.
pub fn sample_status() -> Value {
    json!({
        "config_root": "/home/user/.config/nvim",
        "cwd": "/home/user/project",
        "mode": "attached",
        "nvim_version": "0.11.4",
        "vim_starting": 0,
        "workspace": "/home/user/project",
    })
}

/// Sample `list_servers` payload, shaped like the real Lua result.
pub fn sample_servers() -> Value {
    json!({
        "attached": [
            {"id": 1, "name": "rust_analyzer", "root_dir": "/home/user/project"},
            {"id": 2, "name": "lua_ls", "root_dir": "/home/user/project"},
        ],
        "configured": ["lua_ls", "rust_analyzer", "taplo"],
        "configured_count": 3,
    })
}

/// Sample `list_formatters` payload, shaped like the real result.
pub fn sample_formatters() -> Value {
    json!({"count": 2, "formatters": ["rustfmt", "stylua"], "source": "runtime files"})
}

/// Sample `list_linters` payload, shaped like the real result.
pub fn sample_linters() -> Value {
    json!({
        "count": 2,
        "internal": ["_util"],
        "linters": ["clippy", "luacheck"],
        "source": "runtime files",
    })
}

/// Sample `list_dap` payload, shaped like the real result.
pub fn sample_dap() -> Value {
    json!({
        "adapters_registered": ["codelldb"],
        "configurations": [
            {"configurations": ["Launch"], "filetype": "rust"},
        ],
        "dap_loaded": true,
        "modules": ["rust", "lua"],
    })
}

/// The tool names the real server reports, for fake tool lists.
pub fn sample_tool_names() -> Vec<String> {
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
    .iter()
    .map(|name| name.to_string())
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_result_parses_success_text() {
        let result = json!({
            "content": [{"type": "text", "text": "{\"count\": 3}"}],
            "isError": false,
        });
        assert_eq!(tool_result_value(&result).ok(), Some(json!({"count": 3})));
    }

    #[test]
    fn tool_result_maps_connect_failure_to_detached() {
        let result = json!({
            "content": [{"type": "text", "text": "connecting to neovim failed: x"}],
            "isError": true,
        });
        assert!(matches!(
            tool_result_value(&result),
            Err(FacadeError::Detached(_))
        ));
    }

    #[test]
    fn tool_result_maps_closed_broker_to_stale() {
        let result = json!({
            "content": [{"type": "text", "text": "neovim broker connection closed"}],
            "isError": true,
        });
        assert!(matches!(
            tool_result_value(&result),
            Err(FacadeError::Stale(_))
        ));
    }

    #[test]
    fn tool_result_rejects_non_json_success_text() {
        let result = json!({
            "content": [{"type": "text", "text": "not json at all"}],
            "isError": false,
        });
        assert!(matches!(
            tool_result_value(&result),
            Err(FacadeError::Protocol(_))
        ));
    }

    #[test]
    fn tool_result_rejects_missing_content() {
        let result = json!({"isError": false});
        assert!(matches!(
            tool_result_value(&result),
            Err(FacadeError::Protocol(_))
        ));
    }

    #[test]
    fn fake_records_calls_in_order() {
        let mut fake = FakeFacade::attached();
        let _ = fake.call("list_servers", json!({}));
        let _ = fake.call("list_dap", json!({}));
        let names: Vec<String> = fake
            .recorded_calls()
            .iter()
            .map(|(tool, _)| tool.clone())
            .collect();
        assert_eq!(names, vec!["list_servers", "list_dap"]);
    }
}
