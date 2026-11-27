//! Fidelity oracle: the compressed tree is measured against the raw AX tree.
//!
//! Both sides are held in the same process. `raw_ax_nodes()` is what Chromium
//! produced; `last_tree()` is what an agent sees. These assertions encode
//! accuracy rule 1 ("never drop visible text") so a pruning change that loses
//! content fails here instead of shipping.
//!
//! What is checked:
//!
//! - text recall: every visible `StaticText` survives (truncation excepted)
//! - action recall: every actionable element keeps an action ID
//! - document order: section markers appear in page order
//! - no duplication: named strings appear exactly once
//! - hidden text: `display:none`, `visibility:hidden`, `aria-hidden` stay out
//! - state: `checked`, `disabled`, `expanded`, selected value, input value
//! - truncation: the 120-char bound holds and exactly-120 names are kept
//!
//! Skips gracefully when no Chromium binary is available.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;

use flashbrowse_cdp::protocol::accessibility::AXNode;
use flashbrowse_core::tree::formatter::MAX_NAME_CHARS;
use flashbrowse_core::tree::ActionMap;
use flashbrowse_core::{BrowserSession, SessionConfig};

// ---------------------------------------------------------------- server

/// Serve `tests/fixtures/*` on a throwaway port. One connection at a time is
/// enough: the browser only fetches the page and a favicon.
fn spawn_static_server() -> (u16, Arc<std::thread::JoinHandle<()>>) {
    use std::net::Shutdown;
    use std::time::Duration;

    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let handle = std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let dir = dir.clone();
            std::thread::spawn(move || {
                let mut stream = stream;
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let mut request = String::new();
                if let Ok(cloned) = stream.try_clone() {
                    let mut reader = BufReader::new(cloned);
                    if reader.read_line(&mut request).unwrap_or(0) == 0 {
                        return;
                    }
                    let mut line = String::new();
                    while reader.read_line(&mut line).unwrap_or(0) > 0 {
                        if line == "\r\n" || line == "\n" {
                            break;
                        }
                        line.clear();
                    }
                }
                let path = request.split_whitespace().nth(1).unwrap_or("/");
                let file = path.trim_start_matches('/');
                let file = if file.is_empty() { "index.html" } else { file };
                match std::fs::read(dir.join(file)) {
                    Ok(body) => {
                        let head = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = stream.write_all(head.as_bytes());
                        let _ = stream.write_all(&body);
                    }
                    Err(_) => {
                        let _ = stream.write_all(
                            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        );
                    }
                }
                let _ = stream.flush();
                let _ = stream.shutdown(Shutdown::Write);
            });
        }
    });
    (port, Arc::new(handle))
}

// ---------------------------------------------------------------- oracle

/// Roles that must carry an action ID when Chromium gives them a backend
/// handle. Kept independent of the compressor's list on purpose: a role that
/// is dropped from both would otherwise never fail.
const ACTIONABLE: &[&str] = &[
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

/// Roles whose accessible name comes from their subtree, so a child
/// `StaticText` with the same text is decorative rather than lost.
const NAME_BEARING: &[&str] = &[
    "statictext",
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

fn role_of_node(node: &AXNode) -> String {
    node.role
        .as_ref()
        .map(|v| v.as_text())
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

fn name_of(node: &AXNode) -> String {
    node.name
        .as_ref()
        .map(|v| v.as_text())
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// Collapse runs of whitespace so a raw name and a rendered line compare
/// equal even when the source HTML wrapped the text.
fn normalize_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// What the renderer is allowed to emit for `name` (truncation included).
///
/// The formatter truncates the raw name; fixtures use single-token long
/// labels so normalizing first cannot change the length.
fn expected(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    if chars.len() <= MAX_NAME_CHARS {
        return name.to_string();
    }
    let mut out: String = chars[..MAX_NAME_CHARS - 1].iter().collect();
    out.push('…');
    out
}

/// Same rule as the compressor: punctuation-only text is decoration.
fn is_punctuation_only(s: &str) -> bool {
    s.chars()
        .all(|c| c.is_ascii_punctuation() || c.is_whitespace())
        && s.chars().any(|c| c.is_ascii_punctuation())
}

struct Snapshot {
    raw: Vec<AXNode>,
    out: String,
    map: ActionMap,
    parent_of: HashMap<String, String>,
    children_of: HashMap<String, Vec<String>>,
    role_of: HashMap<String, String>,
    index: HashMap<String, usize>,
}

impl Snapshot {
    fn build(raw: Vec<AXNode>, out: String, map: ActionMap) -> Self {
        let mut parent_of = HashMap::new();
        let mut children_of: HashMap<String, Vec<String>> = HashMap::new();
        let mut role_of = HashMap::new();
        let mut index = HashMap::new();
        for (i, n) in raw.iter().enumerate() {
            index.insert(n.node_id.clone(), i);
            role_of.insert(n.node_id.clone(), role_of_node(n));
            for c in &n.child_ids {
                parent_of.insert(c.clone(), n.node_id.clone());
                children_of
                    .entry(n.node_id.clone())
                    .or_default()
                    .push(c.clone());
            }
        }
        Self {
            raw,
            out,
            map,
            parent_of,
            children_of,
            role_of,
            index,
        }
    }

    fn role(&self, id: &str) -> &str {
        self.role_of.get(id).map(String::as_str).unwrap_or("")
    }

    fn node(&self, id: &str) -> Option<&AXNode> {
        self.index.get(id).map(|&i| &self.raw[i])
    }

    fn has_meaningful_child(&self, id: &str) -> bool {
        self.children_of.get(id).is_some_and(|kids| {
            kids.iter().any(|k| {
                let role = self.role(k);
                role != "inlinetextbox" && role != "none"
            })
        })
    }

    /// True when an ancestor within `depth` renders its own non-empty name,
    /// which makes this `StaticText` a decorative duplicate.
    fn has_named_ancestor(&self, id: &str, depth: usize) -> bool {
        let mut cur = id;
        for _ in 0..depth {
            let Some(parent) = self.parent_of.get(cur) else {
                return false;
            };
            if NAME_BEARING.contains(&self.role(parent)) {
                if let Some(node) = self.node(parent) {
                    if !name_of(node).is_empty() {
                        return true;
                    }
                }
            }
            cur = parent;
        }
        false
    }

    /// Visible text that must appear in the compressed tree.
    fn expected_texts(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for n in &self.raw {
            if n.ignored || self.role(&n.node_id) != "statictext" {
                continue;
            }
            let name = normalize_ws(&name_of(n));
            if name.is_empty() || is_punctuation_only(&name) {
                continue;
            }
            // A concatenation node: its children carry the text.
            if self.has_meaningful_child(&n.node_id) {
                continue;
            }
            // Decorative text inside a named control or cell.
            if self.has_named_ancestor(&n.node_id, 3) {
                continue;
            }
            out.push((n.node_id.clone(), expected(&name)));
        }
        out
    }
}

fn text_recall_failures(s: &Snapshot) -> Vec<String> {
    s.expected_texts()
        .into_iter()
        .filter(|(_, text)| !s.out.contains(text))
        .map(|(id, text)| format!("visible text missing (node {id}): {text:?}"))
        .collect()
}

fn action_failures(s: &Snapshot) -> Vec<String> {
    let mut failures = Vec::new();
    let raw_backends: HashSet<i64> = s.raw.iter().filter_map(|n| n.backend_node_id).collect();
    let mapped: HashSet<i64> = s.map.values().copied().collect();

    // A stale handle would let an action fire at a node that no longer exists.
    let stale: Vec<i64> = mapped.difference(&raw_backends).copied().collect();
    if !stale.is_empty() {
        failures.push(format!(
            "action map holds backend ids absent from the raw tree: {stale:?}"
        ));
    }

    for n in &s.raw {
        if n.ignored {
            continue;
        }
        let role = role_of_node(n);
        if !ACTIONABLE.contains(&role.as_str()) {
            continue;
        }
        let Some(backend) = n.backend_node_id else {
            continue;
        };
        let name = normalize_ws(&name_of(n));
        // An unnamed link is dropped by design; an unnamed button is not.
        if name.is_empty() && role == "link" {
            continue;
        }
        if !mapped.contains(&backend) {
            failures.push(format!(
                "actionable node without an action id: {role} {name:?} (backend {backend})"
            ));
        }
    }
    failures
}

fn order_failures(s: &Snapshot, markers: &[&str]) -> Vec<String> {
    let mut failures = Vec::new();
    let mut last = 0usize;
    for m in markers {
        match s.out.find(m) {
            None => failures.push(format!("document-order marker missing: {m:?}")),
            Some(pos) if pos < last => failures.push(format!("out of document order: {m:?}")),
            Some(pos) => last = pos,
        }
    }
    failures
}

fn duplicate_failures(s: &Snapshot, unique: &[&str]) -> Vec<String> {
    unique
        .iter()
        .filter_map(|text| {
            let n = s.out.matches(text).count();
            (n != 1).then(|| format!("{text:?} appears {n} times, expected exactly 1"))
        })
        .collect()
}

fn count_failures(s: &Snapshot, counts: &[(&str, usize)]) -> Vec<String> {
    counts
        .iter()
        .filter_map(|(text, want)| {
            let got = s.out.matches(text).count();
            (got != *want).then(|| format!("{text:?} appears {got} times, expected {want}"))
        })
        .collect()
}

fn absent_failures(s: &Snapshot, texts: &[&str]) -> Vec<String> {
    texts
        .iter()
        .filter(|t| s.out.contains(**t))
        .map(|t| format!("hidden text leaked into the tree: {t:?}"))
        .collect()
}

fn required_failures(s: &Snapshot, texts: &[&str]) -> Vec<String> {
    texts
        .iter()
        .filter(|t| !s.out.contains(**t))
        .map(|t| format!("expected text missing: {t:?}"))
        .collect()
}

/// Truncation must be bounded, not lossy: 119 chars plus the ellipsis for a
/// 200-char label, and no truncation at exactly the limit.
fn truncation_failures(s: &Snapshot) -> Vec<String> {
    let mut failures = Vec::new();
    let kept = "A".repeat(MAX_NAME_CHARS - 1);
    if !s.out.contains(&kept) {
        failures.push(format!(
            "200-char label lost its first {} characters",
            MAX_NAME_CHARS - 1
        ));
    }
    let beyond = "A".repeat(MAX_NAME_CHARS + 10);
    if s.out.contains(&beyond) {
        failures.push("200-char label was not truncated at the limit".to_string());
    }
    let exact = "B".repeat(MAX_NAME_CHARS);
    if !s.out.contains(&exact) {
        failures.push("a name of exactly the limit was truncated".to_string());
    }
    failures
}

// ---------------------------------------------------------------- cases

struct Case {
    file: &'static str,
    /// Substrings that must appear, in this order.
    markers: &'static [&'static str],
    /// Substrings that must appear exactly once.
    unique: &'static [&'static str],
    /// Substrings with an expected occurrence count.
    counts: &'static [(&'static str, usize)],
    /// Text that must NOT appear.
    absent: &'static [&'static str],
    /// Substrings that must appear (states, values, unicode).
    required: &'static [&'static str],
}

const CASES: &[Case] = &[
    Case {
        file: "forms.html",
        markers: &[
            "Form fixture",
            "Name",
            "Country",
            "Plan",
            "Save",
            "Advanced options",
            "Visible closing paragraph.",
        ],
        unique: &[
            "Visible closing paragraph.",
            "Advanced body text.",
            "Initial note text",
        ],
        counts: &[("Subscribe", 1)],
        absent: &[
            "SECRET HIDDEN CONTENT",
            "INVISIBLE HIDDEN TEXT",
            "ARIA HIDDEN TEXT",
        ],
        required: &[
            "value=\"Ada\"",
            "value=\"Japan\"",
            "checked",
            "disabled",
            "expanded",
        ],
    },
    Case {
        file: "table.html",
        markers: &[
            "Table fixture",
            "Filter",
            "ID",
            "item-01",
            "Merged cell",
            "item-25",
            "item-50",
            "Previous page",
            "Next page",
        ],
        unique: &["item-01", "item-25", "item-50", "Merged cell"],
        counts: &[("item-", 50)],
        absent: &[],
        required: &["1 | item-01 | ok", "Merged cell | note"],
    },
    Case {
        file: "a11y.html",
        markers: &[
            "Accessibility fixture",
            "Close dialog",
            "Home page",
            "Read more",
            "Bold",
            "Title only link",
            "Overview",
            "Details",
        ],
        unique: &["Close dialog", "Home page", "Title only link"],
        counts: &[("Read more", 5), ("(unlabeled)", 1)],
        absent: &[],
        required: &[],
    },
    Case {
        file: "unicode.html",
        markers: &["Unicode fixture", "Emoji", "Line one", "Combining"],
        unique: &["日本語のテキスト", "مرحبا بالعالم"],
        counts: &[],
        absent: &[],
        required: &["café", "Line one line two with spaces."],
    },
    Case {
        file: "shadow.html",
        markers: &[
            "Shadow fixture",
            "Light DOM paragraph before the host.",
            "Shadow paragraph text.",
            "Light DOM paragraph after the host.",
        ],
        unique: &["Shadow paragraph text.", "Shadow button"],
        counts: &[],
        absent: &[],
        required: &["Shadow button"],
    },
    Case {
        file: "frames.html",
        markers: &[
            "Frame fixture",
            "Parent text before the frame.",
            "Inside frame text.",
            "Frame button",
            "Parent text after the frame.",
        ],
        unique: &["Parent text before the frame.", "Inside frame text."],
        counts: &[],
        absent: &[],
        required: &["Frame button"],
    },
    Case {
        file: "spatial.html",
        markers: &[
            "Spatial fixture",
            "Chart summary",
            "Red zone",
            "Green zone",
            "Zoom",
            "Drag me",
            "Legend",
        ],
        unique: &[
            "Chart summary: revenue up 12 percent.",
            "Red zone",
            "Green zone",
            "Drag me",
        ],
        counts: &[],
        absent: &[],
        required: &["value=\"50\""],
    },
];

#[tokio::test]
async fn fidelity_across_fixtures() {
    if flashbrowse_cdp::find_chrome(None).is_err() {
        eprintln!("SKIP: no Chromium binary found");
        return;
    }

    let (port, _server) = spawn_static_server();
    let mut session = BrowserSession::launch(SessionConfig::default())
        .await
        .expect("launch");

    let mut failures: Vec<String> = Vec::new();
    for case in CASES {
        let url = format!("http://127.0.0.1:{port}/{}", case.file);
        session
            .open(&url)
            .await
            .unwrap_or_else(|e| panic!("open {}: {e}", case.file));

        let out = session.last_tree().to_string();
        let map = session.action_map().clone();
        let raw = session
            .raw_ax_nodes()
            .await
            .expect("raw AX tree must be readable");
        let snap = Snapshot::build(raw, out, map);

        let mut case_failures = Vec::new();
        case_failures.extend(text_recall_failures(&snap));
        case_failures.extend(action_failures(&snap));
        case_failures.extend(order_failures(&snap, case.markers));
        case_failures.extend(duplicate_failures(&snap, case.unique));
        case_failures.extend(count_failures(&snap, case.counts));
        case_failures.extend(absent_failures(&snap, case.absent));
        case_failures.extend(required_failures(&snap, case.required));
        if case.file == "unicode.html" {
            case_failures.extend(truncation_failures(&snap));
        }
        failures.extend(
            case_failures
                .into_iter()
                .map(|msg| format!("{}: {msg}", case.file)),
        );
    }

    session.shutdown().await;
    assert!(
        failures.is_empty(),
        "{} fidelity failure(s):\n\n{}",
        failures.len(),
        failures.join("\n")
    );
}
