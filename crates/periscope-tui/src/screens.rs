//! Screen content: pure text builders plus the capability-tree
//! builder. Every function here is deterministic and terminal-free —
//! panels wrap these lines in ratatui widgets, line mode prints
//! them, and tests assert on them directly.
//!
//! The tree builder consumes the four registry payloads periscope's
//! Lua side returns (`list_servers`, `list_formatters`,
//! `list_linters`, `list_dap`). Payloads are external data: a root
//! that is not an object, or a field of the wrong type, is a typed
//! build error — never a panic and never a silently empty tree.
//! Node count, depth, and label length are all bounded; overflow is
//! shown as an explicit truncation marker.

use serde_json::Value;

use crate::facade::FacadeError;
use crate::{
    AttachState, LABEL_MAX_CHARS, LINE_MAX_CHARS, LogEvent, TREE_DEPTH_MAX, TREE_NODES_MAX,
    truncate_chars,
};

/// One node of the capability tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeNode {
    /// Display label, bounded to LABEL_MAX_CHARS.
    pub label: String,
    /// Child nodes (bounded by the tree-wide node budget).
    pub children: Vec<TreeNode>,
}

impl TreeNode {
    fn leaf(label: impl Into<String>) -> TreeNode {
        TreeNode {
            label: truncate_chars(&label.into(), LABEL_MAX_CHARS),
            children: Vec::new(),
        }
    }
}

/// The four-section capability tree for the attached editor.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CapabilityTree {
    /// Section roots: LSP, formatters, linters, DAP.
    pub sections: Vec<TreeNode>,
}

impl CapabilityTree {
    /// Total node count across all sections.
    pub fn node_count(&self) -> usize {
        fn count(node: &TreeNode) -> usize {
            1 + node.children.iter().map(count).sum::<usize>()
        }
        self.sections.iter().map(count).sum()
    }

    /// Flatten to (depth, label) rows for rendering, bounded to
    /// TREE_NODES_MAX rows; when the tree is larger, the last row is
    /// an explicit truncation marker (never a silent cut).
    pub fn rows(&self) -> Vec<(usize, String)> {
        let mut rows = Vec::new();
        for section in &self.sections {
            push_rows(section, 0, &mut rows);
        }
        if rows.len() > TREE_NODES_MAX {
            rows.truncate(TREE_NODES_MAX - 1);
            rows.push((0, "(tree truncated at the node bound)".to_string()));
        }
        rows
    }
}

fn push_rows(node: &TreeNode, depth: usize, rows: &mut Vec<(usize, String)>) {
    // The tree itself is node-bounded at build time (budget +
    // structural allowance), so flattening cannot grow unbounded.
    rows.push((depth, node.label.clone()));
    if depth >= TREE_DEPTH_MAX {
        return;
    }
    for child in &node.children {
        push_rows(child, depth + 1, rows);
    }
}

/// Mutable budget shared by the section builders.
struct TreeBudget {
    used: usize,
    truncated: bool,
}

impl TreeBudget {
    /// Charge one node; returns false when the budget is exhausted.
    fn charge(&mut self) -> bool {
        if self.used >= TREE_NODES_MAX {
            self.truncated = true;
            return false;
        }
        self.used += 1;
        true
    }
}

/// Build the capability tree from the four registry payloads.
/// Any payload whose root is not a JSON object, or whose known
/// fields carry the wrong type, fails the whole build with a
/// message naming the payload — partial trees are not rendered.
pub fn build_tree(
    servers: &Value,
    formatters: &Value,
    linters: &Value,
    dap: &Value,
) -> Result<CapabilityTree, String> {
    let mut budget = TreeBudget {
        used: 0,
        truncated: false,
    };
    let sections = vec![
        servers_section(servers, &mut budget)?,
        names_section(formatters, "formatters", "Formatters", &mut budget)?,
        linters_section(linters, &mut budget)?,
        dap_section(dap, &mut budget)?,
    ];
    let mut tree = CapabilityTree { sections };
    if budget.truncated {
        tree.sections
            .push(TreeNode::leaf("(tree truncated at the node bound)"));
    }
    Ok(tree)
}

fn object<'a>(
    payload: &'a Value,
    name: &str,
) -> Result<&'a serde_json::Map<String, Value>, String> {
    payload
        .as_object()
        .ok_or_else(|| format!("{name} payload is not a JSON object"))
}

/// Read an optional string-array field. Absent means empty; present
/// with a wrong type (or non-string entries) is a build error.
fn string_list(
    object: &serde_json::Map<String, Value>,
    key: &str,
    payload: &str,
) -> Result<Vec<String>, String> {
    let Some(value) = object.get(key) else {
        return Ok(Vec::new());
    };
    let entries = value
        .as_array()
        .ok_or_else(|| format!("{payload} field {key} is not an array"))?;
    let mut names = Vec::new();
    for entry in entries {
        let name = entry
            .as_str()
            .ok_or_else(|| format!("{payload} field {key} holds a non-string entry"))?;
        names.push(name.to_string());
    }
    Ok(names)
}

fn names_section(
    payload: &Value,
    key: &str,
    title: &str,
    budget: &mut TreeBudget,
) -> Result<TreeNode, String> {
    let object = object(payload, key)?;
    let names = string_list(object, key, key)?;
    let mut section = TreeNode::leaf(format!("{title} ({})", names.len()));
    budget.charge();
    for name in names {
        if !budget.charge() {
            break;
        }
        section.children.push(TreeNode::leaf(name));
    }
    Ok(section)
}

fn servers_section(payload: &Value, budget: &mut TreeBudget) -> Result<TreeNode, String> {
    let object = object(payload, "list_servers")?;
    let configured = string_list(object, "configured", "list_servers")?;
    let attached_raw = match object.get("attached") {
        None => Vec::new(),
        Some(value) => value
            .as_array()
            .ok_or_else(|| "list_servers field attached is not an array".to_string())?
            .clone(),
    };
    let mut section = TreeNode::leaf(format!(
        "LSP servers ({} configured, {} attached)",
        configured.len(),
        attached_raw.len()
    ));
    budget.charge();
    let mut configured_node = TreeNode::leaf("configured");
    budget.charge();
    for name in configured {
        if !budget.charge() {
            break;
        }
        configured_node.children.push(TreeNode::leaf(name));
    }
    section.children.push(configured_node);
    let mut attached_node = TreeNode::leaf("attached clients");
    budget.charge();
    for entry in &attached_raw {
        if !budget.charge() {
            break;
        }
        let name = entry
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| "list_servers attached entry has no name".to_string())?;
        let root = entry.get("root_dir").and_then(Value::as_str).unwrap_or("");
        let label = if root.is_empty() {
            name.to_string()
        } else {
            format!("{name} @ {root}")
        };
        attached_node.children.push(TreeNode::leaf(label));
    }
    section.children.push(attached_node);
    Ok(section)
}

fn linters_section(payload: &Value, budget: &mut TreeBudget) -> Result<TreeNode, String> {
    let object = object(payload, "list_linters")?;
    let linters = string_list(object, "linters", "list_linters")?;
    let internal = string_list(object, "internal", "list_linters")?;
    let mut section = TreeNode::leaf(format!(
        "Linters ({} + {} internal)",
        linters.len(),
        internal.len()
    ));
    budget.charge();
    for name in linters {
        if !budget.charge() {
            break;
        }
        section.children.push(TreeNode::leaf(name));
    }
    if !internal.is_empty() {
        let mut node = TreeNode::leaf("internal");
        budget.charge();
        for name in internal {
            if !budget.charge() {
                break;
            }
            node.children.push(TreeNode::leaf(name));
        }
        section.children.push(node);
    }
    Ok(section)
}

fn dap_section(payload: &Value, budget: &mut TreeBudget) -> Result<TreeNode, String> {
    let object = object(payload, "list_dap")?;
    let modules = string_list(object, "modules", "list_dap")?;
    let adapters = string_list(object, "adapters_registered", "list_dap")?;
    let loaded = object
        .get("dap_loaded")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut section = TreeNode::leaf(format!(
        "DAP ({} modules, {} adapters, loaded: {loaded})",
        modules.len(),
        adapters.len()
    ));
    budget.charge();
    let mut modules_node = TreeNode::leaf("modules");
    budget.charge();
    for name in modules {
        if !budget.charge() {
            break;
        }
        modules_node.children.push(TreeNode::leaf(name));
    }
    section.children.push(modules_node);
    let mut adapters_node = TreeNode::leaf("adapters registered");
    budget.charge();
    for name in adapters {
        if !budget.charge() {
            break;
        }
        adapters_node.children.push(TreeNode::leaf(name));
    }
    section.children.push(adapters_node);
    let configurations = match object.get("configurations") {
        None => Vec::new(),
        Some(value) => value
            .as_array()
            .ok_or_else(|| "list_dap field configurations is not an array".to_string())?
            .clone(),
    };
    let mut configs_node = TreeNode::leaf("launch configurations");
    budget.charge();
    for entry in &configurations {
        if !budget.charge() {
            break;
        }
        let filetype = entry
            .get("filetype")
            .and_then(Value::as_str)
            .ok_or_else(|| "list_dap configuration entry has no filetype".to_string())?;
        let names: Vec<Value> = match entry.get("configurations") {
            None => Vec::new(),
            Some(value) => value
                .as_array()
                .ok_or_else(|| "list_dap configurations entry is not an array".to_string())?
                .clone(),
        };
        let mut node = TreeNode::leaf(filetype);
        budget.charge();
        for name in names {
            if !budget.charge() {
                break;
            }
            let label = name.as_str().unwrap_or("?");
            node.children.push(TreeNode::leaf(label));
        }
        configs_node.children.push(node);
    }
    section.children.push(configs_node);
    Ok(section)
}

/// Render the tree as indented text lines, or the honest placeholder
/// for the state the app is actually in.
pub fn tree_lines(tree: Option<&CapabilityTree>, attach: AttachState) -> Vec<String> {
    match (tree, attach) {
        (Some(tree), AttachState::Attached) => {
            let mut lines = vec![format!("capability tree: {} nodes", tree.node_count())];
            for (depth, label) in tree.rows() {
                let indent = "  ".repeat(depth.min(TREE_DEPTH_MAX));
                lines.push(truncate_chars(&format!("{indent}{label}"), LINE_MAX_CHARS));
            }
            lines
        }
        (_, AttachState::Detached) => vec![
            "capability tree unavailable: no editor is attached".to_string(),
            "the LSP / formatter / linter / DAP tree exists only inside a live editor".to_string(),
        ],
        (_, AttachState::Unknown) => vec![
            "capability tree unknown: editor state cannot be claimed".to_string(),
            "refresh (r) to query the broker again; no cached tree is shown".to_string(),
        ],
        (None, AttachState::Attached) => {
            vec!["capability tree not loaded yet: press r on the Tree screen".to_string()]
        }
    }
}

/// Overview lines: broker status and attached-editor identity when
/// the server reports them; the detached/unknown truth otherwise.
pub fn overview_lines(
    status: Option<&Value>,
    attach: AttachState,
    tool_count: Option<usize>,
    rule_count: usize,
    last_event: Option<&LogEvent>,
) -> Vec<String> {
    let mut lines = vec![
        "periscope — Neovim broker console".to_string(),
        format!("server: periscope {}", env!("CARGO_PKG_VERSION")),
        format!("editor state: {}", attach.label()),
    ];
    match (status, attach) {
        (Some(status), AttachState::Attached) => {
            let field = |key: &str| {
                status
                    .get(key)
                    .and_then(Value::as_str)
                    .unwrap_or("(not reported)")
            };
            lines.push(format!("broker mode: {}", field("mode")));
            lines.push(format!("editor: Neovim {}", field("nvim_version")));
            lines.push(format!("config root: {}", field("config_root")));
            lines.push(format!("editor cwd: {}", field("cwd")));
            lines.push(format!("workspace: {}", field("workspace")));
        }
        (_, AttachState::Detached) => {
            lines.push("no editor is attached to the broker".to_string());
            lines.push(
                "start Neovim with --listen, or let periscope manage one, then refresh (r)"
                    .to_string(),
            );
        }
        (_, AttachState::Unknown) => {
            lines.push("editor state is unknown; no cached status is shown".to_string());
            lines.push("refresh (r) to query the broker again".to_string());
        }
        (None, AttachState::Attached) => {
            lines.push("status not loaded yet: refresh (r)".to_string());
        }
    }
    match tool_count {
        Some(count) => lines.push(format!("server tools: {count}")),
        None => lines.push("server tools: (not loaded)".to_string()),
    }
    lines.push(format!("trigger rules declared: {rule_count}"));
    match last_event {
        Some(event) => lines.push(format!("last event: {} {}", event.kind.label(), event.text)),
        None => lines.push("last event: (none)".to_string()),
    }
    lines
        .iter()
        .map(|line| truncate_chars(line, LINE_MAX_CHARS))
        .collect()
}

/// One-line summary of a workflow result for the log: the result's
/// top-level scalar fields, sorted by key for determinism, bounded
/// to RESULT summary width.
pub fn result_summary(result: &Value) -> String {
    let Some(object) = result.as_object() else {
        return truncate_chars(&result.to_string(), LINE_MAX_CHARS);
    };
    let mut keys: Vec<&String> = object.keys().collect();
    keys.sort();
    let mut parts = Vec::new();
    for key in keys {
        let value = &object[key];
        let rendered = match value {
            Value::String(text) => truncate_chars(text, 40),
            Value::Number(number) => number.to_string(),
            Value::Bool(flag) => flag.to_string(),
            Value::Array(items) => format!("[{} items]", items.len()),
            Value::Object(map) => format!("{{{} fields}}", map.len()),
            Value::Null => "null".to_string(),
        };
        parts.push(format!("{key}={rendered}"));
    }
    truncate_chars(&parts.join(" "), LINE_MAX_CHARS)
}

/// Render a facade failure as the single honest line screens show.
pub fn error_line(error: &FacadeError) -> String {
    truncate_chars(&error.to_string(), LINE_MAX_CHARS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::{sample_dap, sample_formatters, sample_linters, sample_servers};
    use serde_json::json;

    fn sample_tree() -> CapabilityTree {
        build_tree(
            &sample_servers(),
            &sample_formatters(),
            &sample_linters(),
            &sample_dap(),
        )
        .expect("sample payloads build")
    }

    #[test]
    fn tree_builds_four_sections_with_counts() {
        let tree = sample_tree();
        assert_eq!(tree.sections.len(), 4);
        assert_eq!(
            tree.sections[0].label,
            "LSP servers (3 configured, 2 attached)"
        );
        assert_eq!(tree.sections[1].label, "Formatters (2)");
        assert!(tree.node_count() > 10);
    }

    #[test]
    fn tree_rejects_non_object_root() {
        let bad = json!(["not", "an", "object"]);
        let result = build_tree(&bad, &sample_formatters(), &sample_linters(), &sample_dap());
        assert_eq!(
            result.err(),
            Some("list_servers payload is not a JSON object".to_string())
        );
    }

    #[test]
    fn tree_rejects_wrong_field_type() {
        let bad = json!({"configured": "rust_analyzer", "attached": []});
        let result = build_tree(&bad, &sample_formatters(), &sample_linters(), &sample_dap());
        assert!(result.is_err());
    }

    #[test]
    fn tree_rejects_non_string_entries() {
        let bad = json!({"configured": [42], "attached": []});
        let result = build_tree(&bad, &sample_formatters(), &sample_linters(), &sample_dap());
        assert!(result.is_err());
    }

    #[test]
    fn tree_bounds_oversized_payloads() {
        let names: Vec<Value> = (0..10_000)
            .map(|index| json!(format!("server_{index}")))
            .collect();
        let servers = json!({"configured": names, "attached": []});
        let tree = build_tree(
            &servers,
            &sample_formatters(),
            &sample_linters(),
            &sample_dap(),
        )
        .expect("oversized payload still builds, bounded");
        // Bound: the charged data budget plus the named structural
        // allowance (section/group nodes and the truncation marker).
        assert!(
            tree.node_count() <= TREE_NODES_MAX + crate::TREE_STRUCTURAL_MAX,
            "actual node count {}",
            tree.node_count()
        );
        let lines = tree_lines(Some(&tree), AttachState::Attached);
        assert!(
            lines.iter().any(|line| line.contains("truncated")),
            "truncation must be visible: {lines:?}"
        );
    }

    #[test]
    fn tree_lines_detached_are_honest() {
        let lines = tree_lines(None, AttachState::Detached);
        assert!(lines[0].contains("no editor is attached"));
    }

    #[test]
    fn overview_detached_never_claims_an_editor() {
        let lines = overview_lines(None, AttachState::Detached, None, 0, None);
        let joined = lines.join("\n");
        assert!(joined.contains("DETACHED"));
        assert!(joined.contains("no editor is attached"));
        assert!(!joined.contains("Neovim 0."));
    }

    #[test]
    fn overview_attached_shows_reported_identity() {
        let status = crate::facade::sample_status();
        let lines = overview_lines(Some(&status), AttachState::Attached, Some(16), 0, None);
        let joined = lines.join("\n");
        assert!(joined.contains("Neovim 0.11.4"));
        assert!(joined.contains("broker mode: attached"));
        assert!(joined.contains("server tools: 16"));
    }

    #[test]
    fn overview_unknown_hides_cached_status() {
        let status = crate::facade::sample_status();
        let lines = overview_lines(Some(&status), AttachState::Unknown, None, 0, None);
        let joined = lines.join("\n");
        assert!(joined.contains("UNKNOWN"));
        assert!(!joined.contains("0.11.4"), "stale version must not render");
    }

    #[test]
    fn result_summary_is_sorted_and_bounded() {
        let result = json!({"z": 1, "a": "x".repeat(500)});
        let summary = result_summary(&result);
        assert!(summary.starts_with("a="));
        assert!(summary.chars().count() <= LINE_MAX_CHARS);
    }
}
