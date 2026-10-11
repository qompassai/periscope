//! Conversions between msgpack values (`rmpv::Value`) and JSON values
//! (`serde_json::Value`).
//!
//! The broker speaks msgpack-RPC to Neovim and JSON-RPC (MCP) to its
//! client. Neovim-side Lua snippets are contractually required to return
//! string-keyed tables, so map conversion stringifies any non-string key
//! rather than failing; payload sizes are bounded by the broker's read
//! limits before conversion happens.

use rmpv::Value as MpValue;
use serde_json::{Map, Value as JsonValue};

/// Convert a JSON value into a msgpack value (for RPC parameters).
pub fn json_to_mp(value: &JsonValue) -> MpValue {
    match value {
        JsonValue::Null => MpValue::Nil,
        JsonValue::Bool(flag) => MpValue::Boolean(*flag),
        JsonValue::Number(number) => number_to_mp(number),
        JsonValue::String(text) => MpValue::String(text.clone().into()),
        JsonValue::Array(items) => MpValue::Array(items.iter().map(json_to_mp).collect()),
        JsonValue::Object(entries) => MpValue::Map(
            entries
                .iter()
                .map(|(key, item)| (MpValue::String(key.clone().into()), json_to_mp(item)))
                .collect(),
        ),
    }
}

/// Convert a msgpack value into a JSON value (for RPC results).
///
/// Binary payloads are surfaced as UTF-8 lossy strings; Neovim returns
/// text for everything this server requests, so this is a display-only
/// fallback, never a data path.
pub fn mp_to_json(value: &MpValue) -> JsonValue {
    match value {
        MpValue::Nil => JsonValue::Null,
        MpValue::Boolean(flag) => JsonValue::Bool(*flag),
        MpValue::Integer(integer) => {
            if let Some(unsigned) = integer.as_u64() {
                JsonValue::Number(unsigned.into())
            } else if let Some(signed) = integer.as_i64() {
                JsonValue::Number(signed.into())
            } else {
                JsonValue::Null
            }
        }
        MpValue::F32(number) => json_f64(f64::from(*number)),
        MpValue::F64(number) => json_f64(*number),
        MpValue::String(text) => JsonValue::String(text.clone().into_str().unwrap_or_default()),
        MpValue::Binary(bytes) => JsonValue::String(String::from_utf8_lossy(bytes).into_owned()),
        MpValue::Array(items) => JsonValue::Array(items.iter().map(mp_to_json).collect()),
        MpValue::Map(entries) => {
            let mut object = Map::with_capacity(entries.len());
            for (key, item) in entries {
                let key_text = match key {
                    MpValue::String(text) => text.clone().into_str().unwrap_or_default(),
                    other => mp_scalar_to_string(other),
                };
                object.insert(key_text, mp_to_json(item));
            }
            JsonValue::Object(object)
        }
        MpValue::Ext(_, bytes) => JsonValue::String(String::from_utf8_lossy(bytes).into_owned()),
    }
}

fn json_f64(number: f64) -> JsonValue {
    serde_json::Number::from_f64(number).map_or(JsonValue::Null, JsonValue::Number)
}

fn mp_scalar_to_string(value: &MpValue) -> String {
    match value {
        MpValue::Integer(integer) => integer.to_string(),
        MpValue::Boolean(flag) => flag.to_string(),
        MpValue::F64(number) => number.to_string(),
        _ => String::new(),
    }
}

fn number_to_mp(number: &serde_json::Number) -> MpValue {
    if let Some(unsigned) = number.as_u64() {
        MpValue::Integer(unsigned.into())
    } else if let Some(signed) = number.as_i64() {
        MpValue::Integer(signed.into())
    } else if let Some(float) = number.as_f64() {
        MpValue::F64(float)
    } else {
        MpValue::Nil
    }
}
