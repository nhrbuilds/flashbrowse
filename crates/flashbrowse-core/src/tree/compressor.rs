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

/// Full tree: every non-ignored node, document order, standard action IDs.
fn compress_full(nodes: &[AXNode]) -> (Vec<SemanticNode>, ActionMap) {
    let mut out: Vec<SemanticNode> = Vec::with_capacity(nodes.len());
    let mut map: ActionMap = HashMap::new();
    let mut next_id = Some(1u32);

    let mut parent_of: HashMap<&str, &str> = HashMap::new();
    let mut role_of: HashMap<&str, String> = HashMap::with_capacity(nodes.len());
    let mut children_of: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut index: HashMap<&str, &AXNode> = HashMap::with_capacity(nodes.len());
    for n in nodes {
        role_of.insert(n.node_id.as_str(), ax_role(n));
        if index.insert(n.node_id.as_str(), n).is_some() {
            return render_unlinked_nodes(nodes);
        }
        for child in &n.child_ids {
            parent_of.insert(child.as_str(), n.node_id.as_str());
            children_of
                .entry(n.node_id.as_str())
                .or_default()
                .push(child.as_str());
        }
    }

    let Some(depths) = forest_depths(nodes, &parent_of) else {
        return render_unlinked_nodes(nodes);
    };
    let ordered = document_order(nodes, &children_of, &parent_of);

    let mut id_of_node: HashMap<&str, u32> = HashMap::new();
    for node in &ordered {
        if node.ignored {
            continue;
        }
        let role = role_of
            .get(node.node_id.as_str())
            .map(String::as_str)
            .unwrap_or("");
        if !ACTIONABLE_ROLES.contains(&role) {
            continue;
        }
        let Some(backend_node_id) = node.backend_node_id else {
            continue;
        };
        let Some(id) = next_id else { break };
        next_id = id.checked_add(1);
        map.insert(id, backend_node_id);
        id_of_node.insert(node.node_id.as_str(), id);
    }

    for node in &ordered {
        if node.ignored {
            continue;
        }
        let role = role_of
            .get(node.node_id.as_str())
            .map(String::as_str)
            .unwrap_or("");
        let mut name = ax_name(node);
        if name.is_empty() && derives_name_from_children(role) {
            name = derive_name(node.node_id.as_str(), &children_of, &index);
        }
        let depth = depths
            .get(node.node_id.as_str())
            .copied()
            .unwrap_or_default();
        let extras = ax_extras(node, role);
        if ACTIONABLE_ROLES.contains(&role) {
            if let Some(&id) = id_of_node.get(node.node_id.as_str()) {
                if name.is_empty() {
                    name = "(unlabeled)".to_string();
                }
                out.push(SemanticNode {
                    action_id: Some(id),
                    role: role.to_string(),
                    name,
                    extras,
                    level: ax_level(node),
                    node_id: node.node_id.clone(),
                    depth,
                });
                continue;
            }
        }
        out.push(SemanticNode {
            action_id: None,
            level: ax_level(node),
            role: role.to_string(),
            name,
            extras,
            node_id: node.node_id.clone(),
            depth,
        });
    }

    (out, map)
}

fn compress_pruned(nodes: &[AXNode]) -> (Vec<SemanticNode>, ActionMap) {
    let mut out: Vec<SemanticNode> = Vec::with_capacity(nodes.len() / 2);
    let mut map: ActionMap = HashMap::new();
    let mut next_id = Some(1u32);

    // Indexes (single pass, no per-node re-computation in the hot loop).
    let mut parent_of: HashMap<&str, &str> = HashMap::new();
    let mut role_of: HashMap<&str, String> = HashMap::with_capacity(nodes.len());
    let mut children_of: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut index: HashMap<&str, &AXNode> = HashMap::with_capacity(nodes.len());
    for n in nodes {
        role_of.insert(n.node_id.as_str(), ax_role(n));
        if index.insert(n.node_id.as_str(), n).is_some() {
            return render_unlinked_nodes(nodes);
        }
        for child in &n.child_ids {
            parent_of.insert(child.as_str(), n.node_id.as_str());
            children_of
                .entry(n.node_id.as_str())
                .or_default()
                .push(child.as_str());
        }
    }
    // Parent roles that render their own name (so child StaticText is a dupe).
    let mut name_bearing_parent: HashMap<&str, bool> = HashMap::with_capacity(nodes.len());
    for n in nodes {
        let role = role_of
            .get(n.node_id.as_str())
            .map(String::as_str)
            .unwrap_or("");
        name_bearing_parent.insert(
            n.node_id.as_str(),
            NAME_FROM_CHILDREN_ROLES.contains(&role) || role == "statictext",
        );
    }

    let mut seen: HashSet<&str> = HashSet::with_capacity(nodes.len());

    // Chromium returns the AX tree breadth-first; restore document order so
    // the agent reads the page the way it is laid out.
    let Some(depths) = forest_depths(nodes, &parent_of) else {
        return render_unlinked_nodes(nodes);
    };
    let ordered = document_order(nodes, &children_of, &parent_of);

    // Bottom-up pass: identify visible non-ARIA elements with explicit click
    // handlers (`clickable: true`), avoiding wrappers around already-actionable
    // descendants and recording which StaticText nodes they absorb.
    let mut clickable_non_aria: HashMap<&str, String> = HashMap::new();
    let mut suppressed_by_clickable: HashSet<&str> = HashSet::new();
    {
        let mut subtree_has_actionable: HashSet<&str> = HashSet::new();
        for node in ordered.iter().rev() {
            let id = node.node_id.as_str();
            let role = role_of.get(id).map(String::as_str).unwrap_or("");
            let self_aria_actionable =
                !node.ignored && ACTIONABLE_ROLES.contains(&role) && node.backend_node_id.is_some();
            let child_has_actionable = children_of
                .get(id)
                .is_some_and(|kids| kids.iter().any(|k| subtree_has_actionable.contains(*k)));
            if !child_has_actionable
                && !self_aria_actionable
                && !node.ignored
                && node.backend_node_id.is_some()
                && is_clickable_non_aria_role(role)
                && node.prop_bool("clickable")
            {
                let own = ax_name(node);
                if !own.is_empty() {
                    let (_, sources) = derive_name_with_sources(id, &children_of, &index);
                    for src in sources {
                        if let Some(src_node) = index.get(src) {
                            let src_text = ax_name(src_node);
                            if !src_text.is_empty() && own.contains(&src_text) {
                                suppressed_by_clickable.insert(src);
                            }
                        }
                    }
                    clickable_non_aria.insert(id, own);
                    subtree_has_actionable.insert(id);
                } else {
                    let (derived, sources) = derive_name_with_sources(id, &children_of, &index);
                    if !derived.is_empty() {
                        for src in sources {
                            suppressed_by_clickable.insert(src);
                        }
                        clickable_non_aria.insert(id, derived);
                        subtree_has_actionable.insert(id);
                    }
                }
            } else if self_aria_actionable || child_has_actionable {
                subtree_has_actionable.insert(id);
            }
        }
    }

    // ---- Pass 1: assign action IDs -------------------------------------
    //
    // The IDs must be known before the tree is rendered, because a table
    // row renders an actionable cell as `[N]` (referencing the link's own
    // numbered line) instead of repeating its text.
    let mut id_of_node: HashMap<&str, u32> = HashMap::new();
    {
        let mut pass_seen: HashSet<&str> = HashSet::with_capacity(nodes.len());
        for node in &ordered {
            if node.ignored {
                continue;
            }
            if !pass_seen.insert(node.node_id.as_str()) {
                continue;
            }
            let role = role_of
                .get(node.node_id.as_str())
                .map(String::as_str)
                .unwrap_or("");
            if !ACTIONABLE_ROLES.contains(&role)
                && !clickable_non_aria.contains_key(node.node_id.as_str())
            {
                continue;
            }
            let Some(backend_node_id) = node.backend_node_id else {
                continue;
            };
            let Some(id) = next_id else { break };
            next_id = id.checked_add(1);
            map.insert(id, backend_node_id);
            id_of_node.insert(node.node_id.as_str(), id);
        }
    }

    // ---- Pass 2: render -------------------------------------------------
    //
    // Table rows are rendered as ONE line (`a | b | c`), so the cell nodes
    // themselves must not be emitted again.
    let mut row_texts: HashMap<&str, String> = HashMap::new();
    let mut covered_by_row: HashSet<&str> = HashSet::new();
    for node in &ordered {
        if node.ignored {
            continue;
        }
        let role = role_of
            .get(node.node_id.as_str())
            .map(String::as_str)
            .unwrap_or("");
        if role != "row" {
            continue;
        }
        let Some(text) = row_text(node, &role_of, &index, &children_of, &id_of_node) else {
            continue;
        };
        if text.trim().is_empty() {
            continue;
        }
        row_texts.insert(node.node_id.as_str(), text);
        for cid in &node.child_ids {
            let crole = role_of.get(cid.as_str()).map(String::as_str).unwrap_or("");
            if ROW_CELL_ROLES.contains(&crole) {
                covered_by_row.insert(cid.as_str());
            }
        }
    }

    for node in &ordered {
        if node.ignored {
            continue;
        }
        if !seen.insert(node.node_id.as_str()) {
            continue;
        }
        // A cell rendered as part of its row line: emitting it again would
        // duplicate the text.
        if covered_by_row.contains(node.node_id.as_str()) {
            continue;
        }
        let role = role_of
            .get(node.node_id.as_str())
            .map(String::as_str)
            .unwrap_or("");
        let mut name = ax_name(node);
        if name.is_empty() && derives_name_from_children(role) {
            // Chromium reports an empty accessible name for some links
            // whose text is owned by child nodes. Derive it so the agent
            // can see what the element actually opens.
            name = derive_name(node.node_id.as_str(), &children_of, &index);
        }

        // An image that repeats an ancestor's accessible name (an icon link
        // whose only content is the image) adds a duplicate line, not content.
        if role == "image"
            && !name.is_empty()
            && ancestor_contains(
                &parent_of,
                &index,
                &role_of,
                node.node_id.as_str(),
                &name,
                3,
            )
        {
            continue;
        }

        // InlineTextBox is pure layout noise - always drop.
        if role == "inlinetextbox" {
            continue;
        }
        if suppressed_by_clickable.contains(node.node_id.as_str()) {
            continue;
        }
        if let Some(clickable_name) = clickable_non_aria.get(node.node_id.as_str()) {
            if let Some(id) = id_of_node.get(node.node_id.as_str()) {
                let emit_role = if role == "image" {
                    "image".to_string()
                } else {
                    "clickable".to_string()
                };
                out.push(SemanticNode {
                    action_id: Some(*id),
                    role: emit_role,
                    name: clickable_name.clone(),
                    extras: ax_extras(node, role),
                    level: None,
                    node_id: node.node_id.clone(),
                    depth: depths
                        .get(node.node_id.as_str())
                        .copied()
                        .unwrap_or_default(),
                });
                continue;
            }
        }
        // StaticText duplicating an ancestor's accessible name (e.g. a table
        // cell whose name is the concatenation of its children).
        if role == "statictext" {
            if let Some(pid) = parent_of.get(node.node_id.as_str()) {
                if name_bearing_parent.get(pid).copied().unwrap_or(false) {
                    continue;
                }
                // Text already present in an ancestor's name -> redundant.
                if ancestor_contains(&parent_of, &index, &role_of, pid, &name, 3) {
                    continue;
                }
                // A control's value is already rendered as `value="..."`.
                if ancestor_value_matches(&parent_of, &index, node.node_id.as_str(), &name, 3) {
                    continue;
                }
            }
        }

        // Table row: emit ONE line for the whole row (`a | b | c`).
        // The row's own accessible name is empty in Chromium, so without
        // this the cell text would be lost entirely.
        if role == "row" {
            if let Some(text) = row_texts.get(node.node_id.as_str()) {
                out.push(SemanticNode {
                    action_id: None,
                    level: None,
                    role: "row".to_string(),
                    name: text.clone(),
                    extras: Vec::new(),
                    node_id: node.node_id.clone(),
                    depth: depths
                        .get(node.node_id.as_str())
                        .copied()
                        .unwrap_or_default(),
                });
            }
            // The cells themselves are emitted as part of the row line.
            continue;
        }
        // A table's accessible name is the concatenation of all its rows;
        // the rows are emitted individually -> drop the duplicate.
        if role == "table" && !node.child_ids.is_empty() {
            continue;
        }
        // `rowgroup` only groups rows - the rows carry the content.
        // (Chromium computes a rowgroup's name as the concatenation of its
        // rows, so keeping it would duplicate every row.)
        if role == "rowgroup" && !node.child_ids.is_empty() {
            continue;
        }
        // `listmarker` is a pure bullet glyph - decoration, not content.
        if role == "listmarker" {
            continue;
        }
        // Landmark roles (main/banner/navigation/...) carry a name that is
        // the concatenation of their whole subtree - the children are
        // emitted individually, so we keep only the structural label.
        if LANDMARK_ROLES.contains(&role) && !node.child_ids.is_empty() {
            out.push(SemanticNode {
                action_id: None,
                level: None,
                role: role.to_string(),
                name: String::new(),
                extras: Vec::new(),
                node_id: node.node_id.clone(),
                depth: depths
                    .get(node.node_id.as_str())
                    .copied()
                    .unwrap_or_default(),
            });
            continue;
        }
        // Layout containers (Chromium's internal table roles): their name is
        // just the concatenation of their children, which we emit
        // individually - emitting the container name duplicates every child.
        if LAYOUT_CONTAINER_ROLES.contains(&role) && !node.child_ids.is_empty() {
            continue;
        }
        // Pure structure (list items): no label, no name - the children
        // carry everything worth emitting.
        if STRUCTURE_ONLY_ROLES.contains(&role) && !node.child_ids.is_empty() {
            continue;
        }
        // A StaticText that owns *meaningful* children is a concatenation of
        // them (Chromium emits the whole subtree text on the container) - the
        // children are emitted individually, so the parent is a duplicate.
        //
        // The InlineTextBox child is excluded: in Chromium EVERY StaticText
        // owns one, so treating it as "has children" dropped every piece of
        // text on the page.
        if role == "statictext" && has_meaningful_child(node, &role_of) {
            continue;
        }
        // Pure punctuation (" | ", ".", "-") is decoration, not content.
        if role == "statictext"
            && !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_punctuation() || c.is_whitespace())
            && name.chars().any(|c| c.is_ascii_punctuation())
        {
            continue;
        }

        let is_actionable_role = ACTIONABLE_ROLES.contains(&role);
        let is_structural = STRUCTURAL_ROLES.contains(&role);
        let is_content = CONTENT_ROLES.contains(&role);

        // Prune empty structural nodes entirely - no text, cannot be acted on.
        if is_structural && name.is_empty() {
            continue;
        }
        // Prune empty content shells (no name payload).
        if is_content && name.is_empty() && role != "heading" {
            continue;
        }
        // Whitespace-only names on unknown roles.
        if !is_actionable_role && name.is_empty() && !is_content && !is_structural {
            continue;
        }

        let extras = ax_extras(node, role);

        // Actionable + backend handle -> numbered line.
        if is_actionable_role {
            if name.is_empty() {
                // An unnamed link cannot be reasoned about by an agent
                // (icon/anchor without text) -> drop it. Unnamed buttons
                // stay (icon buttons are meaningful) but are marked.
                if role == "link" {
                    if let Some(&id) = id_of_node.get(node.node_id.as_str()) {
                        map.remove(&id);
                    }
                    continue;
                }
                name = "(unlabeled)".to_string();
            }
            if let Some(id) = id_of_node.get(node.node_id.as_str()) {
                out.push(SemanticNode {
                    action_id: Some(*id),
                    role: role.to_string(),
                    name,
                    extras,
                    level: None,
                    node_id: node.node_id.clone(),
                    depth: depths
                        .get(node.node_id.as_str())
                        .copied()
                        .unwrap_or_default(),
                });
                continue;
            }
            // No backend id: fall through to unnumbered content (still useful text).
        }

        // Structural nodes that hold text become plain `text` lines.
        // `statictext` is plain text too - "Statictext: foo" would be noise
        // in the agent's prompt.
        let role = if is_structural || role == "statictext" {
            "text"
        } else {
            role
        };
        if name.is_empty() && role != "heading" {
            continue;
        }
        out.push(SemanticNode {
            action_id: None,
            level: ax_level(node),
            role: role.to_string(),
            name,
            extras,
            node_id: node.node_id.clone(),
            depth: depths
                .get(node.node_id.as_str())
                .copied()
                .unwrap_or_default(),
        });
    }

    (out, map)
}

#[cfg(test)]
mod tests {
    use flashbrowse_cdp::protocol::accessibility::{AXNode, AXProperty, AXValue};

    use super::compress;

    fn val(s: &str) -> AXValue {
        AXValue {
            type_: Some("ComputedString".to_string()),
            value: Some(serde_json::Value::String(s.to_string())),
        }
    }
    fn role(s: &str) -> AXValue {
        AXValue {
            type_: Some("RoleValue".to_string()),
            value: Some(serde_json::Value::String(s.to_string())),
        }
    }
    fn node(id: &str, role_s: &str, name: &str, backend: Option<i64>) -> AXNode {
        AXNode {
            node_id: id.to_string(),
            ignored: false,
            role: Some(role(role_s)),
            name: Some(val(name)),
            value: None,
            backend_node_id: backend,
            child_ids: vec![],
            ignored_reasons: None,
            properties: None,
        }
    }
    fn with_children(mut n: AXNode, children: &[&str]) -> AXNode {
        n.child_ids = children.iter().map(|s| s.to_string()).collect();
        n
    }

    #[test]
    fn cyclic_or_duplicate_links_preserve_non_ignored_text() {
        let input = serde_json::json!([
            {"nodeId":"1","ignored":false,"role":{"value":"button"},"name":{"value":"First"},"childIds":["2"],"backendDOMNodeId":3},
            {"nodeId":"2","ignored":false,"role":{"value":"button"},"name":{"value":"Second"},"childIds":["1"],"backendDOMNodeId":4}
        ]);
        let mut input: Vec<AXNode> = serde_json::from_value(input).unwrap();
        let (out, actions) = compress(&input);
        assert_eq!(
            out.iter()
                .map(|node| node.name.as_str())
                .collect::<Vec<_>>(),
            ["First", "Second"]
        );
        assert!(actions.is_empty());
        input[1].node_id = input[0].node_id.clone();
        let (out, actions) = compress(&input);
        assert_eq!(out.len(), 2);
        assert!(actions.is_empty());
    }

    #[test]
    fn text_inside_a_dropped_layout_cell_survives() {
        // Hacker News footer: the LayoutTableCell's name is the concatenation
        // of its children and the cell itself is dropped, so its StaticText
        // child is NOT a duplicate. Treating it as one lost "Search:".
        let cell = with_children(
            node("1", "layouttablecell", "Guidelines | Search:", Some(1)),
            &["2", "3"],
        );
        let link = with_children(node("2", "link", "Guidelines", Some(2)), &["4"]);
        let label = node("3", "statictext", "Search:", None);
        let link_text = node("4", "statictext", "Guidelines", None);
        let (out, _) = compress(&[cell, link, label, link_text]);
        let names: Vec<&str> = out.iter().map(|n| n.name.as_str()).collect();
        assert!(names.contains(&"Search:"), "label dropped: {names:?}");
        assert_eq!(
            names.iter().filter(|n| **n == "Guidelines").count(),
            1,
            "link text duplicated: {names:?}"
        );
    }

    #[test]
    fn full_tree_is_a_superset_of_pruned_nodes() {
        let cell = with_children(
            node("1", "layouttablecell", "Guidelines | Search:", Some(1)),
            &["2", "3"],
        );
        let link = with_children(node("2", "link", "Guidelines", Some(2)), &["4"]);
        let label = node("3", "statictext", "Search:", None);
        let link_text = node("4", "statictext", "Guidelines", None);
        let nodes = [cell, link, label, link_text];
        let (pruned, _) = super::compress_pruned(&nodes);
        let (full, _) = super::compress_full(&nodes);
        assert!(
            full.len() >= pruned.len(),
            "full={} pruned={}",
            full.len(),
            pruned.len()
        );
    }

    #[test]
    fn slider_and_spinbutton_render_their_value() {
        let mut slider = node("1", "slider", "Zoom", Some(1));
        slider.value = Some(val("50"));
        let mut spin = node("2", "spinbutton", "Quantity", Some(2));
        spin.value = Some(val("3"));
        let (out, map) = compress(&[slider, spin]);
        assert_eq!(map.len(), 2, "both controls are actionable");
        assert!(out[0].extras.contains(&"value=\"50\"".to_string()));
        assert!(out[1].extras.contains(&"value=\"3\"".to_string()));
    }

    #[test]
    fn prune_ignored_and_empty_structural() {
        let nodes = vec![
            AXNode {
                node_id: "1".to_string(),
                ignored: true,
                role: Some(role("button")),
                name: Some(val("X")),
                value: None,
                backend_node_id: Some(1),
                child_ids: vec![],
                ignored_reasons: None,
                properties: None,
            },
            node("2", "generic", "", None),
            node("3", "button", "Submit", Some(10)),
        ];
        let (out, map) = compress(&nodes);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].action_id, Some(1));
        assert_eq!(map.get(&1), Some(&10));
    }

    #[test]
    fn assigns_sequential_ids_depth_first() {
        let nodes = vec![
            node("1", "link", "Home", Some(1)),
            node("2", "button", "Go", Some(2)),
            node("3", "heading", "Hi", None),
        ];
        let (out, map) = compress(&nodes);
        assert_eq!(out[0].action_id, Some(1));
        assert_eq!(out[1].action_id, Some(2));
        assert_eq!(out[2].action_id, None);
        assert_eq!(out[2].role, "heading");
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn unnumbered_without_backend() {
        let nodes = vec![node("1", "button", "Orphan", None)];
        let (out, map) = compress(&nodes);
        assert!(out[0].action_id.is_none());
        assert!(map.is_empty());
    }

    #[test]
    fn heading_level_read_from_properties() {
        let mut h3 = node("1", "heading", "Three", None);
        h3.properties = Some(vec![AXProperty {
            name: "level".to_string(),
            value: Some(AXValue {
                type_: Some("integer".to_string()),
                value: Some(serde_json::Value::from(3)),
            }),
        }]);
        let (out, _) = compress(&[h3]);
        assert_eq!(out[0].level, Some(3));
    }

    #[test]
    fn heading_text_not_duplicated_by_statictext() {
        let nodes = vec![
            with_children(node("1", "heading", "Three", None), &["2"]),
            node("2", "statictext", "Three", Some(5)),
        ];
        let (out, _) = compress(&nodes);
        assert_eq!(out.len(), 1, "statictext dupe must be dropped");
        assert_eq!(out[0].role, "heading");
        assert_eq!(out[0].name, "Three");
    }

    #[test]
    fn input_value_is_reported() {
        let mut tb = node("1", "textbox", "Name", Some(9));
        tb.value = Some(val("oldv"));
        let (out, _) = compress(&[tb]);
        assert!(out[0].extras.iter().any(|e| e == "value=\"oldv\""));
    }

    #[test]
    fn table_row_keeps_cell_text() {
        // table > rowgroup > row > cell > statictext
        let nodes = vec![
            with_children(node("1", "table", "", None), &["2"]),
            with_children(node("2", "rowgroup", "", None), &["3"]),
            with_children(node("3", "row", "", None), &["4", "6"]),
            with_children(node("4", "cell", "alpha", None), &["5"]),
            node("5", "statictext", "alpha", None),
            with_children(node("6", "cell", "ok", None), &["7"]),
            node("7", "statictext", "ok", None),
        ];
        let (out, _) = compress(&nodes);
        let row = out
            .iter()
            .find(|n| n.role == "row")
            .expect("the row must be emitted");
        assert_eq!(row.name, "alpha | ok");
        // The cell text must NOT be dropped, and must NOT be duplicated.
        let text: Vec<&str> = out.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(text.iter().filter(|t| t.contains("alpha")).count(), 1);
    }

    #[test]
    fn table_row_keeps_text_when_cell_has_no_name() {
        // A cell whose accessible name is empty but which owns the text.
        let nodes = vec![
            with_children(node("1", "table", "", None), &["2"]),
            with_children(node("2", "row", "", None), &["3"]),
            with_children(node("3", "cell", "", None), &["4"]),
            node("4", "statictext", "gamma", None),
        ];
        let (out, _) = compress(&nodes);
        let row = out.iter().find(|n| n.role == "row").expect("row");
        assert_eq!(row.name, "gamma");
    }

    #[test]
    fn list_item_keeps_its_text() {
        // list > listitem > statictext
        let nodes = vec![
            with_children(node("1", "list", "", None), &["2"]),
            with_children(node("2", "listitem", "", None), &["3"]),
            node("3", "statictext", "Plain item", None),
        ];
        let (out, _) = compress(&nodes);
        assert!(
            out.iter().any(|n| n.name == "Plain item"),
            "list item text must survive: {:?}",
            out.iter().map(|n| &n.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn output_is_in_document_order() {
        // Chromium returns the AX tree breadth-first, so the raw array is
        // [parent, childA, childB, grandchildA, grandchildB]. Reading the
        // raw order would put "First" before "Second" only by accident;
        // the tree says First comes first, and so must the output.
        let nodes = vec![
            with_children(node("1", "generic", "", None), &["2", "3"]),
            with_children(node("2", "heading", "First", None), &["4"]),
            with_children(node("3", "heading", "Second", None), &["5"]),
            node("4", "statictext", "First", None),
            node("5", "statictext", "Second", None),
        ];
        let (out, _) = compress(&nodes);
        let names: Vec<&str> = out.iter().map(|n| n.name.as_str()).collect();
        let first = names.iter().position(|n| *n == "First").expect("First");
        let second = names.iter().position(|n| *n == "Second").expect("Second");
        assert!(first < second, "document order broken: {names:?}");
    }

    #[test]
    fn actionable_descendant_is_not_duplicated_in_parent_text() {
        // A list item containing a link: the link renders itself with an
        // action ID, so the list item must not repeat its text.
        let nodes = vec![
            with_children(node("1", "list", "", None), &["2"]),
            with_children(node("2", "listitem", "", None), &["3"]),
            with_children(node("3", "link", "Linked item", Some(42)), &["4"]),
            node("4", "statictext", "Linked item", None),
        ];
        let (out, map) = compress(&nodes);
        assert_eq!(map.get(&1), Some(&42));
        let link_lines = out.iter().filter(|n| n.action_id.is_some()).count();
        assert_eq!(link_lines, 1);
        // The link text must appear exactly once in the whole output.
        let occurrences = out
            .iter()
            .filter(|n| n.name.contains("Linked item"))
            .count();
        assert_eq!(occurrences, 1, "link text duplicated: {out:?}");
    }

    #[test]
    fn clickable_non_aria_div_gets_action_id_without_duplicating_text_or_wrapping_buttons() {
        let mut clickable_div = node("2", "generic", "", Some(20));
        clickable_div.properties = Some(vec![AXProperty {
            name: "clickable".to_string(),
            value: Some(AXValue {
                type_: Some("boolean".to_string()),
                value: Some(serde_json::Value::Bool(true)),
            }),
        }]);
        let mut wrapper_div = node("4", "generic", "", Some(40));
        wrapper_div.properties = Some(vec![AXProperty {
            name: "clickable".to_string(),
            value: Some(AXValue {
                type_: Some("boolean".to_string()),
                value: Some(serde_json::Value::Bool(true)),
            }),
        }]);
        let nodes = vec![
            with_children(node("1", "generic", "", None), &["2", "4"]),
            with_children(clickable_div, &["3"]),
            node("3", "statictext", "Custom Div", None),
            with_children(wrapper_div, &["5"]),
            with_children(node("5", "button", "Real Button", Some(50)), &["6"]),
            node("6", "statictext", "Real Button", None),
        ];
        let (out, map) = compress(&nodes);
        assert_eq!(map.get(&1), Some(&20));
        assert_eq!(map.get(&2), Some(&50));
        assert_eq!(map.len(), 2, "wrapper div must not duplicate button action");
        assert_eq!(
            out.len(),
            2,
            "child statictext must not be duplicated: {out:?}"
        );
        assert_eq!(out[0].action_id, Some(1));
        assert_eq!(out[0].role, "clickable");
        assert_eq!(out[0].name, "Custom Div");
        assert_eq!(out[1].action_id, Some(2));
        assert_eq!(out[1].role, "button");
        assert_eq!(out[1].name, "Real Button");
    }
}
