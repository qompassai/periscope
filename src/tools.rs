//! The MCP tool surface: definitions and dispatch.
//!
//! The `get_*` and `list_*` tools are read-only intelligence over the
//! brokered editor. The `run_*` tools execute the config's own
//! formatter and linter pipelines inside the editor: formatting is
//! applied to the editor's buffer only (never written to disk —
//! write-class tools belong to a later phase behind an explicit
//! enable flag), and linting only reads. Positions follow the LSP
//! convention throughout: 0-based lines, character offsets in
//! UTF-16 code units.
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
/// Grace for a language server to attach before a format call, so
/// the pipeline's LSP fallback has a client to work with.
const FORMAT_ATTACH_GRACE_MS: u64 = 5_000;
/// Overall budget for one formatter or linter execution call.
const EXEC_BUDGET_MS: u64 = 60_000;
/// Deadline handed to the formatter pipeline itself, inside the
/// execution budget so the editor can report its own timeout.
const FORMAT_TIMEOUT_MS: u64 = 55_000;
/// Per-linter deadline handed to the linter runner.
const LINT_TIMEOUT_MS: u64 = 50_000;
/// Most formatter names accepted in one run_format call.
const FORMATTER_NAMES_MAX: usize = 16;
/// Most linter names accepted in one run_lint call.
const LINTER_NAMES_MAX: usize = 32;
/// Longest accepted formatter/linter name.
const TOOL_NAME_MAX_BYTES: usize = 64;
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
        },
        {
            "name": "run_format",
            "description": "Format one file with the editor config's own formatter pipeline and return the result. The formatted text is applied to the brokered editor's buffer and returned in `text`; the file on disk is never written (`written` is always false). With no `formatters`, the filetype's configured pipeline runs (including the pipeline's LSP fallback unless `lsp` is 'never').",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": path_property,
                    "formatters": {
                        "type": "array", "items": { "type": "string" },
                        "description": "Explicit formatter names to run, in order. Omit for the filetype's configured pipeline."
                    },
                    "lsp": {
                        "type": "string", "enum": ["fallback", "never", "only"],
                        "description": "The pipeline's LSP mode. 'only' cannot be combined with explicit `formatters`."
                    }
                },
                "required": ["path"], "additionalProperties": false
            }
        },
        {
            "name": "run_lint",
            "description": "Lint one file with the editor config's own linter runner and return per-linter results: status, exit code, and the diagnostics each linter produced (every diagnostic names its linter in `source`). With no `linters`, the filetype's configured set runs. Nothing is written to disk.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": path_property,
                    "linters": {
                        "type": "array", "items": { "type": "string" },
                        "description": "Explicit linter names to run. Omit for the filetype's configured set."
                    }
                },
                "required": ["path"], "additionalProperties": false
            }
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
        "run_format" => {
            let path = path_arg(args)?;
            let formatters = names_arg(args, "formatters", FORMATTER_NAMES_MAX)?;
            let lsp_mode = lsp_mode_arg(args, !formatters.is_empty())?;
            broker
                .exec_lua(
                    &lua::run_format(),
                    vec![
                        json!(path),
                        json!(formatters),
                        json!(lsp_mode),
                        json!(FORMAT_ATTACH_GRACE_MS),
                        json!(FORMAT_TIMEOUT_MS),
                    ],
                    FORMAT_ATTACH_GRACE_MS + EXEC_BUDGET_MS + RPC_SLACK_MS,
                )
                .await
        }
        "run_lint" => {
            let path = path_arg(args)?;
            let linters = names_arg(args, "linters", LINTER_NAMES_MAX)?;
            broker
                .exec_lua(
                    &lua::run_lint(),
                    vec![
                        json!(path),
                        json!(linters),
                        json!(LINT_TIMEOUT_MS),
                        json!(EXEC_BUDGET_MS),
                    ],
                    EXEC_BUDGET_MS + RPC_SLACK_MS,
                )
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

/// Validate an optional list of formatter/linter names: an array of
/// at most `max` strings, each 1..=64 bytes of `[A-Za-z0-9._-]`.
/// Returns the names sorted and deduplicated for deterministic
/// editor-side behavior; an absent argument yields an empty list
/// (the editor then uses the filetype's configured set).
fn names_arg(args: &JsonValue, key: &str, max: usize) -> Result<Vec<String>> {
    let Some(value) = args.get(key) else {
        return Ok(Vec::new());
    };
    let items = value
        .as_array()
        .ok_or_else(|| Error::InvalidInput(format!("argument {key} must be an array")))?;
    if items.is_empty() {
        return Err(Error::InvalidInput(format!(
            "argument {key} must not be empty when provided"
        )));
    }
    if items.len() > max {
        return Err(Error::InvalidInput(format!(
            "argument {key} accepts at most {max} names, got {}",
            items.len()
        )));
    }
    let mut names: Vec<String> = Vec::with_capacity(items.len());
    for item in items {
        let name = item.as_str().ok_or_else(|| {
            Error::InvalidInput(format!("argument {key} entries must be strings"))
        })?;
        if name.is_empty()
            || name.len() > TOOL_NAME_MAX_BYTES
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        {
            return Err(Error::InvalidInput(format!(
                "invalid name in argument {key}: {name:?}"
            )));
        }
        names.push(name.to_string());
    }
    names.sort();
    names.dedup();
    Ok(names)
}

/// Validate the optional formatter LSP mode. Mirrors the formatter
/// pipeline's own contract: explicit formatter names conflict with
/// `lsp = "only"`. Returns "" when unspecified (the pipeline's
/// configured default then applies).
fn lsp_mode_arg(args: &JsonValue, has_names: bool) -> Result<String> {
    let Some(value) = args.get("lsp") else {
        return Ok(String::new());
    };
    let mode = value
        .as_str()
        .ok_or_else(|| Error::InvalidInput("argument lsp must be a string".to_string()))?;
    if !matches!(mode, "fallback" | "never" | "only") {
        return Err(Error::InvalidInput(format!(
            "argument lsp must be fallback, never, or only: {mode:?}"
        )));
    }
    if has_names && mode == "only" {
        return Err(Error::InvalidInput(
            "argument lsp=only conflicts with explicit formatters".to_string(),
        ));
    }
    Ok(mode.to_string())
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
