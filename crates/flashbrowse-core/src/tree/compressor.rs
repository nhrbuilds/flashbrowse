//! Recursive pruning and token minimization engine.
//!
//! Input: Chromium `Accessibility.getFullAXTree` output (`Vec<AXNode>`).
//! Output: flat `Vec<SemanticNode>` + `ActionMap` (ID -> `BackendNodeId`).
//!
//! Pruning rules:
//! - Drop `ignored == true` nodes.
//! - Drop `InlineTextBox` (pure layout noise, always).
//! - Drop `generic`/`none`/`presentation`/`paragraph` shells with no text.
//! - Drop `StaticText` whose text is already carried by its parent
//!   (heading/button/link text is duplicated in the AX tree).
//! - Emit one line per table row (`a | b | c`) instead of one line per
//!   cell: fewer tokens, and the row/column relationship survives.
//! - Assign `[1..]` IDs depth-first to actionable roles that carry a
//!   `backendDOMNodeId` (without one they cannot be clicked).
//!
//! Accuracy rule: **no visible text may be dropped**. Every pruning
//! decision below removes a *duplicate*, never the only copy of the
//! content.

mod full;
mod pruned;

use full::compress_full;
use pruned::compress_pruned;

#[cfg(test)]
#[path = "compressor/tests.rs"]
mod tests;

use std::collections::{HashMap, HashSet};

use flashbrowse_cdp::protocol::accessibility::{AXNode, AXValue};

use super::node::{ActionMap, SemanticNode};

/// Roles that get a sequential action ID when they carry a backend node.
const ACTIONABLE_ROLES: &[&str] = &[
    "button",
    "link",
    "menuitem",
    "menuitemcheckbox",
    "menuitemradio",
    "tab",
    "checkbox",
    "radio",
    "switch",
    "textbox",
    "searchbox",
    "combobox",
    "spinbutton",
    "slider",
];

/// Structural roles dropped unless they bear text.
const STRUCTURAL_ROLES: &[&str] = &[
    "generic",
    "none",
    "presentation",
    "paragraph",
    "rootwebarea",
    "genericcontainer",
];

/// Content roles kept without an ID (their text is the payload).
const CONTENT_ROLES: &[&str] = &[
    "heading",
    "table",
    "row",
    "cell",
    "gridcell",
    "list",
    "listitem",
    "article",
    "main",
    "navigation",
    "banner",
    "contentinfo",
];

/// Landmark roles: their accessible name duplicates their subtree.
const LANDMARK_ROLES: &[&str] = &[
    "main",
    "banner",
    "navigation",
    "contentinfo",
    "complementary",
    "region",
    "article",
    "section",
    "form",
];

/// Structural roles: emitted neither as label nor name.
/// `row`/`cell` are handled separately (rows become one line each).
const STRUCTURE_ONLY_ROLES: &[&str] = &["list", "table"];

/// Chromium internal layout containers (table scaffolding).
const LAYOUT_CONTAINER_ROLES: &[&str] = &[
    "layouttable",
    "layouttablerow",
    "layouttablecell",
    "genericcontainer",
    "row",
];

/// Roles whose accessible name is rendered by a child `StaticText` node,
/// so the child's text must be suppressed to avoid double-emitting it.
const NAME_FROM_CHILDREN_ROLES: &[&str] = &[
    "heading",
    "button",
    "link",
    "menuitem",
    "menuitemcheckbox",
    "menuitemradio",
    "tab",
    "checkbox",
    "radio",
    "switch",
    "cell",
    "gridcell",
    "columnheader",
    "rowheader",
    "listitem",
    "paragraph",
    "textbox",
    "searchbox",
    "combobox",
    "spinbutton",
    "slider",
    "option",
];

/// Roles whose accessible name may be empty while their visible text lives
/// in descendant nodes. Only these derive a name from their subtree:
/// deriving one for a container would repeat every descendant and blow up
/// the token count (and duplicate text that is emitted on its own).
fn derives_name_from_children(role: &str) -> bool {
    ACTIONABLE_ROLES.contains(&role)
        || matches!(
            role,
            "paragraph"
                | "listitem"
                | "cell"
                | "gridcell"
                | "columnheader"
                | "rowheader"
                | "heading"
                | "option"
                | "caption"
                | "legend"
        )
}

/// Roles that make up a table row.
const ROW_CELL_ROLES: &[&str] = &["cell", "gridcell", "columnheader", "rowheader"];

/// Extract the string payload of an `AXValue` (`{type, value}`).
fn ax_string(opt: Option<&AXValue>) -> String {
    opt.map(|v| v.as_text()).unwrap_or_default()
}

/// Role string from `AXValue` (`role.value` is a JSON string).
fn ax_role(node: &AXNode) -> String {
    ax_string(node.role.as_ref()).trim().to_ascii_lowercase()
}

/// Accessible name, trimmed.
fn ax_name(node: &AXNode) -> String {
    ax_string(node.name.as_ref()).trim().to_string()
}

/// Heading level from `properties` (`level`), default 1.
fn ax_level(node: &AXNode) -> Option<u32> {
    if ax_role(node) != "heading" {
        return None;
    }
    Some(node.prop_i64("level").unwrap_or(1).clamp(1, 6) as u32)
}

/// Build sorted `extras` (disabled, checked, expanded, selected, value).
fn ax_extras(node: &AXNode, role: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if node.prop_bool("disabled") {
        out.push("disabled".to_string());
    }
    if node.prop_bool("checked") {
        out.push("checked".to_string());
    }
    if node.prop_bool("expanded") {
        out.push("expanded".to_string());
    }
    if node.prop_bool("readonly") {
        out.push("readonly".to_string());
    }
    if node.prop_bool("required") {
        out.push("required".to_string());
    }
    if node.prop_bool("selected") {
        out.push("selected".to_string());
    }
    // Node value: input value, selected option text, combobox value, slider
    // position. Geometry is not available, so a slider's value is the only
    // way an agent can reason about where it sits.
    if let Some(v) = node.value.as_ref() {
        let t = v.as_text();
        let t = t.trim();
        if !t.is_empty()
            && matches!(
                role,
                "textbox" | "searchbox" | "combobox" | "slider" | "spinbutton"
            )
        {
            out.push(format!("value={t:?}"));
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Compress `nodes` into semantic output lines + action map.
///
/// Chromium emits the AX tree depth-first, so declaration order already
/// preserves document order.
/// Whether an ancestor (within `depth` levels) has a name containing `text`.
fn ancestor_contains(
    parent_of: &HashMap<&str, &str>,
    index: &HashMap<&str, &AXNode>,
    role_of: &HashMap<&str, String>,
    start: &str,
    text: &str,
    depth: usize,
) -> bool {
    if text.len() < 3 {
        return false;
    }
    let mut cur = start;
    for _ in 0..depth {
        let Some(parent) = parent_of.get(cur) else {
            return false;
        };
        let Some(node) = index.get(parent) else {
            return false;
        };
        let prole = role_of.get(parent).map(String::as_str).unwrap_or("");
        let pname = ax_name(node);
        if !pname.is_empty() && pname.contains(text) && ancestor_name_is_rendered(prole) {
            return true;
        }
        cur = parent;
    }
    false
}

/// True when a node with this role renders its own accessible name, so a
/// child's identical text is a real duplicate.
///
/// Layout containers, landmarks and structure-only roles do not: their name is
/// a concatenation of their subtree that is dropped, so treating it as a
/// duplicate would lose the only copy of the text. This is the Hacker News
/// footer case: a `LayoutTableCell` named `Guidelines | Search:` owns the
/// "Search:" label, and the cell itself is never emitted.
fn ancestor_name_is_rendered(role: &str) -> bool {
    !LAYOUT_CONTAINER_ROLES.contains(&role)
        && !LANDMARK_ROLES.contains(&role)
        && !STRUCTURE_ONLY_ROLES.contains(&role)
        && !matches!(role, "rowgroup" | "none" | "presentation" | "rootwebarea")
}

/// True when an ancestor within `depth` has a node value equal to `text`.
///
/// Chromium exposes an input's value both as `AXNode.value` and as a
/// `StaticText` child, so the text would be emitted twice (`value="Ada"` and
/// `Text: Ada`).
fn ancestor_value_matches(
    parent_of: &HashMap<&str, &str>,
    index: &HashMap<&str, &AXNode>,
    start: &str,
    text: &str,
    depth: usize,
) -> bool {
    if text.is_empty() {
        return false;
    }
    let mut cur = start;
    for _ in 0..depth {
        let Some(parent) = parent_of.get(cur) else {
            return false;
        };
        let Some(node) = index.get(parent) else {
            return false;
        };
        if let Some(value) = node.value.as_ref() {
            if value.as_text().trim() == text {
                return true;
            }
        }
        cur = parent;
    }
    false
}

fn is_clickable_non_aria_role(role: &str) -> bool {
    matches!(
        role,
        "generic"
            | "genericcontainer"
            | "none"
            | "presentation"
            | "group"
            | "section"
            | "paragraph"
            | "image"
    )
}

/// Collect `derive_name` parts and the `StaticText` node IDs they came from.
fn derive_name_with_sources<'a>(
    node_id: &str,
    children_of: &HashMap<&str, Vec<&'a str>>,
    index: &HashMap<&str, &'a AXNode>,
) -> (String, Vec<&'a str>) {
    let mut parts: Vec<String> = Vec::with_capacity(4);
    let mut sources: Vec<&'a str> = Vec::with_capacity(4);
    let mut stack: Vec<&'a str> = children_of
        .get(node_id)
        .map(|v| v.to_vec())
        .unwrap_or_default();
    stack.reverse();
    let mut visited: usize = 0;
    while let Some(id) = stack.pop() {
        visited = visited.saturating_add(1);
        if visited > 64 {
            break;
        }
        let Some(child) = index.get(id) else { continue };
        if child.ignored {
            continue;
        }
        let role = ax_role(child);
        if ACTIONABLE_ROLES.contains(&role.as_str()) {
            continue;
        }
        if role == "statictext" {
            let text = ax_name(child);
            if !text.is_empty() {
                parts.push(text);
                sources.push(id);
                if parts.len() >= 6 {
                    break;
                }
            }
        } else if role != "inlinetextbox" {
            if let Some(kids) = children_of.get(id) {
                for k in kids.iter().rev() {
                    stack.push(k);
                }
            }
        }
    }
    (parts.join(" ").trim().to_string(), sources)
}

/// Derive a node's text from its descendants (Chromium leaves `name` empty
/// when the text is owned by child nodes).
///
/// Descendants that are themselves actionable (a link inside a list item,
/// a button inside a table cell) are SKIPPED: they are emitted on their
/// own with an action ID, so including their text here would duplicate it.
fn derive_name(
    node_id: &str,
    children_of: &HashMap<&str, Vec<&str>>,
    index: &HashMap<&str, &AXNode>,
) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(4);
    let mut stack: Vec<&str> = children_of
        .get(node_id)
        .map(|v| v.to_vec())
        .unwrap_or_default();
    stack.reverse();
    let mut visited: usize = 0;
    while let Some(id) = stack.pop() {
        visited = visited.saturating_add(1);
        if visited > 64 {
            break;
        }
        let Some(child) = index.get(id) else { continue };
        let role = ax_role(child);
        // An actionable descendant renders itself with an action ID.
        if ACTIONABLE_ROLES.contains(&role.as_str()) {
            continue;
        }
        if role == "statictext" {
            let text = ax_name(child);
            if !text.is_empty() {
                parts.push(text);
                if parts.len() >= 6 {
                    break;
                }
            }
        } else if role != "inlinetextbox" {
            if let Some(kids) = children_of.get(id) {
                for k in kids.iter().rev() {
                    stack.push(k);
                }
            }
        }
    }
    parts.join(" ").trim().to_string()
}

/// Text of a table cell: its accessible name, or the text it owns.
fn cell_text(
    cell_id: &str,
    index: &HashMap<&str, &AXNode>,
    children_of: &HashMap<&str, Vec<&str>>,
) -> String {
    match index.get(cell_id) {
        Some(node) => {
            let own = ax_name(node);
            if own.is_empty() {
                derive_name(cell_id, children_of, index)
            } else {
                own
            }
        }
        None => String::new(),
    }
}

/// Text of a table cell, with actionable content rendered as `[N]`.
///
/// A table cell usually holds exactly one link. Rendering the link's text
/// inside the row AND as its own numbered line duplicates the text (the
/// single biggest token cost on table-heavy pages). Replacing it with the
/// action ID keeps both views: the row shows the structure, the numbered
/// line shows the label.
///
/// Returns `None` when the cell holds no actionable content (the plain
/// text is then used as-is).
fn cell_text_with_refs(
    cell_id: &str,
    role_of: &HashMap<&str, String>,
    index: &HashMap<&str, &AXNode>,
    children_of: &HashMap<&str, Vec<&str>>,
    id_of_node: &HashMap<&str, u32>,
) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut refs = 0usize;
    collect_refs(
        cell_id,
        role_of,
        index,
        children_of,
        id_of_node,
        &mut parts,
        &mut refs,
        0,
    );
    if refs == 0 {
        return None;
    }
    Some(parts.join(" ").trim().to_string())
}

/// Walk a cell's descendants, collecting text and `[N]` action references.
#[allow(clippy::too_many_arguments)]
fn collect_refs(
    node_id: &str,
    role_of: &HashMap<&str, String>,
    index: &HashMap<&str, &AXNode>,
    children_of: &HashMap<&str, Vec<&str>>,
    id_of_node: &HashMap<&str, u32>,
    parts: &mut Vec<String>,
    refs: &mut usize,
    depth: usize,
) {
    if depth > 8 {
        return;
    }
    let Some(kids) = children_of.get(node_id) else {
        return;
    };
    for kid in kids {
        let role = role_of.get(*kid).map(String::as_str).unwrap_or("");
        if role == "inlinetextbox" {
            continue;
        }
        if ACTIONABLE_ROLES.contains(&role) {
            if let Some(id) = id_of_node.get(*kid) {
                parts.push(format!("[{id}]"));
                *refs = refs.saturating_add(1);
            }
            // Do not descend: the link's own label is on its numbered line.
            continue;
        }
        if role == "statictext" {
            let text = index.get(*kid).map(|n| ax_name(n)).unwrap_or_default();
            if !text.is_empty() {
                parts.push(text);
            }
            continue;
        }
        collect_refs(
            kid,
            role_of,
            index,
            children_of,
            id_of_node,
            parts,
            refs,
            depth.saturating_add(1),
        );
    }
}

/// Render a table row as one line: `a | b | c`.
///
/// One line per row (instead of one per cell) is both cheaper and closer
/// to how a human reads a table. `None` when the row has no cells.
fn row_text(
    node: &AXNode,
    role_of: &HashMap<&str, String>,
    index: &HashMap<&str, &AXNode>,
    children_of: &HashMap<&str, Vec<&str>>,
    id_of_node: &HashMap<&str, u32>,
) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut any_cell = false;
    for cid in &node.child_ids {
        let role = role_of.get(cid.as_str()).map(String::as_str).unwrap_or("");
        if !ROW_CELL_ROLES.contains(&role) {
            continue;
        }
        any_cell = true;
        // Prefer the `[N]` form when the cell holds an action; fall back to
        // the plain text otherwise.
        let text = cell_text_with_refs(cid.as_str(), role_of, index, children_of, id_of_node)
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| cell_text(cid.as_str(), index, children_of));
        parts.push(text);
    }
    if !any_cell {
        return None;
    }
    Some(parts.join(" | "))
}

/// True when a node has a child that carries content (not layout noise).
///
/// `InlineTextBox` is pure layout scaffolding: Chromium gives EVERY
/// `StaticText` one, so counting it as "has children" would drop all text.
fn has_meaningful_child(node: &AXNode, role_of: &HashMap<&str, String>) -> bool {
    node.child_ids.iter().any(|cid| {
        let role = role_of.get(cid.as_str()).map(String::as_str).unwrap_or("");
        role != "inlinetextbox" && role != "none"
    })
}

/// Reorder nodes into document order (depth-first).
///
/// Chromium's `Accessibility.getFullAXTree` returns nodes **breadth-first**,
/// so the raw array interleaves siblings with their descendants: a table's
/// header row can arrive after its body rows. An agent reading that output
/// sees a scrambled page.
///
/// This rebuilds the tree from the parent/child links and walks it depth
/// first, which is the order the page is actually laid out. Nodes that are
/// unreachable from a root (orphans) are appended in their original order.
fn document_order<'a>(
    nodes: &'a [AXNode],
    children_of: &HashMap<&str, Vec<&'a str>>,
    parent_of: &HashMap<&str, &str>,
) -> Vec<&'a AXNode> {
    let index: HashMap<&str, &AXNode> = nodes.iter().map(|n| (n.node_id.as_str(), n)).collect();
    let mut visited: HashSet<&str> = HashSet::with_capacity(nodes.len());
    let mut ordered: Vec<&AXNode> = Vec::with_capacity(nodes.len());

    // Roots: nodes that are nobody's child.
    let mut stack: Vec<&str> = nodes
        .iter()
        .filter(|n| !parent_of.contains_key(n.node_id.as_str()))
        .map(|n| n.node_id.as_str())
        .collect();
    // Reverse so the first root is processed first (LIFO stack).
    stack.reverse();

    while let Some(id) = stack.pop() {
        if !visited.insert(id) {
            continue;
        }
        let Some(node) = index.get(id) else { continue };
        ordered.push(node);
        if let Some(kids) = children_of.get(id) {
            for k in kids.iter().rev() {
                if !visited.contains(*k) {
                    stack.push(k);
                }
            }
        }
    }

    // Orphans (parent link points at a node we never saw): keep them, in
    // their original order, so no content is lost.
    for n in nodes {
        if visited.insert(n.node_id.as_str()) {
            ordered.push(n);
        }
    }
    ordered
}

/// Compression mode for eval arms (default pruning vs full AX tree).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompressOptions {
    /// When `false`, emit every non-ignored node with no dedupe or layout folding.
    pub prune: bool,
}

impl Default for CompressOptions {
    fn default() -> Self {
        Self { prune: true }
    }
}

/// Memoize depths across parent walks. A path longer than the number of
/// parent links revisits a node, so the input cannot be a forest.
fn forest_depths<'a>(
    nodes: &'a [AXNode],
    parent_of: &HashMap<&'a str, &'a str>,
) -> Option<HashMap<&'a str, usize>> {
    let mut depths: HashMap<&str, usize> = HashMap::with_capacity(nodes.len());
    let mut path = Vec::new();
    for node in nodes {
        let mut current = node.node_id.as_str();
        let mut depth = loop {
            if let Some(depth) = depths.get(current) {
                break *depth;
            }
            let Some(&parent) = parent_of.get(current) else {
                depths.insert(current, 0);
                break 0;
            };
            if path.len() >= parent_of.len() {
                return None;
            }
            path.push(current);
            current = parent;
        };
        while let Some(id) = path.pop() {
            depth = depth.checked_add(1)?;
            depths.insert(id, depth);
        }
    }
    Some(depths)
}

/// Preserve the original text when invalid links make AX ownership ambiguous.
fn render_unlinked_nodes(nodes: &[AXNode]) -> (Vec<SemanticNode>, ActionMap) {
    tracing::warn!("invalid accessibility tree links; preserving nodes in input order");
    let semantic = nodes
        .iter()
        .filter(|node| !node.ignored)
        .map(|node| {
            let role = ax_role(node);
            SemanticNode {
                action_id: None,
                name: ax_name(node),
                extras: ax_extras(node, &role),
                level: ax_level(node),
                node_id: node.node_id.clone(),
                role,
                depth: 0,
            }
        })
        .collect();
    (semantic, HashMap::new())
}

pub fn compress(nodes: &[AXNode]) -> (Vec<SemanticNode>, ActionMap) {
    compress_with_options(nodes, CompressOptions::default())
}

pub fn compress_with_options(
    nodes: &[AXNode],
    opts: CompressOptions,
) -> (Vec<SemanticNode>, ActionMap) {
    if !opts.prune {
        return compress_full(nodes);
    }
    compress_pruned(nodes)
}
