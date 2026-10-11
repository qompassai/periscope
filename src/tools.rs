//! The MCP tool surface: definitions and dispatch.
//!
//! Every tool is read-only intelligence over the brokered editor:
//! nothing here edits a buffer, writes a file, or runs a formatter.
//! (Write-class tools belong to a later phase behind an explicit
//! enable flag.) Positions follow the LSP convention throughout:
//! 0-based lines, character offsets in UTF-16 code units.
//!
//! All path arguments are validated before any Lua runs: the target
//! must exist, be a regular file, and be valid UTF-8.

use serde_json::{Value as JsonValue, json};

use crate::broker::{BrokerMode, NvimBroker};
use crate::error::{Error, Result};
use crate::lua;

/// Budget for a language server to attach and initialize.
const ATTACH_BUDGET_MS: u64 = 20_000;
/// Budget for the request phase of an LSP tool call. The editor-side
/// code polls the request while servers report in-flight progress
/// (a cold rust-analyzer needs ~10 s on a small project, longer on a
/// large one), so this is a warm-up budget, not a response latency.
const REQUEST_BUDGET_MS: u64 = 30_000;
/// Budget for diagnostics to settle after attach.
const SETTLE_BUDGET_MS: u64 = 5_000;
/// Slack added to Lua-side budgets to form the RPC deadline.
const RPC_SLACK_MS: u64 = 10_000;
/// Largest accepted line argument (sanity bound, not a file limit).
const LINE_MAX: u64 = 10_000_000;
/// Largest accepted character argument.
const CHARACTER_MAX: u64 = 1_000_000;

/// The tools/list payload: every tool with its input schema.
pub fn definitions() -> JsonValue {
    let path_property = json!({
        "type": "string",
        "description": "Absolute path of the file to inspect. It is loaded into the brokered editor (without being displayed) if it is not open yet."
    });
    let line_property = json!({
        "type": "integer", "minimum": 0,
        "description": "0-based line number."
    });
    let character_property = json!({
        "type": "integer", "minimum": 0,
        "description": "0-based character offset in UTF-16 code units (the LSP convention)."
    });
    json!([
        {
            "name": "broker_status",
            "description": "Report the broker mode (managed or attached), the editor version, its config root, and its working directory.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        },
        {
            "name": "get_definition",
            "description": "Definitions of the symbol at a position, from the editor's attached language servers, merged across clients.",
            "inputSchema": {
                "type": "object",
                "properties": { "path": path_property, "line": line_property, "character": character_property },
                "required": ["path", "line", "character"], "additionalProperties": false
            }
        },
        {
            "name": "get_diagnostics",
            "description": "Diagnostics for one file from every source the editor aggregates (language servers and the linter pipeline), each entry naming its producing tool in `source`.",
            "inputSchema": {
                "type": "object",
                "properties": { "path": path_property },
                "required": ["path"], "additionalProperties": false
            }
        },
        {
            "name": "get_document_symbols",
            "description": "The symbol outline of one file, flattened with container names, from the editor's attached language servers.",
            "inputSchema": {
                "type": "object",
                "properties": { "path": path_property },
                "required": ["path"], "additionalProperties": false
            }
        },
        {
            "name": "get_hover",
            "description": "Hover text (types, documentation) for the symbol at a position, from the editor's attached language servers.",
            "inputSchema": {
                "type": "object",
                "properties": { "path": path_property, "line": line_property, "character": character_property },
                "required": ["path", "line", "character"], "additionalProperties": false
            }
        },
        {
            "name": "get_references",
            "description": "References to the symbol at a position, merged across the editor's attached language servers.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": path_property, "line": line_property, "character": character_property,
                    "include_declaration": { "type": "boolean", "default": true }
                },
                "required": ["path", "line", "character"], "additionalProperties": false
            }
        },
        {
            "name": "list_dap",
            "description": "The debug setup the config ships: per-language dap modules, adapters registered with nvim-dap right now, and launch configurations by filetype.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        },
        {
            "name": "list_formatters",
            "description": "Every formatter in the editor config's registry.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        },
        {
            "name": "list_linters",
            "description": "Every linter in the editor config's registry (internal helpers reported separately).",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        },
        {
            "name": "list_servers",
            "description": "Every language server the editor config defines, plus the clients active in the editor right now.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        }
    ])
}

/// Dispatch one tools/call. Unknown names and invalid arguments are
/// tool errors, not protocol failures.
pub async fn call(
    broker: &NvimBroker,
    mode: BrokerMode,
    name: &str,
    args: &JsonValue,
) -> Result<JsonValue> {
    match name {
        "broker_status" => status(broker, mode).await,
        "get_definition" => {
            let (path, line, character) = position_args(args)?;
            broker
                .exec_lua(
                    &lua::definition(),
                    vec![
                        json!(path),
                        json!(line),
                        json!(character),
                        json!(ATTACH_BUDGET_MS),
                        json!(REQUEST_BUDGET_MS),
                    ],
                    ATTACH_BUDGET_MS + REQUEST_BUDGET_MS + RPC_SLACK_MS,
                )
                .await
        }
        "get_diagnostics" => {
            let path = path_arg(args)?;
            broker
                .exec_lua(
                    &lua::diagnostics(),
                    vec![
                        json!(path),
                        json!(ATTACH_BUDGET_MS),
                        json!(SETTLE_BUDGET_MS),
                    ],
                    ATTACH_BUDGET_MS + SETTLE_BUDGET_MS + RPC_SLACK_MS,
                )
                .await
        }
        "get_document_symbols" => {
            let path = path_arg(args)?;
            broker
                .exec_lua(
                    &lua::document_symbols(),
                    vec![
                        json!(path),
                        json!(ATTACH_BUDGET_MS),
                        json!(REQUEST_BUDGET_MS),
                    ],
                    ATTACH_BUDGET_MS + REQUEST_BUDGET_MS + RPC_SLACK_MS,
                )
                .await
        }
        "get_hover" => {
            let (path, line, character) = position_args(args)?;
            broker
                .exec_lua(
                    &lua::hover(),
                    vec![
                        json!(path),
                        json!(line),
                        json!(character),
                        json!(ATTACH_BUDGET_MS),
                        json!(REQUEST_BUDGET_MS),
                    ],
                    ATTACH_BUDGET_MS + REQUEST_BUDGET_MS + RPC_SLACK_MS,
                )
                .await
        }
        "get_references" => {
            let (path, line, character) = position_args(args)?;
            let include_declaration = args
                .get("include_declaration")
                .and_then(JsonValue::as_bool)
                .unwrap_or(true);
            broker
                .exec_lua(
                    &lua::references(),
                    vec![
                        json!(path),
                        json!(line),
                        json!(character),
                        json!(include_declaration),
                        json!(ATTACH_BUDGET_MS),
                        json!(REQUEST_BUDGET_MS),
                    ],
                    ATTACH_BUDGET_MS + REQUEST_BUDGET_MS + RPC_SLACK_MS,
                )
                .await
        }
        "list_dap" => broker.exec_lua(lua::DAP, Vec::new(), RPC_SLACK_MS).await,
        "list_formatters" => {
            broker
                .exec_lua(lua::FORMATTERS, Vec::new(), RPC_SLACK_MS)
                .await
        }
        "list_linters" => {
            broker
                .exec_lua(lua::LINTERS, Vec::new(), RPC_SLACK_MS)
                .await
        }
        "list_servers" => {
            broker
                .exec_lua(lua::SERVERS, Vec::new(), RPC_SLACK_MS)
                .await
        }
        _ => Err(Error::InvalidInput(format!("unknown tool: {name}"))),
    }
}

async fn status(broker: &NvimBroker, mode: BrokerMode) -> Result<JsonValue> {
    let mut result = broker
        .exec_lua(lua::STATUS, Vec::new(), RPC_SLACK_MS)
        .await?;
    let object = result
        .as_object_mut()
        .ok_or_else(|| Error::BrokerRpc("status returned a non-object".to_string()))?;
    object.insert("mode".to_string(), json!(mode.as_str()));
    object.insert(
        "workspace".to_string(),
        json!(broker.workspace.to_string_lossy().into_owned()),
    );
    Ok(result)
}

/// Validate the `path` argument: present, a string, an existing
/// regular file. Returns the canonical path as a UTF-8 string.
fn path_arg(args: &JsonValue) -> Result<String> {
    let raw = args
        .get("path")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| Error::InvalidInput("missing string argument: path".to_string()))?;
    let canonical = std::fs::canonicalize(raw)
        .map_err(|error| Error::InvalidInput(format!("path does not resolve: {raw} ({error})")))?;
    if !canonical.is_file() {
        return Err(Error::InvalidInput(format!("not a regular file: {raw}")));
    }
    canonical
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| Error::InvalidInput(format!("path is not valid UTF-8: {raw}")))
}

/// Validate a path plus a position.
fn position_args(args: &JsonValue) -> Result<(String, u64, u64)> {
    let path = path_arg(args)?;
    let line = bounded_u64(args, "line", LINE_MAX)?;
    let character = bounded_u64(args, "character", CHARACTER_MAX)?;
    Ok((path, line, character))
}

fn bounded_u64(args: &JsonValue, key: &str, max: u64) -> Result<u64> {
    let value = args
        .get(key)
        .and_then(JsonValue::as_u64)
        .ok_or_else(|| Error::InvalidInput(format!("missing or non-integer argument: {key}")))?;
    if value > max {
        return Err(Error::InvalidInput(format!(
            "argument {key} exceeds {max}: {value}"
        )));
    }
    Ok(value)
}
