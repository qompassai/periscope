//! Typed errors for the broker and the MCP front end.
//!
//! Contract: expected failures (bad input, a Neovim-side error, a timeout,
//! a dead child) are values of this enum, never panics. Messages are
//! bounded and safe to hand to an MCP client: they never carry secrets,
//! environment dumps, or unbounded payloads.

use std::fmt;

/// Every failure the server can report to a caller.
#[derive(Debug)]
pub enum Error {
    /// The Neovim child exited or the socket closed mid-request.
    BrokerClosed,
    /// Neovim itself returned an RPC error; payload is its message text.
    BrokerRpc(String),
    /// Caller supplied invalid input (missing file, bad position, ...).
    InvalidInput(String),
    /// A bounded wait expired. Carries the operation name and the budget.
    Timeout {
        operation: &'static str,
        budget_ms: u64,
    },
    /// Local I/O failure (spawn, socket, stdio). Carries context, not the
    /// raw OS error chain, so messages stay stable across platforms.
    Io {
        context: &'static str,
        detail: String,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::BrokerClosed => write!(f, "neovim broker connection closed"),
            Error::BrokerRpc(message) => write!(f, "neovim error: {message}"),
            Error::InvalidInput(message) => write!(f, "invalid input: {message}"),
            Error::Timeout {
                operation,
                budget_ms,
            } => {
                write!(f, "timed out after {budget_ms} ms waiting for {operation}")
            }
            Error::Io { context, detail } => write!(f, "{context}: {detail}"),
        }
    }
}

impl std::error::Error for Error {}

/// Convenience alias for broker and tool results.
pub type Result<T> = std::result::Result<T, Error>;
