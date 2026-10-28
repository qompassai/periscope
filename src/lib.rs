#![forbid(unsafe_code)]
//! Periscope: an MCP server that brokers into a Neovim instance
//! running the user's own config.
//!
//! Instead of re-implementing editor intelligence (or spawning
//! duplicate language servers the way a standalone LSP bridge does),
//! periscope executes small Lua programs inside a real editor — one
//! the user already has running (attach mode) or a headless one it
//! spawns and owns (managed mode) — so every answer carries that
//! config's exact servers, settings, formatter and linter registries,
//! and debug setup. Slice 1 is read-only: status, registry listings,
//! diagnostics, hover, definition, references, and document symbols.

pub mod attention;
pub mod broker;
pub mod codec;
pub mod error;
pub mod guard;
pub mod lua;
pub mod server;
pub mod tools;
