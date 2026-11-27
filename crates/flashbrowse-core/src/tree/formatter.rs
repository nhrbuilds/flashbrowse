//! LLM-optimized textual representation renderer.
//!
//! Output format:
//! ```text
//! Title: Hacker News
//! URL: https://news.ycombinator.com
//! ---
//! [1] Link: "Hacker News"
//! Heading 1: Top Stories
//! [10] Link: "Rust 1.85 Released" (value="http://...")
//! ```

use super::node::SemanticNode;

/// Max characters per name before elision (token guard).
///
/// Public so the fidelity test can assert the truncation bound without
/// duplicating the number.
pub const MAX_NAME_CHARS: usize = 120;

/// Longest useful `extras` payload per line (token guard).
pub const MAX_EXTRAS_CHARS: usize = 80;

fn truncate_to(s: &str, max_chars: usize) -> String {
    // Char-count truncation, never mid-UTF8.
    if max_chars == 0 {
        return String::new();
    }
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    if max_chars == 1 {
        return "…".to_string();
    }
    let mut out: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn truncate(s: &str) -> String {
    truncate_to(s, MAX_NAME_CHARS)
}

/// Extract `[N]` action references from a cell string.
fn extract_action_refs(cell: &str) -> Vec<&str> {
    let mut refs = Vec::new();
    let mut tail = cell;
    while let Some((_, rest)) = tail.split_once('[') {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        let after_digits = rest.get(digits..).unwrap_or_default();
        if let Some(after_ref) = after_digits.strip_prefix(']').filter(|_| digits > 0) {
            let start = cell.len().saturating_sub(rest.len()).saturating_sub(1);
            let end = cell.len().saturating_sub(after_ref.len());
            if let Some(reference) = cell.get(start..end) {
                refs.push(reference);
            }
            tail = after_ref;
        } else {
            tail = rest;
        }
    }
    refs
}

/// Truncate a single cell to `budget` chars while preserving `[N]` refs.
fn truncate_cell_keeping_refs(cell: &str, budget: usize) -> String {
    if cell.chars().count() <= budget {
        return cell.to_string();
    }
    let refs = extract_action_refs(cell);
    if refs.is_empty() {
        return truncate_to(cell, budget);
    }
    let ref_tail = refs.join(" ");
    let tail_len = ref_tail.chars().count();
    let prefix_budget = budget.saturating_sub(tail_len.saturating_add(1));
    if prefix_budget < 2 {
        return ref_tail;
    }
    let prefix = truncate_to(cell.trim(), prefix_budget);
    format!("{prefix} {ref_tail}")
}

/// Truncate a table row across its ` | ` cells without dropping `[N]` refs.
fn truncate_row(s: &str) -> String {
    if s.chars().count() <= MAX_NAME_CHARS {
        return s.to_string();
    }
    let cells: Vec<&str> = s.split(" | ").collect();
    if cells.len() <= 1 {
        return truncate_cell_keeping_refs(s, MAX_NAME_CHARS);
    }

    let sep_chars = cells.len().saturating_sub(1).saturating_mul(3);
    let mut alloc = vec![0usize; cells.len()];
    let mut flex: Vec<(usize, usize)> = Vec::new();
    let mut used = sep_chars;

    for (i, &cell) in cells.iter().enumerate() {
        let len = cell.chars().count();
        let has_refs = !extract_action_refs(cell).is_empty();
        // Short ref-bearing cells (like `[1]` or `[1] [2]`) are protected in full
        // so action links on the row are never truncated away.
        if has_refs && len <= 24 {
            if let Some(slot) = alloc.get_mut(i) {
                *slot = len;
            }
            used = used.saturating_add(len);
        } else {
            flex.push((i, len));
        }
    }

    flex.sort_by_key(|&(_, len)| len);
    let mut rem = MAX_NAME_CHARS.saturating_sub(used);
    let mut left = flex.len();
    for (i, len) in flex {
        let share = rem.checked_div(left).unwrap_or(0);
        let take = len.min(share);
        if let Some(slot) = alloc.get_mut(i) {
            *slot = take;
        }
        rem = rem.saturating_sub(take);
        left = left.saturating_sub(1);
    }

    cells
        .iter()
        .zip(alloc)
        .map(|(&cell, budget)| truncate_cell_keeping_refs(cell, budget))
        .collect::<Vec<_>>()
        .join(" | ")
}

fn truncate_for_node(n: &SemanticNode) -> String {
    let trimmed = n.name.trim();
    if n.role == "row" {
        truncate_row(trimmed)
    } else {
        truncate(trimmed)
    }
}

fn render_extras(extras: &[String]) -> String {
    if extras.is_empty() {
        return String::new();
    }
    let mut joined = extras.join(", ");
    if joined.chars().count() > MAX_EXTRAS_CHARS {
        joined = joined
            .chars()
            .take(MAX_EXTRAS_CHARS - 1)
            .collect::<String>()
            + "…";
    }
    format!(" ({joined})")
}

fn role_title(role: &str) -> String {
    match role {
        "heading" => "Heading".to_string(),
        "text" => "Text".to_string(),
        other => {
            let mut c = other.chars();
            match c.next() {
                None => String::new(),
                Some(first) => first.to_ascii_uppercase().to_string() + c.as_str(),
            }
        }
    }
}

/// One semantic node as a single output line (no trailing newline).
fn format_node(n: &SemanticNode) -> Option<String> {
    let name = truncate_for_node(n);
    let role_title = role_title(&n.role);
    match n.action_id {
        Some(id) => Some(format!(
            "[{id}] {role_title}: \"{name}\"{}",
            render_extras(&n.extras)
        )),
        None if n.role == "heading" => {
            if name.is_empty() {
                return None;
            }
            let lvl = n.level.unwrap_or(1);
            Some(format!("{role_title} {lvl}: {name}"))
        }
        None => {
            if name.is_empty() {
                return Some(role_title);
            }
            Some(format!("{role_title}: {name}{}", render_extras(&n.extras)))
        }
    }
}

/// Render compressor output to the spec's LLM text format (flat list).
pub fn render(title: &str, url: &str, nodes: &[SemanticNode]) -> String {
    let mut out = String::new();
    out.push_str(&format!("Title: {}\n", title.trim()));
    out.push_str(&format!("URL: {}\n", url.trim()));
    out.push_str("---\n");

    for n in nodes {
        if let Some(line) = format_node(n) {
            out.push_str(&line);
            out.push('\n');
        }
    }
    out
}

/// Same content as [`render`], indented by [`SemanticNode::depth`] up to 128 levels.
pub fn render_nested(title: &str, url: &str, nodes: &[SemanticNode]) -> String {
    let mut out = String::new();
    out.push_str(&format!("Title: {}\n", title.trim()));
    out.push_str(&format!("URL: {}\n", url.trim()));
    out.push_str("---\n");

    for n in nodes {
        if let Some(line) = format_node(n) {
            for _ in 0..n.depth.min(128) {
                out.push_str("  ");
            }
            out.push_str(&line);
            out.push('\n');
        }
    }
    out
}

/// (role, name, extras joined, action_id) tuples for equality checks.
pub fn semantic_tuples(nodes: &[SemanticNode]) -> Vec<(String, String, String, Option<u32>)> {
    nodes
        .iter()
        .filter_map(|n| {
            let extras = n.extras.join(", ");
            if n.name.trim().is_empty() && n.action_id.is_none() && n.role != "heading" {
                return None;
            }
            Some((n.role.clone(), truncate_for_node(n), extras, n.action_id))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{render, render_nested, semantic_tuples};
    use crate::tree::node::SemanticNode;

    fn sn(id: Option<u32>, role: &str, name: &str) -> SemanticNode {
        SemanticNode {
            action_id: id,
            role: role.to_string(),
            name: name.to_string(),
            extras: vec![],
            level: if role == "heading" { Some(1) } else { None },
            node_id: "1".to_string(),
            depth: 0,
        }
    }

    #[test]
    fn maximum_depth_keeps_visible_text_and_ids() {
        let mut n = sn(Some(7), "button", "Kept");
        n.depth = usize::MAX;
        let rendered = render_nested("T", "u", &[n]);
        let line = rendered.lines().last().expect("node line");
        assert_eq!(line.len() - line.trim_start().len(), 256);
        assert!(line.contains("[7] Button: \"Kept\""));
    }

    #[test]
    fn renders_extras_on_plain_content() {
        // `option` and `disclosuretriangle` are not actionable, but their
        // state is exactly what an agent needs to read the page.
        let mut n = sn(None, "option", "Japan");
        n.extras = vec!["selected".to_string()];
        let out = render("T", "u", &[n]);
        assert!(out.contains("Option: Japan (selected)"), "{out}");
    }

    #[test]
    fn renders_numbered_lines() {
        let out = render(
            "HN",
            "https://news.ycombinator.com",
            &[
                sn(Some(1), "link", "Hacker News"),
                sn(None, "heading", "Top Stories"),
            ],
        );
        assert!(out.contains("Title: HN"));
        assert!(out.contains("URL: https://news.ycombinator.com"));
        assert!(out.contains("[1] Link: \"Hacker News\""));
        assert!(out.contains("Heading 1: Top Stories"));
    }

    #[test]
    fn nested_and_flat_share_semantic_tuples() {
        let mut child = sn(Some(2), "link", "Child");
        child.depth = 2;
        let mut parent = sn(None, "navigation", "");
        parent.depth = 1;
        let root = sn(Some(1), "link", "Root");
        let nodes = vec![root, parent, child];
        let flat = render("T", "http://example.com", &nodes);
        let nested = render_nested("T", "http://example.com", &nodes);
        assert_eq!(semantic_tuples(&nodes), semantic_tuples(&nodes));
        for line in flat.lines().skip(3) {
            let trimmed = line.trim_start();
            assert!(
                nested.contains(trimmed),
                "nested output must contain flat line {trimmed:?}"
            );
        }
    }

    #[test]
    fn row_truncation_preserves_trailing_action_refs() {
        let long_desc = "x".repeat(200);
        let row = sn(None, "row", &format!("1 | {long_desc} | [1] [2]"));
        let out = render("T", "u", &[row]);
        let row_line = out.lines().nth(3).unwrap();
        assert!(row_line.starts_with("Row: 1 | "), "{row_line}");
        assert!(row_line.ends_with(" | [1] [2]"), "{row_line}");
        assert!(
            row_line.strip_prefix("Row: ").unwrap().chars().count() <= super::MAX_NAME_CHARS,
            "{row_line}"
        );
    }

    #[test]
    fn wide_multi_column_row_preserves_action_refs() {
        let c = "col-value-1234567890-abcdef".to_string();
        let row = sn(None, "row", &format!("{c} | {c} | {c} | {c} | {c} | [9]"));
        let out = render("T", "u", &[row]);
        let row_line = out.lines().nth(3).unwrap();
        assert!(row_line.ends_with(" | [9]"), "{row_line}");
        assert!(
            row_line.strip_prefix("Row: ").unwrap().chars().count() <= super::MAX_NAME_CHARS,
            "{row_line}"
        );
    }
}
