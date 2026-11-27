//! Chromium CDP implementation of browser primitives used by [`BrowserSession`](crate::BrowserSession).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use flashbrowse_cdp::protocol::{accessibility, dom, network, page, runtime, target};
use flashbrowse_cdp::CdpClient;
use futures_util::stream::{self, StreamExt};
use serde_json::Value;
use tokio::sync::OnceCell;

use crate::driver::FrameSession;
use crate::interceptor;
use crate::resource_policy::ResourcePolicy;
use crate::CoreError;

const CLICKABLE_NON_ARIA_SCRIPT: &str = r#"(() => {
  const skipTags = new Set(['HTML', 'BODY', 'A', 'BUTTON', 'INPUT', 'SELECT', 'TEXTAREA', 'SUMMARY', 'LABEL', 'OPTION']);
  const direct = [];
  const pointer = [];
  const hasReactClick = (el) => {
    for (const k of Object.keys(el)) {
      if (k.startsWith('__reactProps$') && el[k] && typeof el[k].onClick === 'function') {
        return true;
      }
    }
    return false;
  };
  const roots = [{ elements: document.querySelectorAll('*')[Symbol.iterator](), host: null }];
  while (roots.length) {
      const frame = roots[roots.length - 1];
      const next = frame.host ? { value: frame.host, done: false } : frame.elements.next();
      if (next.done || (direct.length >= 48 && pointer.length >= 48)) {
        roots.pop();
        continue;
      }
      const el = next.value;
      if (!frame.host && el.shadowRoot) {
        frame.host = el;
        roots.push({ elements: el.shadowRoot.querySelectorAll('*')[Symbol.iterator](), host: null });
        continue;
      }
      frame.host = null;
      if (skipTags.has(el.tagName)) continue;
      if (el.hasAttribute('role')) continue;
      const st = getComputedStyle(el);
      if (st.display === 'none' || st.visibility === 'hidden') continue;
      if (el.getClientRects().length === 0) continue;
      if (el.hasAttribute('onclick') || typeof el.onclick === 'function' || hasReactClick(el)) {
        if (direct.length < 48) direct.push(el);
        continue;
      }
      if (pointer.length < 48 && st.cursor === 'pointer') {
        const text = (el.innerText || el.textContent || '').trim();
        if (!text || text.length > 120) continue;
        const parentCursor = el.parentElement ? getComputedStyle(el.parentElement).cursor : '';
        if (parentCursor === 'pointer') continue;
        pointer.push(el);
      }
  }
  if (!direct.length && !pointer.length) return null;
  return { direct, pointer };
})()"#;

async fn extract_array_element_object_ids(
    client: &Arc<CdpClient>,
    session_id: &str,
    array_object_id: &str,
) -> Vec<String> {
    let Ok(props) = client
        .send(
            Some(session_id),
            "Runtime.getProperties",
            serde_json::json!({
                "objectId": array_object_id,
                "ownProperties": true,
            }),
        )
        .await
    else {
        return Vec::new();
    };
    props
        .get("result")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|entry| {
            entry
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|name| name.chars().all(|c| c.is_ascii_digit()))
        })
        .filter_map(|entry| {
            entry
                .get("value")
                .and_then(|v| v.get("objectId"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect()
}

async fn has_click_event_listener(
    client: &Arc<CdpClient>,
    session_id: &str,
    object_id: &str,
) -> bool {
    let Ok(resp) = client
        .send(
            Some(session_id),
            "DOMDebugger.getEventListeners",
            serde_json::json!({
                "objectId": object_id,
                "depth": 1,
                "pierce": false,
            }),
        )
        .await
    else {
        return false;
    };
    resp.get("listeners")
        .and_then(Value::as_array)
        .is_some_and(|list| {
            list.iter().any(|l| {
                matches!(
                    l.get("type").and_then(Value::as_str),
                    Some("click" | "mousedown" | "pointerdown")
                )
            })
        })
}

async fn collect_clickable_non_aria_backends(
    client: &Arc<CdpClient>,
    session_id: &str,
) -> HashSet<i64> {
    let backends = collect_clickable_non_aria_backends_inner(client, session_id).await;
    // Release the group unconditionally: every early return inside the inner
    // helper would otherwise leave V8 remote objects alive until navigation.
    let _ = client
        .send(
            Some(session_id),
            "Runtime.releaseObjectGroup",
            serde_json::json!({ "objectGroup": "fb-clickables" }),
        )
        .await;
    backends
}

async fn collect_clickable_non_aria_backends_inner(
    client: &Arc<CdpClient>,
    session_id: &str,
) -> HashSet<i64> {
    const OBJECT_GROUP: &str = "fb-clickables";
    let Ok(eval) = client
        .send(
            Some(session_id),
            "Runtime.evaluate",
            serde_json::json!({
                "expression": CLICKABLE_NON_ARIA_SCRIPT,
                "returnByValue": false,
                "objectGroup": OBJECT_GROUP,
            }),
        )
        .await
    else {
        return HashSet::new();
    };
    let Some(root_object_id) = eval
        .get("result")
        .and_then(|r| r.get("objectId"))
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return HashSet::new();
    };
    let Ok(root_props) = client
        .send(
            Some(session_id),
            "Runtime.getProperties",
            serde_json::json!({
                "objectId": root_object_id,
                "ownProperties": true,
            }),
        )
        .await
    else {
        return HashSet::new();
    };
    let mut direct_arr_id: Option<String> = None;
    let mut pointer_arr_id: Option<String> = None;
    for entry in root_props
        .get("result")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let name = entry.get("name").and_then(Value::as_str);
        let oid = entry
            .get("value")
            .and_then(|v| v.get("objectId"))
            .and_then(Value::as_str)
            .map(str::to_string);
        match name {
            Some("direct") => direct_arr_id = oid,
            Some("pointer") => pointer_arr_id = oid,
            _ => {}
        }
    }

    let mut confirmed_ids = if let Some(ref oid) = direct_arr_id {
        extract_array_element_object_ids(client, session_id, oid).await
    } else {
        Vec::new()
    };

    if let Some(ref oid) = pointer_arr_id {
        let pointer_ids = extract_array_element_object_ids(client, session_id, oid).await;
        if !pointer_ids.is_empty() {
            let verified: Vec<String> = stream::iter(pointer_ids)
                .map(|elem_oid| {
                    let client = client.clone();
                    let sid = session_id.to_string();
                    async move {
                        if has_click_event_listener(&client, &sid, &elem_oid).await {
                            Some(elem_oid)
                        } else {
                            None
                        }
                    }
                })
                .buffer_unordered(8)
                .filter_map(|opt| async move { opt })
                .collect()
                .await;
            confirmed_ids.extend(verified);
        }
    }

    let backends: Vec<i64> = stream::iter(confirmed_ids)
        .map(|object_id| {
            let client = client.clone();
            let sid = session_id.to_string();
            async move {
                client
                    .send(
                        Some(&sid),
                        "DOM.describeNode",
                        serde_json::json!({ "objectId": object_id }),
                    )
                    .await
                    .ok()
                    .and_then(|res| {
                        res.get("node")
                            .and_then(|n| n.get("backendNodeId"))
                            .and_then(Value::as_i64)
                    })
            }
        })
        .buffer_unordered(8)
        .filter_map(|b| async move { b })
        .collect()
        .await;
    backends.into_iter().collect()
}

/// CDP-backed browser operations for one flattened page target.
#[derive(Clone)]
pub struct CdpDriver {
    client: Arc<CdpClient>,
    session_id: String,
}

impl CdpDriver {
    pub fn new(client: Arc<CdpClient>, session_id: String) -> Self {
        Self { client, session_id }
    }

    pub fn client(&self) -> Arc<CdpClient> {
        self.client.clone()
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub async fn set_resource_policy(&self, policy: ResourcePolicy) -> Result<(), CoreError> {
        network::set_blocked_urls(
            &self.client,
            &self.session_id,
            interceptor::blocked_patterns(policy),
        )
        .await?;
        Ok(())
    }

    pub async fn set_blocklist(&self, patterns: Vec<String>) -> Result<(), CoreError> {
        network::set_blocked_urls(&self.client, &self.session_id, patterns).await?;
        Ok(())
    }

    pub async fn evaluate(&self, expression: &str) -> Result<Value, CoreError> {
        let r = runtime::evaluate(&self.client, &self.session_id, expression).await?;
        if let Some(details) = r.exception_details {
            return Err(CoreError::NavigationFailed(format!(
                "page evaluation failed: {details}"
            )));
        }
        Ok(r.result.value.unwrap_or(Value::Null))
    }

    pub async fn get_full_ax_tree(&self) -> Result<Vec<accessibility::AXNode>, CoreError> {
        let (nodes, _) = self.get_full_ax_tree_with_sessions().await?;
        Ok(nodes)
    }

    /// Fetch the main AX tree and splice any child iframe AX trees into it.
    pub async fn get_full_ax_tree_with_sessions(
        &self,
    ) -> Result<(Vec<accessibility::AXNode>, HashMap<String, FrameSession>), CoreError> {
        let (main_res, clickable_backends, frame_res) = tokio::join!(
            accessibility::get_full_ax_tree(&self.client, &self.session_id),
            collect_clickable_non_aria_backends(&self.client, &self.session_id),
            page::get_frame_tree(&self.client, &self.session_id)
        );
        let mut all_nodes = main_res?.nodes;
        if !clickable_backends.is_empty() {
            for node in &mut all_nodes {
                if node
                    .backend_node_id
                    .is_some_and(|id| clickable_backends.contains(&id))
                {
                    let props = node.properties.get_or_insert_with(Vec::new);
                    if !props.iter().any(|p| p.name == "clickable") {
                        props.push(accessibility::AXProperty {
                            name: "clickable".to_string(),
                            value: Some(accessibility::AXValue {
                                type_: Some("boolean".to_string()),
                                value: Some(Value::Bool(true)),
                            }),
                        });
                    }
                }
            }
        }
        let mut node_sessions: HashMap<String, FrameSession> = HashMap::new();

        let Ok(ft) = frame_res else {
            return Ok((all_nodes, node_sessions));
        };
        let child_frames = collect_child_frames(&ft.frame_tree);
        if child_frames.is_empty() {
            return Ok((all_nodes, node_sessions));
        }

        // Cap child frame walks so ad-heavy pages cannot stall navigation.
        const MAX_CHILD_FRAMES: usize = 32;
        const MAX_FRAME_CONCURRENCY: usize = 4;
        struct FetchedFrame {
            idx: usize,
            owner_backend: Option<i64>,
            nodes: Vec<accessibility::AXNode>,
            oopif_sid: Option<String>,
        }
        let cached_targets: OnceCell<Option<Vec<target::TargetInfo>>> = OnceCell::new();

        let mut fetched: Vec<FetchedFrame> =
            stream::iter(child_frames.into_iter().take(MAX_CHILD_FRAMES).enumerate())
                .map(|(idx, cf)| {
                    let cached_targets = &cached_targets;
                    async move {
                        let (owner_res, ax_res) = tokio::join!(
                            dom::get_frame_owner(&self.client, &self.session_id, &cf.id),
                            accessibility::get_full_ax_tree_for_frame(
                                &self.client,
                                &self.session_id,
                                &cf.id,
                            )
                        );
                        let owner_backend = owner_res.ok().and_then(|r| r.backend_node_id);
                        let (nodes, oopif_sid) = match ax_res {
                            Ok(resp) => (resp.nodes, None),
                            Err(_) => self.fetch_oopif_ax_tree(&cf, cached_targets).await,
                        };
                        FetchedFrame {
                            idx,
                            owner_backend,
                            nodes,
                            oopif_sid,
                        }
                    }
                })
                .buffer_unordered(MAX_FRAME_CONCURRENCY)
                .collect()
                .await;

        // Restore document order regardless of which frame RPC finished first.
        fetched.sort_by_key(|f| f.idx);

        for frame in fetched {
            if frame.nodes.is_empty() {
                continue;
            }
            let prefix = format!("f{}:", frame.idx.saturating_add(1));
            if let Some(sid) = frame.oopif_sid {
                let fs = FrameSession {
                    session_id: sid,
                    owner_backend: frame.owner_backend,
                };
                for n in &frame.nodes {
                    node_sessions.insert(format!("{prefix}{}", n.node_id), fs.clone());
                }
            }
            splice_frame_nodes(&mut all_nodes, frame.nodes, &prefix, frame.owner_backend);
        }

        Ok((all_nodes, node_sessions))
    }

    async fn fetch_oopif_ax_tree(
        &self,
        frame: &page::FrameInfo,
        cached_targets: &OnceCell<Option<Vec<target::TargetInfo>>>,
    ) -> (Vec<accessibility::AXNode>, Option<String>) {
        let targets = cached_targets
            .get_or_init(|| async {
                target::get_targets(&self.client)
                    .await
                    .ok()
                    .map(|r| r.target_infos)
            })
            .await;
        let Some(targets) = targets.as_ref() else {
            return (Vec::new(), None);
        };
        let matched = targets
            .iter()
            .find(|t| t.kind == "iframe" && t.target_id == frame.id)
            .or_else(|| {
                if frame.url.is_empty() || frame.url == "about:blank" {
                    None
                } else {
                    targets
                        .iter()
                        .find(|t| t.kind == "iframe" && t.url == frame.url)
                }
            });
        let Some(t) = matched else {
            return (Vec::new(), None);
        };
        let Ok(att) = target::attach_to_target(&self.client, &t.target_id).await else {
            return (Vec::new(), None);
        };
        let sid = att.session_id;
        let _: Value = self
            .client
            .send(Some(&sid), "Accessibility.enable", serde_json::json!({}))
            .await
            .unwrap_or(Value::Null);
        match accessibility::get_full_ax_tree(&self.client, &sid).await {
            Ok(resp) => (resp.nodes, Some(sid)),
            Err(_) => (Vec::new(), None),
        }
    }

    pub async fn capture_screenshot(&self, full_page: bool) -> Result<String, CoreError> {
        let resp = page::capture_screenshot(&self.client, &self.session_id, full_page).await?;
        Ok(resp.data)
    }

    pub async fn capture_screenshot_clipped(
        &self,
        full_page: bool,
        clip: Option<page::Viewport>,
    ) -> Result<String, CoreError> {
        let resp =
            page::capture_screenshot_clipped(&self.client, &self.session_id, full_page, clip)
                .await?;
        Ok(resp.data)
    }
}

fn collect_child_frames(root: &page::FrameTree) -> Vec<page::FrameInfo> {
    let mut out = Vec::new();
    let mut stack: Vec<&page::FrameTree> = root.child_frames.iter().rev().collect();
    while let Some(ft) = stack.pop() {
        out.push(ft.frame.clone());
        for child in ft.child_frames.iter().rev() {
            stack.push(child);
        }
    }
    out
}

/// Prefix a child frame's `nodeId`/`childIds` and splice its content under the
/// matching `<iframe>` owner node in `parent_nodes`.
pub(crate) fn splice_frame_nodes(
    parent_nodes: &mut Vec<accessibility::AXNode>,
    mut child_nodes: Vec<accessibility::AXNode>,
    prefix: &str,
    owner_backend: Option<i64>,
) {
    if child_nodes.is_empty() {
        return;
    }
    let mut referenced: HashSet<String> = HashSet::new();
    for n in &mut child_nodes {
        n.node_id = format!("{prefix}{}", n.node_id);
        for cid in &mut n.child_ids {
            *cid = format!("{prefix}{cid}");
            referenced.insert(cid.clone());
        }
    }

    let has_owner = owner_backend.is_some_and(|owner_id| {
        parent_nodes
            .iter()
            .any(|n| n.backend_node_id == Some(owner_id))
    });

    // Collect the child frame's top-level content ids (children of its RootWebArea).
    let mut root_children: Vec<String> = Vec::new();
    for n in &mut child_nodes {
        if !referenced.contains(&n.node_id) {
            let role = n
                .role
                .as_ref()
                .map(|r| r.as_text().trim().to_ascii_lowercase())
                .unwrap_or_default();
            if role == "rootwebarea" {
                if has_owner {
                    // Drain child_ids so `parent_of` points to the parent `Iframe`
                    // node rather than this suppressed RootWebArea shell.
                    root_children.append(&mut n.child_ids);
                } else {
                    root_children.extend(n.child_ids.iter().cloned());
                }
                // Suppress the child RootWebArea shell so its <title> is not
                // emitted as duplicate text in the parent tree.
                n.ignored = true;
            } else {
                root_children.push(n.node_id.clone());
            }
        }
    }

    if let Some(owner_id) = owner_backend {
        if let Some(owner) = parent_nodes
            .iter_mut()
            .find(|n| n.backend_node_id == Some(owner_id))
        {
            owner.child_ids.extend(root_children);
        }
    }
    parent_nodes.extend(child_nodes);
}

#[cfg(test)]
mod tests {
    use super::splice_frame_nodes;
    use flashbrowse_cdp::protocol::accessibility::{AXNode, AXValue};

    fn ax(id: &str, role: &str, name: &str, backend: Option<i64>, children: &[&str]) -> AXNode {
        AXNode {
            node_id: id.to_string(),
            ignored: false,
            role: Some(AXValue {
                type_: Some("role".to_string()),
                value: Some(serde_json::Value::String(role.to_string())),
            }),
            name: Some(AXValue {
                type_: Some("computedString".to_string()),
                value: Some(serde_json::Value::String(name.to_string())),
            }),
            value: None,
            backend_node_id: backend,
            child_ids: children.iter().map(|s| s.to_string()).collect(),
            ignored_reasons: None,
            properties: None,
        }
    }

    #[test]
    fn splices_child_frame_under_iframe_owner_in_document_order() {
        let mut parent = vec![
            ax("1", "RootWebArea", "Parent", Some(1), &["2", "3", "4"]),
            ax("2", "StaticText", "Before", Some(2), &[]),
            ax("3", "Iframe", "Inner", Some(14), &[]),
            ax("4", "StaticText", "After", Some(4), &[]),
        ];
        let child = vec![
            ax("1", "RootWebArea", "ChildTitle", Some(1), &["2", "3"]),
            ax("2", "StaticText", "Inside frame", Some(20), &[]),
            ax("3", "button", "Frame btn", Some(21), &[]),
        ];
        splice_frame_nodes(&mut parent, child, "f1:", Some(14));

        let (out, map) = crate::tree::compress(&parent);
        let rendered = crate::tree::render("Parent", "https://x", &out);
        let before_pos = rendered.find("Before").unwrap();
        let inside_pos = rendered.find("Inside frame").unwrap();
        let btn_pos = rendered.find("[1] Button: \"Frame btn\"").unwrap();
        let after_pos = rendered.find("After").unwrap();
        assert!(before_pos < inside_pos && inside_pos < btn_pos && btn_pos < after_pos);
        assert_eq!(map.get(&1), Some(&21));
        // Child nodes spliced under `Iframe` (depth 1) must have depth 2.
        let inside_node = out.iter().find(|n| n.name == "Inside frame").unwrap();
        assert_eq!(inside_node.depth, 2);
    }

    #[test]
    fn splices_multiple_frames_in_document_order_after_index_sort() {
        let mut parent = vec![
            ax("1", "RootWebArea", "Parent", Some(1), &["2", "3"]),
            ax("2", "Iframe", "Frame1", Some(14), &[]),
            ax("3", "Iframe", "Frame2", Some(15), &[]),
        ];
        // Simulate frame 2 finishing before frame 1, then sorted by idx.
        let mut fetched = vec![
            (
                1usize,
                Some(15i64),
                vec![
                    ax("1", "RootWebArea", "F2", Some(1), &["2"]),
                    ax("2", "StaticText", "Second Frame", Some(30), &[]),
                ],
            ),
            (
                0usize,
                Some(14i64),
                vec![
                    ax("1", "RootWebArea", "F1", Some(1), &["2"]),
                    ax("2", "StaticText", "First Frame", Some(20), &[]),
                ],
            ),
        ];
        fetched.sort_by_key(|(idx, _, _)| *idx);
        for (idx, owner_backend, child_nodes) in fetched {
            splice_frame_nodes(
                &mut parent,
                child_nodes,
                &format!("f{}:", idx + 1),
                owner_backend,
            );
        }
        let (out, _) = crate::tree::compress(&parent);
        let rendered = crate::tree::render("Parent", "https://x", &out);
        let first = rendered.find("First Frame").unwrap();
        let second = rendered.find("Second Frame").unwrap();
        assert!(first < second, "{rendered}");
    }
}
