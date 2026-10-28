//! Era guard: the byte-level front door of the server.
//!
//! rmcp's service layer enforces the modern era at the message level
//! (discover negotiation, per-request `_meta` validation) once the
//! handler pins `supported_protocol_versions` to 2026-07-28 — but it
//! silently drops lines it cannot parse, and it will still *answer*
//! an `initialize` request (with the unsupported-version refusal)
//! only by failing the whole serve. This guard sits in front of the
//! SDK on stdio and owns the framing contract, reproducing the
//! hand-rolled baseline's exact shapes wherever the baseline had one:
//!
//! - malformed JSON → `-32700 "parse error"` (id null); oversized
//!   line → `-32700 "message exceeds size limit"` with the remainder
//!   drained (the baseline's 4 MiB cap and message); the server stays
//!   alive throughout
//! - `initialize` (any version) → the designed modern-only refusal,
//!   `-32022` naming the supported versions; the connection and the
//!   process stay up, so a probe can continue with modern frames
//! - unknown methods → `-32601 "method not found: {method}"` (the
//!   baseline's exact message); only `server/discover`, `ping`,
//!   `tools/list`, and `tools/call` frames are forwarded. A frame
//!   with no string `method` (a response, a malformed object) is
//!   dropped silently, as the baseline dropped it; a non-object
//!   request is answered `-32600` (the fleet framing contract — the
//!   baseline dropped those too; itemized in the WS2 gate report)
//! - notifications (no `id`) are dropped silently, as before
//!
//! Forwarded frames pass through except for shape normalization
//! that reproduces the baseline's handler-level degradation inside
//! the SDK's typed envelope: a missing/non-object `params` becomes
//! `{}`, and a non-object `arguments` becomes `{}` (the baseline
//! read no fields from a non-object arguments value — this is also
//! exactly what a client encoding empty arguments as `[]` meets).
//! One baseline shape is answered by the guard itself: on an
//! otherwise well-formed modern `tools/call` (stamped `_meta`
//! protocol version 2026-07-28), a missing or non-string tool `name`
//! earns the baseline's in-band result, `isError` text
//! "tools/call requires a string `name`" — the SDK's typed request
//! cannot represent a nameless call, so the handler could never
//! reproduce it. Frames that are not modern-stamped are forwarded
//! (name normalized to `""`) and meet the SDK's metadata/version
//! refusal before any handler runs, per the fleet's params-less
//! frame rule.

use std::sync::Arc;

use serde_json::{Value, json};
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, Stdin, Stdout,
    WriteHalf,
};
use tokio::sync::Mutex;

/// The one protocol version this server speaks.
pub const MODERN_VERSION: &str = "2026-07-28";
/// Maximum bytes in one JSON-RPC line (the baseline's cap).
const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;

const PARSE_ERROR: i32 = -32700;
const INVALID_REQUEST: i32 = -32600;
const METHOD_NOT_FOUND: i32 = -32601;
const UNSUPPORTED_PROTOCOL_VERSION: i32 = -32022;

/// Stdout shared by the guard (its own error frames) and the pump
/// (SDK responses); one line is written per lock, never interleaved.
pub type SharedStdout = Arc<Mutex<Stdout>>;

/// Response-ordering state shared by the guard and the pump: the
/// ids of forwarded requests whose SDK response has not yet been
/// written to stdout. The baseline answered strictly in request
/// order; the guard therefore never writes one of its own frames
/// while an earlier forwarded request is unanswered (without this,
/// a guard frame — answered instantly — overtakes the SDK's first
/// response, which pays the service bootstrap latency).
pub struct Ordering {
    outstanding: Mutex<Vec<Value>>,
    drained: tokio::sync::Notify,
}

impl Ordering {
    pub fn new() -> Arc<Self> {
        Arc::new(Ordering {
            outstanding: Mutex::new(Vec::new()),
            drained: tokio::sync::Notify::new(),
        })
    }

    async fn track(&self, id: Value) {
        self.outstanding.lock().await.push(id);
    }

    async fn complete(&self, id: &Value) {
        {
            let mut queue = self.outstanding.lock().await;
            if let Some(pos) = queue.iter().position(|q| q == id) {
                queue.remove(pos);
            }
        }
        self.drained.notify_waiters();
    }

    /// Wait until no forwarded request is unanswered. Bounded: if
    /// the SDK died mid-request its response will never come, and
    /// the guard must go on answering framing errors regardless.
    async fn wait_drained(&self) {
        let deadline = std::time::Duration::from_secs(5);
        let _ = tokio::time::timeout(deadline, async {
            loop {
                let notified = self.drained.notified();
                if self.outstanding.lock().await.is_empty() {
                    return;
                }
                notified.await;
            }
        })
        .await;
    }
}

fn error_frame(id: Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// The designed refusal of legacy-era contact (CHECKLIST 1.8 shape:
/// the SDK's own `unsupported_protocol_version` payload).
fn unsupported_version_frame(id: Value, requested: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": UNSUPPORTED_PROTOCOL_VERSION,
            "message": "Unsupported protocol version",
            "data": {"requested": requested, "supported": [MODERN_VERSION]}
        }
    })
}

/// The baseline's in-band answer to a nameless `tools/call`: a
/// successful response whose payload is a tool error.
fn missing_name_frame(id: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "content": [{"type": "text", "text": "tools/call requires a string `name`"}],
            "isError": true
        }
    })
}

/// What the guard does with one inbound line.
#[derive(Debug, PartialEq)]
enum Action {
    /// Forward this (possibly shape-normalized) frame to the SDK.
    Forward(Value),
    /// Answer it here; the SDK never sees it.
    Respond(Value),
    /// Drop silently (blank line, notification, method-less frame).
    Drop,
}

/// Classify one line. Pure: unit-tested without any transport.
fn classify(line: &[u8]) -> Action {
    let text = String::from_utf8_lossy(line);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Action::Drop;
    }
    let value: Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(_) => {
            return Action::Respond(error_frame(Value::Null, PARSE_ERROR, "parse error"));
        }
    };
    let Some(object) = value.as_object() else {
        return Action::Respond(error_frame(
            Value::Null,
            INVALID_REQUEST,
            "request must be a JSON object",
        ));
    };
    // No `id` key: a notification. Dropped silently, as the
    // baseline dropped it (its front end never answered — and never
    // executed — a frame without an id).
    if !object.contains_key("id") {
        return Action::Drop;
    }
    let id = object.get("id").cloned().unwrap_or(Value::Null);
    // No string method: a response or a malformed object — the
    // baseline answered nothing for these.
    let Some(method) = object.get("method").and_then(Value::as_str) else {
        return Action::Drop;
    };
    match method {
        "initialize" => {
            let requested = object
                .get("params")
                .and_then(|p| p.get("protocolVersion"))
                .and_then(Value::as_str)
                .map_or(Value::Null, |v| Value::String(v.to_string()));
            Action::Respond(unsupported_version_frame(id, requested))
        }
        "server/discover" | "ping" | "tools/list" => Action::Forward(normalize_params(value)),
        "tools/call" => classify_call(value, id),
        _ => Action::Respond(error_frame(
            id,
            METHOD_NOT_FOUND,
            &format!("method not found: {method}"),
        )),
    }
}

/// Classify a `tools/call` frame: the baseline's nameless-call
/// shape is answered here for modern-stamped frames; everything
/// else forwards with the baseline's degradation normalized in.
fn classify_call(mut value: Value, id: Value) -> Action {
    let modern_stamped = value
        .get("params")
        .and_then(|p| p.get("_meta"))
        .and_then(|m| m.get("io.modelcontextprotocol/protocolVersion"))
        .and_then(Value::as_str)
        == Some(MODERN_VERSION);
    let name_ok = value
        .get("params")
        .and_then(|p| p.get("name"))
        .is_some_and(Value::is_string);
    if modern_stamped && !name_ok {
        return Action::Respond(missing_name_frame(id));
    }
    let params = params_map(&mut value);
    if !name_ok {
        params.insert("name".to_string(), Value::String(String::new()));
    }
    if let Some(arguments) = params.get("arguments")
        && !arguments.is_object()
    {
        params.insert("arguments".to_string(), json!({}));
    }
    Action::Forward(value)
}

/// The baseline ignored `params` on discover/list/ping entirely; a
/// missing or non-object value is normalized to `{}` so the SDK's
/// typed envelope always parses.
fn normalize_params(mut value: Value) -> Value {
    let object = value.as_object_mut().expect("classified object");
    match object.get("params") {
        Some(p) if p.is_object() => {}
        _ => {
            object.insert("params".to_string(), json!({}));
        }
    }
    value
}

/// Ensure `params` exists as an object and return the frame's
/// params map for further normalization.
fn params_map(value: &mut Value) -> &mut serde_json::Map<String, Value> {
    let object = value.as_object_mut().expect("classified object");
    match object.get("params") {
        Some(p) if p.is_object() => {}
        _ => {
            object.insert("params".to_string(), json!({}));
        }
    }
    object
        .get_mut("params")
        .and_then(Value::as_object_mut)
        .expect("params normalized to an object")
}

async fn write_frame(stdout: &SharedStdout, frame: &Value) {
    let mut line = serde_json::to_string(frame).unwrap_or_default();
    line.push('\n');
    let mut out = stdout.lock().await;
    let _ = out.write_all(line.as_bytes()).await;
    let _ = out.flush().await;
}

async fn write_bytes(stdout: &SharedStdout, bytes: &[u8]) {
    let mut out = stdout.lock().await;
    let _ = out.write_all(bytes).await;
    let _ = out.flush().await;
}

/// One inbound line, bounded: the error trips when the content
/// (excluding the newline) reaches [`MAX_LINE_BYTES`]; an oversized
/// line's remainder is drained.
enum LineOutcome {
    Line(Vec<u8>),
    Oversized,
    Eof,
}

async fn next_line<R: AsyncBufRead + Unpin>(reader: &mut R) -> std::io::Result<LineOutcome> {
    let mut out: Vec<u8> = Vec::new();
    let mut content_len = 0usize;
    let mut oversized = false;
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            return Ok(if oversized {
                LineOutcome::Oversized
            } else if out.is_empty() && content_len == 0 {
                LineOutcome::Eof
            } else {
                LineOutcome::Line(out)
            });
        }
        match chunk.iter().position(|&b| b == b'\n') {
            Some(pos) => {
                content_len += pos;
                if content_len >= MAX_LINE_BYTES {
                    oversized = true;
                }
                if !oversized {
                    out.extend_from_slice(&chunk[..pos]);
                }
                reader.consume(pos + 1);
                return Ok(if oversized {
                    LineOutcome::Oversized
                } else {
                    LineOutcome::Line(out)
                });
            }
            None => {
                let len = chunk.len();
                content_len += len;
                if content_len >= MAX_LINE_BYTES {
                    oversized = true;
                    out.clear();
                } else {
                    out.extend_from_slice(chunk);
                }
                reader.consume(len);
            }
        }
    }
}

/// Run the guard until stdin reaches EOF, then shut the duplex
/// write side down: a split write half signals end-of-input to the
/// SDK service only through an explicit shutdown, never by being
/// dropped (both halves share ownership of the stream).
pub async fn run(
    stdin: Stdin,
    mut to_server: WriteHalf<DuplexStream>,
    stdout: SharedStdout,
    ordering: Arc<Ordering>,
) -> std::io::Result<()> {
    let result = forward_loop(stdin, &mut to_server, &stdout, &ordering).await;
    let _ = to_server.shutdown().await;
    result
}

async fn forward_loop(
    stdin: Stdin,
    to_server: &mut WriteHalf<DuplexStream>,
    stdout: &SharedStdout,
    ordering: &Ordering,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stdin);
    loop {
        match next_line(&mut reader).await? {
            LineOutcome::Eof => return Ok(()),
            LineOutcome::Oversized => {
                ordering.wait_drained().await;
                write_frame(
                    stdout,
                    &error_frame(Value::Null, PARSE_ERROR, "message exceeds size limit"),
                )
                .await;
            }
            LineOutcome::Line(bytes) => match classify(&bytes) {
                Action::Drop => {}
                Action::Respond(frame) => {
                    ordering.wait_drained().await;
                    write_frame(stdout, &frame).await;
                }
                Action::Forward(frame) => {
                    if let Some(id) = frame.get("id").cloned() {
                        ordering.track(id).await;
                    }
                    let mut line = serde_json::to_string(&frame).unwrap_or_default();
                    line.push('\n');
                    to_server.write_all(line.as_bytes()).await?;
                    to_server.flush().await?;
                }
            },
        }
    }
}

/// Copy SDK responses from the duplex to stdout, one line at a time,
/// marking each answered request id complete for the guard's
/// ordering wait.
pub async fn pump(
    mut from_server: ReadHalf<DuplexStream>,
    stdout: SharedStdout,
    ordering: Arc<Ordering>,
) {
    let mut reader = BufReader::new(&mut from_server);
    let mut line: Vec<u8> = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {
                write_bytes(&stdout, &line).await;
                if let Ok(frame) = serde_json::from_slice::<Value>(&line)
                    && let Some(id) = frame.get("id")
                {
                    ordering.complete(id).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify_str(s: &str) -> Action {
        classify(s.as_bytes())
    }

    fn classify_json(frame: &Value) -> Action {
        classify(serde_json::to_string(frame).unwrap().as_bytes())
    }

    fn modern_meta() -> Value {
        json!({
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientInfo": {"name": "test", "version": "1"},
            "io.modelcontextprotocol/clientCapabilities": {},
        })
    }

    fn modern_call(id: u64, params: Value) -> Value {
        let mut params = params;
        params
            .as_object_mut()
            .expect("object params")
            .insert("_meta".to_string(), modern_meta());
        json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": params})
    }

    #[test]
    fn blank_lines_notifications_and_methodless_frames_drop() {
        assert_eq!(classify_str("  \n"), Action::Drop);
        assert_eq!(
            classify_str("{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}"),
            Action::Drop
        );
        // A notification-shaped initialize is a notification.
        assert_eq!(
            classify_str("{\"jsonrpc\":\"2.0\",\"method\":\"initialize\",\"params\":{}}"),
            Action::Drop
        );
        // A notification-shaped tools/call drops unexecuted (the
        // baseline's front end dropped it too — verified in the
        // WS2-W2 baseline capture, errors session).
        assert_eq!(
            classify_str(
                "{\"jsonrpc\":\"2.0\",\"method\":\"tools/call\",\"params\":{\"name\":\"attention_publish\"}}"
            ),
            Action::Drop
        );
        // A response-shaped frame (id, no method) is not answered.
        assert_eq!(
            classify_str("{\"jsonrpc\":\"2.0\",\"id\":9,\"result\":{}}"),
            Action::Drop
        );
    }

    #[test]
    fn framing_errors_match_the_baseline() {
        match classify_str("this is not json") {
            Action::Respond(f) => {
                assert_eq!(f["error"]["code"], PARSE_ERROR);
                assert_eq!(f["error"]["message"], "parse error");
                assert_eq!(f["id"], Value::Null);
            }
            other => panic!("expected respond, got {other:?}"),
        }
        match classify_str("[1,2,3]") {
            Action::Respond(f) => assert_eq!(f["error"]["code"], INVALID_REQUEST),
            other => panic!("expected respond, got {other:?}"),
        }
    }

    #[test]
    fn initialize_is_refused_with_supported_versions() {
        let frame = "{\"id\":7,\"jsonrpc\":\"2.0\",\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-03-26\",\"clientInfo\":{\"name\":\"x\"}}}";
        match classify_str(frame) {
            Action::Respond(f) => {
                assert_eq!(f["id"], 7);
                assert_eq!(f["error"]["code"], UNSUPPORTED_PROTOCOL_VERSION);
                assert_eq!(f["error"]["data"]["requested"], "2025-03-26");
                assert_eq!(f["error"]["data"]["supported"], json!(["2026-07-28"]));
            }
            other => panic!("expected respond, got {other:?}"),
        }
    }

    #[test]
    fn unknown_methods_carry_the_method_name() {
        let frame = "{\"id\":1,\"jsonrpc\":\"2.0\",\"method\":\"resources/list\",\"params\":{}}";
        match classify_str(frame) {
            Action::Respond(f) => {
                assert_eq!(f["error"]["code"], METHOD_NOT_FOUND);
                assert_eq!(f["error"]["message"], "method not found: resources/list");
            }
            other => panic!("expected respond, got {other:?}"),
        }
    }

    #[test]
    fn nameless_modern_calls_get_the_baseline_inband_error() {
        for frame in [
            modern_call(1, json!({"arguments": {}})),
            modern_call(2, json!({"name": 42, "arguments": {}})),
        ] {
            match classify_json(&frame) {
                Action::Respond(f) => {
                    assert_eq!(f["result"]["isError"], json!(true));
                    assert_eq!(
                        f["result"]["content"][0]["text"],
                        "tools/call requires a string `name`"
                    );
                }
                other => panic!("expected respond for {frame}, got {other:?}"),
            }
        }
        // The same shapes without modern _meta forward instead —
        // the SDK's metadata refusal owns them (fleet rule).
        match classify_str("{\"id\":3,\"method\":\"tools/call\",\"params\":{}}") {
            Action::Forward(f) => assert_eq!(f["params"]["name"], ""),
            other => panic!("expected forward, got {other:?}"),
        }
    }

    #[test]
    fn malformed_calls_normalize_to_baseline_degradation() {
        // Empty-arguments encodings ([] is what the WS0 Lua clients
        // send for an empty table) normalize to {}.
        for frame in [
            modern_call(2, json!({"name": "broker_status", "arguments": []})),
            modern_call(3, json!({"name": "broker_status", "arguments": "nope"})),
        ] {
            match classify_json(&frame) {
                Action::Forward(f) => assert_eq!(f["params"]["arguments"], json!({})),
                other => panic!("expected forward for {frame}, got {other:?}"),
            }
        }
        // Non-object params on tools/list normalize to {}.
        match classify_str("{\"id\":9,\"method\":\"tools/list\",\"params\":\"x\"}") {
            Action::Forward(f) => assert_eq!(f["params"], json!({})),
            other => panic!("expected forward, got {other:?}"),
        }
    }

    #[test]
    fn valid_modern_frames_forward() {
        for frame in [
            json!({"id": 1, "method": "server/discover", "params": {"_meta": modern_meta()}}),
            json!({"id": 2, "method": "tools/list", "params": {"_meta": modern_meta()}}),
            modern_call(3, json!({"name": "broker_status", "arguments": {}})),
            json!({"id": 4, "method": "ping"}),
        ] {
            match classify_json(&frame) {
                Action::Forward(_) => {}
                other => panic!("expected forward for {frame}, got {other:?}"),
            }
        }
    }
}
