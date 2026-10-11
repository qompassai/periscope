//! The MCP front end: newline-delimited JSON-RPC over stdio.
//!
//! Protocol contract (MCP, stdio transport): one JSON object per
//! line, responses in request order, notifications never answered.
//! The line size is capped; an oversized line is drained and answered
//! with a parse-class error instead of being buffered without bound.
//! The broker is connected lazily on the first tool call (or status
//! request) and retried per call while it stays unavailable, so an
//! editor started after this server is still picked up.

use serde_json::{Value as JsonValue, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

use crate::broker::{BrokerConfig, NvimBroker};
use crate::tools;

/// Largest accepted inbound line, in bytes (4 MiB).
const LINE_MAX_BYTES: usize = 4 * 1024 * 1024;
/// Newest protocol revision this server speaks natively.
const PROTOCOL_VERSION: &str = "2025-03-26";
/// Revisions accepted verbatim from the client's initialize request.
const PROTOCOL_VERSIONS_ACCEPTED: [&str; 3] = ["2024-11-05", "2025-03-26", "2025-06-18"];

struct State {
    broker: Option<NvimBroker>,
    broker_config: BrokerConfig,
    initialized: bool,
}

/// Run the server until stdin reaches EOF, then shut the broker down.
pub async fn run<R, W>(input: R, output: W, broker_config: BrokerConfig)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut reader = BufReader::new(input);
    let mut output = output;
    let mut state = State {
        broker: None,
        broker_config,
        initialized: false,
    };
    loop {
        let Some(line) = read_line_bounded(&mut reader).await else {
            break;
        };
        let Some(text) = line else {
            // Oversized line: drained already; answer with a parse error.
            write_message(
                &mut output,
                &json!({"jsonrpc": "2.0", "id": null,
                        "error": {"code": -32700, "message": "message exceeds size limit"}}),
            )
            .await;
            continue;
        };
        if text.trim().is_empty() {
            continue;
        }
        let message: JsonValue = match serde_json::from_str(&text) {
            Ok(message) => message,
            Err(_) => {
                write_message(
                    &mut output,
                    &json!({"jsonrpc": "2.0", "id": null,
                            "error": {"code": -32700, "message": "parse error"}}),
                )
                .await;
                continue;
            }
        };
        if let Some(response) = handle_message(&mut state, &message).await {
            write_message(&mut output, &response).await;
        }
    }
    if let Some(broker) = state.broker.take() {
        broker.shutdown().await;
    }
}

/// Read one line, bounded. Returns `None` at EOF, `Some(None)` for a
/// line that exceeded the cap (already drained to its newline).
async fn read_line_bounded<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
) -> Option<Option<String>> {
    let mut buffer: Vec<u8> = Vec::new();
    let mut oversized = false;
    loop {
        let chunk = match reader.fill_buf().await {
            Ok(chunk) => chunk,
            Err(_) => return None,
        };
        if chunk.is_empty() {
            if buffer.is_empty() && !oversized {
                return None;
            }
            break;
        }
        let newline_at = chunk.iter().position(|byte| *byte == b'\n');
        let take = newline_at.map_or(chunk.len(), |index| index + 1);
        if !oversized {
            if buffer.len() + take > LINE_MAX_BYTES {
                oversized = true;
                buffer.clear();
            } else {
                buffer.extend_from_slice(&chunk[..take]);
            }
        }
        reader.consume(take);
        if newline_at.is_some() {
            break;
        }
    }
    if oversized {
        return Some(None);
    }
    Some(Some(String::from_utf8_lossy(&buffer).into_owned()))
}

async fn write_message<W: AsyncWrite + Unpin>(output: &mut W, message: &JsonValue) {
    let mut text = serde_json::to_string(message).unwrap_or_else(|_| "{}".to_string());
    text.push('\n');
    let _ = output.write_all(text.as_bytes()).await;
    let _ = output.flush().await;
}

async fn handle_message(state: &mut State, message: &JsonValue) -> Option<JsonValue> {
    let method = message.get("method").and_then(JsonValue::as_str);
    let id = message.get("id").cloned();
    let Some(method) = method else {
        // A response or malformed object: nothing to answer.
        return None;
    };
    // Notifications (no id) are never answered: initialized and
    // cancelled are the only ones with meaning here, and both are
    // no-ops for this server.
    let id = id?;
    let params = message.get("params").cloned().unwrap_or(JsonValue::Null);
    let (result, error) = dispatch(state, method, &params).await;
    Some(match error {
        Some((code, text)) => json!({
            "jsonrpc": "2.0", "id": id,
            "error": {"code": code, "message": text},
        }),
        None => json!({"jsonrpc": "2.0", "id": id, "result": result}),
    })
}

type RpcError = (i64, String);

async fn dispatch(
    state: &mut State,
    method: &str,
    params: &JsonValue,
) -> (JsonValue, Option<RpcError>) {
    match method {
        "initialize" => {
            state.initialized = true;
            let requested = params
                .get("protocolVersion")
                .and_then(JsonValue::as_str)
                .unwrap_or("");
            let version = if PROTOCOL_VERSIONS_ACCEPTED.contains(&requested) {
                requested
            } else {
                PROTOCOL_VERSION
            };
            (
                json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {
                        "name": "periscope",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                }),
                None,
            )
        }
        "ping" => (json!({}), None),
        "tools/list" | "tools/call" if !state.initialized => (
            JsonValue::Null,
            Some((-32002, "server not initialized".to_string())),
        ),
        "tools/list" => (json!({"tools": tools::definitions()}), None),
        "tools/call" => (call_tool(state, params).await, None),
        _ => (
            JsonValue::Null,
            Some((-32601, format!("method not found: {method}"))),
        ),
    }
}

/// A tools/call is answered in-band: tool failures are results with
/// `isError: true`, never protocol errors.
async fn call_tool(state: &mut State, params: &JsonValue) -> JsonValue {
    let Some(name) = params.get("name").and_then(JsonValue::as_str) else {
        return tool_error("tools/call requires a string `name`");
    };
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    if state.broker.is_none() {
        match NvimBroker::connect(state.broker_config.clone()).await {
            Ok(broker) => state.broker = Some(broker),
            Err(error) => {
                return tool_error(&format!("connecting to neovim failed: {error}"));
            }
        }
    }
    let broker = state.broker.as_ref().expect("broker checked above");
    match tools::call(broker, broker.mode(), name, &arguments).await {
        Ok(result) => json!({
            "content": [{"type": "text", "text": result.to_string()}],
            "isError": false,
        }),
        Err(error) => tool_error(&error.to_string()),
    }
}

fn tool_error(message: &str) -> JsonValue {
    json!({
        "content": [{"type": "text", "text": message}],
        "isError": true,
    })
}
