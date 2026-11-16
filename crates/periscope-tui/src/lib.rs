#![forbid(unsafe_code)]
//! periscope-tui: the fleet-pattern TUI for the periscope MCP server.
//!
//! Periscope brokers to a real Neovim running the user's diver config,
//! so agents query the same LSP / formatter / linter / DAP tree the
//! user sees. This crate is the operator console for that broker: it
//! programs against a narrow [`facade::Facade`] trait whose real
//! implementation is an MCP stdio client spawning the `periscope`
//! server binary, and whose [`facade::FakeFacade`] implementation is
//! the in-memory scripted double every test uses.
//!
//! Honesty contract: with no editor attached, every screen renders
//! the detached state truthfully. A broker that dies mid-query moves
//! the attach state to `Unknown` and drops every cached result —
//! stale data is never presented as live.
//!
//! Bounds (Tiger Style): every queue, buffer, label, and payload this
//! crate touches carries a named `*_MAX` / `*_MS` constant below.

pub mod app;
pub mod facade;
pub mod panels;
pub mod screens;

/// Most log events retained; older events drop off the front.
pub const LOG_EVENTS_MAX: usize = 256;
/// Most trigger/audit entries retained in the Fleet panel.
pub const AUDIT_ENTRIES_MAX: usize = 128;
/// Most data nodes charged in one capability tree (all sections
/// summed). Section/group scaffolding adds at most
/// TREE_STRUCTURAL_MAX further nodes, so a rendered tree never
/// exceeds TREE_NODES_MAX + TREE_STRUCTURAL_MAX nodes in total.
pub const TREE_NODES_MAX: usize = 512;
/// Most structural (section/group) nodes in one capability tree.
pub const TREE_STRUCTURAL_MAX: usize = 16;
/// Deepest capability-tree nesting rendered.
pub const TREE_DEPTH_MAX: usize = 6;
/// Longest single tree/overview label, in chars.
pub const LABEL_MAX_CHARS: usize = 128;
/// Longest rendered text line, in chars (panel truncation bound).
pub const LINE_MAX_CHARS: usize = 160;
/// Longest accepted workflow target path, in bytes.
pub const TARGET_PATH_MAX_BYTES: usize = 1024;
/// Most result lines copied from one workflow result into the log.
pub const RESULT_LINES_MAX: usize = 64;
/// Most tool names accepted from one tools/list payload.
pub const TOOLS_MAX: usize = 128;
/// Largest single MCP response line accepted, in bytes (matches the
/// periscope server's own 4 MiB inbound cap).
pub const RESPONSE_LINE_MAX_BYTES: usize = 4 * 1024 * 1024;
/// Deadline for one MCP response, in milliseconds. The server's own
/// execution budget is 60 s plus slack; this bound sits above it.
pub const RESPONSE_DEADLINE_MS: u64 = 120_000;
/// Most server arguments accepted on the TUI command line.
pub const SERVER_ARGS_MAX: usize = 16;

/// Truncate `text` to at most `max_chars` chars, marking truncation
/// with a trailing `~`. Char-boundary safe; never splits a codepoint.
pub(crate) fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let kept: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{kept}~")
}

/// The kind of one log event on the shared event stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind {
    /// A workflow run started.
    Started,
    /// A workflow run is in progress.
    Progress,
    /// A workflow run finished successfully.
    Done,
    /// A workflow run failed; the text names the failure class.
    Failed,
    /// Operator-facing information (staging, approvals, state).
    Info,
}

impl EventKind {
    /// Short stable label used in log and line-mode rendering.
    pub fn label(self) -> &'static str {
        match self {
            EventKind::Started => "STARTED",
            EventKind::Progress => "PROGRESS",
            EventKind::Done => "DONE",
            EventKind::Failed => "FAILED",
            EventKind::Info => "INFO",
        }
    }
}

/// One bounded log event: a kind plus one truncated line of text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogEvent {
    /// What happened.
    pub kind: EventKind,
    /// One line of human-readable detail, bounded to LINE_MAX_CHARS.
    pub text: String,
}

impl LogEvent {
    /// Build an event, truncating its text to the line bound.
    pub fn new(kind: EventKind, text: impl Into<String>) -> LogEvent {
        LogEvent {
            kind,
            text: truncate_chars(&text.into(), LINE_MAX_CHARS),
        }
    }
}

/// Whether the brokered editor is known-attached, known-detached, or
/// in an unknown state (a query failed after earlier success, so no
/// cached answer may be presented as live).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachState {
    /// The last status query succeeded against a live editor.
    Attached,
    /// The server reports it cannot reach any editor.
    Detached,
    /// State cannot be claimed: never queried, or a query failed
    /// mid-session. Renders as unknown, never as attached.
    Unknown,
}

impl AttachState {
    /// Stable label for overview and line-mode rendering.
    pub fn label(self) -> &'static str {
        match self {
            AttachState::Attached => "ATTACHED",
            AttachState::Detached => "DETACHED",
            AttachState::Unknown => "UNKNOWN",
        }
    }
}
