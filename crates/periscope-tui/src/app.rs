//! The app: screen state machine, workflow staging/execution, the
//! bounded event log and audit trail, key handling, and the non-TTY
//! line mode that shares the same execute path.
//!
//! State-transition contract: refresh and workflow calls validate
//! through the facade first; only a successful status commits the
//! `Attached` state and caches. A `Detached` answer commits the
//! detached state. A `Stale` failure after earlier success moves the
//! state to `Unknown` and drops every cache — a dead editor's last
//! answer is never presented as live.

use std::collections::VecDeque;
use std::io::{BufRead, Write};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::{Value, json};

use crate::facade::{Facade, FacadeError};
use crate::screens::{self, CapabilityTree};
use crate::{
    AUDIT_ENTRIES_MAX, AttachState, EventKind, LOG_EVENTS_MAX, LogEvent, RESULT_LINES_MAX,
    TARGET_PATH_MAX_BYTES,
};

/// The five screens of the console.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    /// Broker status and attached-editor identity.
    Overview,
    /// LSP / formatter / linter / DAP capability tree.
    Tree,
    /// Workflow list, target, plan/auto staging and approval.
    Workflows,
    /// Bounded scrolling event log.
    Log,
    /// Fleet panel: trigger rules (none declared) and audit tail.
    Fleet,
}

impl Screen {
    /// All screens in tab order.
    pub const ALL: [Screen; 5] = [
        Screen::Overview,
        Screen::Tree,
        Screen::Workflows,
        Screen::Log,
        Screen::Fleet,
    ];

    /// Stable screen title.
    pub fn title(self) -> &'static str {
        match self {
            Screen::Overview => "Overview",
            Screen::Tree => "Tree",
            Screen::Workflows => "Workflows",
            Screen::Log => "Log",
            Screen::Fleet => "Fleet",
        }
    }

    fn next(self) -> Screen {
        let index = Screen::ALL
            .iter()
            .position(|item| *item == self)
            .unwrap_or(0);
        Screen::ALL[(index + 1) % Screen::ALL.len()]
    }
}

/// Whether a workflow run is staged for approval or executes at once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunMode {
    /// Stage the exact tool + args; nothing is called until approval.
    Plan,
    /// Execute declared operations directly.
    Auto,
}

impl RunMode {
    /// Stable label.
    pub fn label(self) -> &'static str {
        match self {
            RunMode::Plan => "plan",
            RunMode::Auto => "auto",
        }
    }
}

/// One declared workflow: a named operation over the member's tools.
#[derive(Clone, Copy, Debug)]
pub struct WorkflowDef {
    /// Stable id used by line mode (`run <id>`).
    pub id: &'static str,
    /// Display title.
    pub title: &'static str,
    /// The MCP tool the run calls.
    pub tool: &'static str,
    /// Whether the run needs a target file path.
    pub needs_target: bool,
}

/// The member's primary operations, as a selectable list.
pub const WORKFLOWS: [WorkflowDef; 5] = [
    WorkflowDef {
        id: "diagnostics",
        title: "Diagnostics for a file",
        tool: "get_diagnostics",
        needs_target: true,
    },
    WorkflowDef {
        id: "formatter-check",
        title: "Formatter check for a file (buffer only, never written)",
        tool: "run_format",
        needs_target: true,
    },
    WorkflowDef {
        id: "lint",
        title: "Lint a file",
        tool: "run_lint",
        needs_target: true,
    },
    WorkflowDef {
        id: "symbols",
        title: "Document symbols for a file",
        tool: "get_document_symbols",
        needs_target: true,
    },
    WorkflowDef {
        id: "dap-state",
        title: "DAP state of the attached editor",
        tool: "list_dap",
        needs_target: false,
    },
];

/// A staged plan-mode run: the exact tool and args awaiting approval.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StagedRun {
    /// Workflow id the run came from.
    pub workflow_id: String,
    /// Tool that will be called on approval.
    pub tool: String,
    /// Exact arguments that will be sent on approval.
    pub args: Value,
}

/// One audit-trail entry (Fleet panel): what was evaluated, staged,
/// executed, refused, or dropped, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditEntry {
    /// Entry kind: staged / approved / aborted / executed / failed /
    /// state.
    pub kind: String,
    /// One line of detail.
    pub text: String,
}

/// The console application, generic over its facade so tests drive
/// the whole state machine against [`crate::facade::FakeFacade`].
pub struct App<F: Facade> {
    facade: F,
    screen: Screen,
    attach: AttachState,
    status_cache: Option<Value>,
    tool_names: Option<Vec<String>>,
    tree: Option<CapabilityTree>,
    tree_error: Option<String>,
    selected_workflow: usize,
    mode: RunMode,
    target: String,
    editing_target: bool,
    staged: Option<StagedRun>,
    events: VecDeque<LogEvent>,
    audit: VecDeque<AuditEntry>,
    should_quit: bool,
}

impl<F: Facade> App<F> {
    /// A fresh app: nothing queried yet, so the state is honestly
    /// `Unknown` and every cache is empty.
    pub fn new(facade: F) -> App<F> {
        App {
            facade,
            screen: Screen::Overview,
            attach: AttachState::Unknown,
            status_cache: None,
            tool_names: None,
            tree: None,
            tree_error: None,
            selected_workflow: 0,
            mode: RunMode::Plan,
            target: String::new(),
            editing_target: false,
            staged: None,
            events: VecDeque::new(),
            audit: VecDeque::new(),
            should_quit: false,
        }
    }

    /// The facade (tests inspect the fake's recorded calls here).
    pub fn facade(&self) -> &F {
        &self.facade
    }

    /// Current screen.
    pub fn screen(&self) -> Screen {
        self.screen
    }

    /// Current attach state.
    pub fn attach(&self) -> AttachState {
        self.attach
    }

    /// Whether the event loop should exit.
    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    /// The staged plan-mode run, if one awaits approval.
    pub fn staged(&self) -> Option<&StagedRun> {
        self.staged.as_ref()
    }

    /// The bounded event log, oldest first.
    pub fn events(&self) -> &VecDeque<LogEvent> {
        &self.events
    }

    /// The bounded audit trail, oldest first.
    pub fn audit(&self) -> &VecDeque<AuditEntry> {
        &self.audit
    }

    /// Declared trigger rules: none tonight (DESIGN D4 lands per
    /// member as rules are declared; the panel renders this count).
    pub fn trigger_rule_count(&self) -> usize {
        0
    }

    fn push_event(&mut self, kind: EventKind, text: impl Into<String>) {
        if self.events.len() >= LOG_EVENTS_MAX {
            self.events.pop_front();
        }
        self.events.push_back(LogEvent::new(kind, text));
    }

    fn push_audit(&mut self, kind: &str, text: impl Into<String>) {
        if self.audit.len() >= AUDIT_ENTRIES_MAX {
            self.audit.pop_front();
        }
        self.audit.push_back(AuditEntry {
            kind: kind.to_string(),
            text: text.into(),
        });
    }

    /// Drop every cached editor answer. Called whenever editor state
    /// stops being claimable, so stale data cannot render as live.
    fn invalidate_caches(&mut self) {
        self.status_cache = None;
        self.tree = None;
        self.tree_error = None;
    }

    /// Apply a facade failure to the attach state, honestly.
    fn apply_error(&mut self, error: &FacadeError) {
        match error {
            FacadeError::Detached(_) => {
                self.attach = AttachState::Detached;
                self.invalidate_caches();
            }
            FacadeError::Stale(_) => {
                self.attach = AttachState::Unknown;
                self.invalidate_caches();
            }
            _ => {}
        }
    }

    /// Query broker status and the tool list; commit the outcome.
    pub fn refresh(&mut self) {
        match self.facade.status() {
            Ok(status) => {
                self.attach = AttachState::Attached;
                self.status_cache = Some(status);
                self.push_event(EventKind::Info, "broker status: editor attached");
            }
            Err(error) => {
                self.push_event(EventKind::Failed, screens::error_line(&error));
                self.apply_error(&error);
                if !matches!(error, FacadeError::Detached(_) | FacadeError::Stale(_)) {
                    self.attach = AttachState::Unknown;
                    self.invalidate_caches();
                }
            }
        }
        match self.facade.tools() {
            Ok(names) => self.tool_names = Some(names),
            Err(error) => {
                self.push_event(EventKind::Failed, screens::error_line(&error));
            }
        }
    }

    /// Load the capability tree via the four registry tools. Any
    /// failure leaves `tree` empty and records the honest reason.
    pub fn load_tree(&mut self) {
        let servers = self.facade.call("list_servers", json!({}));
        let formatters = self.facade.call("list_formatters", json!({}));
        let linters = self.facade.call("list_linters", json!({}));
        let dap = self.facade.call("list_dap", json!({}));
        let payloads = match (servers, formatters, linters, dap) {
            (Ok(a), Ok(b), Ok(c), Ok(d)) => (a, b, c, d),
            (Err(error), _, _, _)
            | (_, Err(error), _, _)
            | (_, _, Err(error), _)
            | (_, _, _, Err(error)) => {
                self.push_event(EventKind::Failed, screens::error_line(&error));
                self.apply_error(&error);
                self.tree = None;
                self.tree_error = Some(error.to_string());
                return;
            }
        };
        match screens::build_tree(&payloads.0, &payloads.1, &payloads.2, &payloads.3) {
            Ok(tree) => {
                self.push_event(
                    EventKind::Info,
                    format!("capability tree loaded: {} nodes", tree.node_count()),
                );
                self.tree = Some(tree);
                self.tree_error = None;
            }
            Err(message) => {
                self.push_event(
                    EventKind::Failed,
                    format!("malformed capability payload: {message}"),
                );
                self.tree = None;
                self.tree_error = Some(message);
            }
        }
    }

    /// Set the workflow target path, bounded; oversized input is
    /// rejected whole, never silently truncated into a wrong path.
    pub fn set_target(&mut self, target: &str) -> Result<(), String> {
        if target.len() > TARGET_PATH_MAX_BYTES {
            return Err(format!("target exceeds {TARGET_PATH_MAX_BYTES} bytes"));
        }
        self.target = target.to_string();
        Ok(())
    }

    /// The current target path.
    pub fn target(&self) -> &str {
        &self.target
    }

    /// Toggle plan/auto mode.
    pub fn toggle_mode(&mut self) {
        self.mode = match self.mode {
            RunMode::Plan => RunMode::Auto,
            RunMode::Auto => RunMode::Plan,
        };
        self.push_event(EventKind::Info, format!("run mode: {}", self.mode.label()));
    }

    /// Current run mode.
    pub fn run_mode(&self) -> RunMode {
        self.mode
    }

    /// The currently selected workflow definition.
    pub fn selected_workflow(&self) -> &WorkflowDef {
        &WORKFLOWS[self.selected_workflow]
    }

    /// Build the exact args a workflow run would send.
    fn args_for(&self, workflow: &WorkflowDef) -> Value {
        if workflow.needs_target {
            json!({"path": self.target})
        } else {
            json!({})
        }
    }

    /// Stage (plan mode) or execute (auto mode) the selected
    /// workflow. Plan mode performs **no** facade call.
    pub fn stage_or_run(&mut self) {
        let workflow = *self.selected_workflow();
        if workflow.needs_target && self.target.trim().is_empty() {
            self.push_event(
                EventKind::Failed,
                format!("workflow {} needs a target file first", workflow.id),
            );
            return;
        }
        let staged = StagedRun {
            workflow_id: workflow.id.to_string(),
            tool: workflow.tool.to_string(),
            args: self.args_for(&workflow),
        };
        match self.mode {
            RunMode::Plan => {
                self.push_audit(
                    "staged",
                    format!("{} {} {}", staged.workflow_id, staged.tool, staged.args),
                );
                self.push_event(
                    EventKind::Info,
                    format!(
                        "staged {}: {} {} (a = approve, x = abort)",
                        staged.workflow_id, staged.tool, staged.args
                    ),
                );
                self.staged = Some(staged);
            }
            RunMode::Auto => self.execute(staged),
        }
    }

    /// Approve the staged run: exactly one facade call with the
    /// staged tool and args. Approving with nothing staged is a
    /// no-op that records why.
    pub fn approve(&mut self) {
        let Some(staged) = self.staged.take() else {
            self.push_event(EventKind::Info, "approve ignored: nothing is staged");
            return;
        };
        self.push_audit(
            "approved",
            format!("{} {}", staged.workflow_id, staged.tool),
        );
        self.execute(staged);
    }

    /// Abort the staged run. The staged call never happens.
    pub fn abort(&mut self) {
        if let Some(staged) = self.staged.take() {
            self.push_audit("aborted", format!("{} {}", staged.workflow_id, staged.tool));
            self.push_event(
                EventKind::Info,
                format!("aborted staged run {}", staged.workflow_id),
            );
        } else {
            self.push_event(EventKind::Info, "abort ignored: nothing is staged");
        }
    }

    /// Execute one run against the facade, streaming the outcome
    /// into the event log and the audit trail.
    fn execute(&mut self, staged: StagedRun) {
        self.push_event(
            EventKind::Started,
            format!("{} via {}", staged.workflow_id, staged.tool),
        );
        match self.facade.call(&staged.tool, staged.args.clone()) {
            Ok(result) => {
                self.push_event(
                    EventKind::Done,
                    format!(
                        "{}: {}",
                        staged.workflow_id,
                        screens::result_summary(&result)
                    ),
                );
                self.push_audit(
                    "executed",
                    format!("{} {}", staged.workflow_id, staged.tool),
                );
                if staged.tool == "list_dap" {
                    // A DAP-state run also refreshes the tree's DAP
                    // section source data; the tree reloads lazily on
                    // its own screen, so nothing is cached here.
                    self.push_event(
                        EventKind::Progress,
                        "dap state received; open Tree (r) for the full tree",
                    );
                }
                let _ = RESULT_LINES_MAX; // result lines bound, log holds the summary
            }
            Err(error) => {
                self.push_event(
                    EventKind::Failed,
                    format!(
                        "{} failed: {}",
                        staged.workflow_id,
                        screens::error_line(&error)
                    ),
                );
                self.push_audit(
                    "failed",
                    format!("{} {}: {error}", staged.workflow_id, staged.tool),
                );
                self.apply_error(&error);
            }
        }
    }

    /// Lines for the current screen (shared by panels + line mode).
    pub fn screen_lines(&self) -> Vec<String> {
        match self.screen {
            Screen::Overview => screens::overview_lines(
                self.status_cache.as_ref(),
                self.attach,
                self.tool_names.as_ref().map(Vec::len),
                self.trigger_rule_count(),
                self.events.back(),
            ),
            Screen::Tree => match &self.tree_error {
                Some(message) if self.attach == AttachState::Attached => vec![
                    format!("capability tree unavailable: {message}"),
                    "the payload was rejected whole; no partial tree is shown".to_string(),
                ],
                _ => screens::tree_lines(self.tree.as_ref(), self.attach),
            },
            Screen::Workflows => self.workflow_lines(),
            Screen::Log => {
                if self.events.is_empty() {
                    vec!["(no events yet)".to_string()]
                } else {
                    self.events
                        .iter()
                        .map(|event| format!("{} {}", event.kind.label(), event.text))
                        .collect()
                }
            }
            Screen::Fleet => self.fleet_lines(),
        }
    }

    fn workflow_lines(&self) -> Vec<String> {
        let mut lines = vec![
            format!(
                "run mode: {} (p toggles)   target: {}",
                self.mode.label(),
                if self.target.is_empty() {
                    "(none — t to edit)"
                } else {
                    &self.target
                }
            ),
            "provider routing: N/A — periscope routes no model calls".to_string(),
        ];
        for (index, workflow) in WORKFLOWS.iter().enumerate() {
            let marker = if index == self.selected_workflow {
                ">"
            } else {
                " "
            };
            lines.push(format!(
                "{marker} {} — {} [{}]",
                workflow.id, workflow.title, workflow.tool
            ));
        }
        match &self.staged {
            Some(staged) => lines.push(format!(
                "STAGED: {} {} — a approves, x aborts",
                staged.tool, staged.args
            )),
            None => lines.push("nothing staged".to_string()),
        }
        lines
    }

    fn fleet_lines(&self) -> Vec<String> {
        let mut lines = vec![
            "fleet / MCP panel".to_string(),
            format!("this member: periscope {}", env!("CARGO_PKG_VERSION")),
            format!("editor state: {}", self.attach.label()),
            "outbound trigger rules: none declared (no triggers.toml rules tonight)".to_string(),
            "inbound trigger rules: none declared".to_string(),
            "audit trail (newest last):".to_string(),
        ];
        if self.audit.is_empty() {
            lines.push("(audit empty — nothing has been staged or run)".to_string());
        }
        for entry in &self.audit {
            lines.push(format!("[{}] {}", entry.kind, entry.text));
        }
        lines
    }

    /// Handle one key event. Pure state transitions only; all I/O
    /// lives in the facade calls the transitions trigger.
    pub fn handle_key(&mut self, key: KeyEvent) {
        if self.editing_target {
            match key.code {
                KeyCode::Enter | KeyCode::Esc => self.editing_target = false,
                KeyCode::Backspace => {
                    self.target.pop();
                }
                KeyCode::Char(character) if self.target.len() < TARGET_PATH_MAX_BYTES => {
                    self.target.push(character);
                }
                _ => {}
            }
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Tab => self.screen = self.screen.next(),
            KeyCode::Char(digit @ '1'..='5') => {
                let index = (digit as usize) - ('1' as usize);
                self.screen = Screen::ALL[index];
            }
            KeyCode::Char('r') => match self.screen {
                Screen::Tree => self.load_tree(),
                _ => self.refresh(),
            },
            KeyCode::Char('p') if self.screen == Screen::Workflows => self.toggle_mode(),
            KeyCode::Char('t') if self.screen == Screen::Workflows => {
                self.editing_target = true;
            }
            KeyCode::Char('a') if self.screen == Screen::Workflows => self.approve(),
            KeyCode::Char('x') if self.screen == Screen::Workflows => self.abort(),
            KeyCode::Enter if self.screen == Screen::Workflows => self.stage_or_run(),
            KeyCode::Char('j') | KeyCode::Down if self.screen == Screen::Workflows => {
                self.selected_workflow = (self.selected_workflow + 1).min(WORKFLOWS.len() - 1);
            }
            KeyCode::Char('k') | KeyCode::Up if self.screen == Screen::Workflows => {
                self.selected_workflow = self.selected_workflow.saturating_sub(1);
            }
            _ => {}
        }
    }
}

/// Run the non-TTY line mode over `input`, writing to `output`.
/// Commands share the app's execute path with the full-screen UI:
/// `status`, `tools`, `tree`, `workflows`, `target <path>`, `mode`,
/// `run <workflow-id>`, `approve`, `abort`, `log`, `fleet`, `quit`.
pub fn run_line_mode<F: Facade>(
    app: &mut App<F>,
    input: impl BufRead,
    output: &mut impl Write,
) -> std::io::Result<()> {
    for line in input.lines() {
        let line = line?;
        let mut parts = line.splitn(2, ' ');
        let command = parts.next().unwrap_or("").trim();
        let rest = parts.next().unwrap_or("").trim();
        match command {
            "" => {}
            "quit" | "exit" => break,
            "status" => {
                app.refresh();
                print_lines(app, Screen::Overview, output)?;
            }
            "tools" => {
                app.refresh();
                match app.tool_names.as_ref() {
                    Some(names) => {
                        for name in names {
                            writeln!(output, "{name}")?;
                        }
                    }
                    None => writeln!(output, "(tool list unavailable)")?,
                }
            }
            "tree" => {
                app.load_tree();
                print_lines(app, Screen::Tree, output)?;
            }
            "workflows" => print_lines(app, Screen::Workflows, output)?,
            "target" => match app.set_target(rest) {
                Ok(()) => writeln!(output, "target: {}", app.target())?,
                Err(message) => writeln!(output, "error: {message}")?,
            },
            "mode" => {
                app.toggle_mode();
                writeln!(output, "run mode: {}", app.run_mode().label())?;
            }
            "run" => {
                let Some(index) = WORKFLOWS.iter().position(|item| item.id == rest) else {
                    writeln!(output, "error: unknown workflow {rest:?}")?;
                    continue;
                };
                app.selected_workflow = index;
                app.stage_or_run();
                print_lines(app, Screen::Log, output)?;
            }
            "approve" => {
                app.approve();
                print_lines(app, Screen::Log, output)?;
            }
            "abort" => {
                app.abort();
                print_lines(app, Screen::Log, output)?;
            }
            "log" => print_lines(app, Screen::Log, output)?,
            "fleet" => print_lines(app, Screen::Fleet, output)?,
            "help" => {
                writeln!(
                    output,
                    "commands: status tools tree workflows target <path> mode \
                     run <id> approve abort log fleet quit"
                )?;
            }
            other => writeln!(output, "error: unknown command {other:?} (try help)")?,
        }
        output.flush()?;
        if app.should_quit() {
            break;
        }
    }
    Ok(())
}

fn print_lines<F: Facade>(
    app: &App<F>,
    screen: Screen,
    output: &mut impl Write,
) -> std::io::Result<()> {
    for line in lines_for(app, screen) {
        writeln!(output, "{line}")?;
    }
    Ok(())
}

/// Render one named screen's lines regardless of the current screen.
fn lines_for<F: Facade>(app: &App<F>, screen: Screen) -> Vec<String> {
    match screen {
        Screen::Overview => screens::overview_lines(
            app.status_cache.as_ref(),
            app.attach,
            app.tool_names.as_ref().map(Vec::len),
            app.trigger_rule_count(),
            app.events.back(),
        ),
        Screen::Tree => match &app.tree_error {
            Some(message) if app.attach == AttachState::Attached => {
                vec![format!("capability tree unavailable: {message}")]
            }
            _ => screens::tree_lines(app.tree.as_ref(), app.attach),
        },
        Screen::Workflows => app.workflow_lines(),
        Screen::Log => {
            if app.events.is_empty() {
                vec!["(no events yet)".to_string()]
            } else {
                app.events
                    .iter()
                    .map(|event| format!("{} {}", event.kind.label(), event.text))
                    .collect()
            }
        }
        Screen::Fleet => app.fleet_lines(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::FakeFacade;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn attached_app() -> App<FakeFacade> {
        let mut app = App::new(FakeFacade::attached());
        app.refresh();
        app
    }

    #[test]
    fn plan_mode_stages_without_any_workflow_call() {
        let mut app = attached_app();
        let baseline = app.facade().call_count();
        app.set_target("/tmp/x.rs").expect("target fits");
        app.stage_or_run();
        assert!(app.staged().is_some());
        assert_eq!(
            app.facade().call_count(),
            baseline,
            "plan mode must not call the facade before approval"
        );
    }

    #[test]
    fn approve_executes_exactly_once_with_staged_args() {
        let mut app = attached_app();
        app.set_target("/tmp/x.rs").expect("target fits");
        app.stage_or_run();
        app.approve();
        let calls = app.facade().recorded_calls();
        let runs: Vec<&(String, Value)> = calls
            .iter()
            .filter(|(tool, _)| tool == "get_diagnostics")
            .collect();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].1, json!({"path": "/tmp/x.rs"}));
        assert!(app.staged().is_none());
    }

    #[test]
    fn approve_then_approve_again_calls_nothing_more() {
        let mut app = attached_app();
        app.set_target("/tmp/x.rs").expect("target fits");
        app.stage_or_run();
        app.approve();
        let after_first = app.facade().call_count();
        app.approve();
        assert_eq!(app.facade().call_count(), after_first);
    }

    #[test]
    fn approval_then_abort_never_calls() {
        let mut app = attached_app();
        let baseline = app.facade().call_count();
        app.set_target("/tmp/x.rs").expect("target fits");
        app.stage_or_run();
        app.abort();
        assert!(app.staged().is_none());
        assert_eq!(app.facade().call_count(), baseline);
    }

    #[test]
    fn auto_mode_executes_immediately() {
        let mut app = attached_app();
        app.toggle_mode();
        app.set_target("/tmp/x.rs").expect("target fits");
        app.stage_or_run();
        assert!(app.staged().is_none());
        assert!(
            app.facade()
                .recorded_calls()
                .iter()
                .any(|(tool, _)| tool == "get_diagnostics")
        );
    }

    #[test]
    fn workflow_without_target_is_refused_before_staging() {
        let mut app = attached_app();
        let baseline = app.facade().call_count();
        app.stage_or_run();
        assert!(app.staged().is_none());
        assert_eq!(app.facade().call_count(), baseline);
        assert!(
            app.events()
                .iter()
                .any(|event| event.text.contains("needs a target"))
        );
    }

    #[test]
    fn detached_app_renders_detached_everywhere() {
        let mut app = App::new(FakeFacade::detached());
        app.refresh();
        assert_eq!(app.attach(), AttachState::Detached);
        app.screen = Screen::Overview;
        let joined = app.screen_lines().join("\n");
        assert!(joined.contains("DETACHED"));
        assert!(joined.contains("no editor is attached"));
        app.load_tree();
        app.screen = Screen::Tree;
        let joined = app.screen_lines().join("\n");
        assert!(joined.contains("no editor is attached"));
    }

    #[test]
    fn editor_dying_mid_query_moves_state_to_unknown_and_drops_caches() {
        let mut fake = FakeFacade::attached();
        fake.push_call(
            "get_diagnostics",
            Err(FacadeError::Stale(
                "neovim broker connection closed".to_string(),
            )),
        );
        let mut app = App::new(fake);
        app.refresh();
        assert_eq!(app.attach(), AttachState::Attached);
        app.load_tree();
        assert!(app.tree.is_some());
        app.toggle_mode(); // auto
        app.set_target("/tmp/x.rs").expect("target fits");
        app.stage_or_run();
        assert_eq!(app.attach(), AttachState::Unknown);
        assert!(app.tree.is_none(), "stale tree must be dropped");
        assert!(app.status_cache.is_none(), "stale status must be dropped");
        app.screen = Screen::Overview;
        let joined = app.screen_lines().join("\n");
        assert!(joined.contains("UNKNOWN"));
        assert!(
            !joined.contains("0.11.4"),
            "dead editor's version must not render as live"
        );
    }

    #[test]
    fn malformed_tree_payload_is_rejected_whole() {
        let mut fake = FakeFacade::attached();
        fake.set_call_default("list_servers", Ok(json!(["bogus"])));
        let mut app = App::new(fake);
        app.refresh();
        app.load_tree();
        assert!(app.tree.is_none());
        app.screen = Screen::Tree;
        let joined = app.screen_lines().join("\n");
        assert!(joined.contains("not a JSON object"));
    }

    #[test]
    fn oversized_target_is_rejected() {
        let mut app = attached_app();
        let huge = "x".repeat(TARGET_PATH_MAX_BYTES + 1);
        assert!(app.set_target(&huge).is_err());
        assert!(app.target().is_empty());
    }

    #[test]
    fn log_is_bounded() {
        let mut app = attached_app();
        for index in 0..(LOG_EVENTS_MAX + 50) {
            app.push_event(EventKind::Info, format!("event {index}"));
        }
        assert_eq!(app.events().len(), LOG_EVENTS_MAX);
    }

    #[test]
    fn fleet_panel_shows_no_rules_and_audit_after_run() {
        let mut app = attached_app();
        app.set_target("/tmp/x.rs").expect("target fits");
        app.stage_or_run();
        app.approve();
        app.screen = Screen::Fleet;
        let joined = app.screen_lines().join("\n");
        assert!(joined.contains("outbound trigger rules: none declared"));
        assert!(joined.contains("inbound trigger rules: none declared"));
        assert!(joined.contains("[staged]"));
        assert!(joined.contains("[executed]"));
    }

    #[test]
    fn key_navigation_and_quit() {
        let mut app = attached_app();
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.screen(), Screen::Tree);
        app.handle_key(key(KeyCode::Char('3')));
        assert_eq!(app.screen(), Screen::Workflows);
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.selected_workflow().id, "formatter-check");
        app.handle_key(key(KeyCode::Char('q')));
        assert!(app.should_quit());
    }

    #[test]
    fn target_editing_via_keys() {
        let mut app = attached_app();
        app.screen = Screen::Workflows;
        app.handle_key(key(KeyCode::Char('t')));
        for character in "/tmp/y.rs".chars() {
            app.handle_key(key(KeyCode::Char(character)));
        }
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.target(), "/tmp/y.rs");
    }

    #[test]
    fn line_mode_plan_then_approve_proves_the_no_call_gap() {
        let mut app = App::new(FakeFacade::attached());
        let input = b"target /tmp/x.rs\nrun diagnostics\napprove\nquit\n".to_vec();
        let mut output: Vec<u8> = Vec::new();
        run_line_mode(&mut app, input.as_slice(), &mut output).expect("line mode runs");
        let text = String::from_utf8(output).expect("utf8 output");
        assert!(
            text.contains("staged diagnostics"),
            "plan staging shown: {text}"
        );
        let recorded = app.facade().recorded_calls();
        let runs: Vec<&(String, Value)> = recorded
            .iter()
            .filter(|(tool, _)| tool == "get_diagnostics")
            .collect();
        assert_eq!(runs.len(), 1, "exactly the approved run happened");
    }

    #[test]
    fn line_mode_detached_status_is_honest() {
        let mut app = App::new(FakeFacade::detached());
        let input = b"status\nquit\n".to_vec();
        let mut output: Vec<u8> = Vec::new();
        run_line_mode(&mut app, input.as_slice(), &mut output).expect("line mode runs");
        let text = String::from_utf8(output).expect("utf8 output");
        assert!(text.contains("DETACHED"));
        assert!(text.contains("no editor is attached"));
    }
}
