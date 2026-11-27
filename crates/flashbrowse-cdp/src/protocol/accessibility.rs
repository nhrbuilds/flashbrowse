//! `Accessibility` domain types (getFullAXTree, AXNode, AXValue).
//!
//! Field names mirror the CDP `Accessibility.AXNode` schema exactly:
//! `nodeId, ignored, ignoredReasons, role, chromeRole, name, description,
//!  value, properties, parentId, childIds, backendDOMNodeId, frameId`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::CdpError;
use crate::transport::CdpClient;

/// AXTree node value. CDP reuses `{type, value}` pairs; `value` is untyped.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AXValue {
    #[serde(rename = "type", default)]
    pub type_: Option<String>,
    #[serde(default)]
    pub value: Option<Value>,
}

impl AXValue {
    /// String payload (`value` may be string/number/bool).
    pub fn as_text(&self) -> String {
        match self.value.as_ref() {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(s)) => s.clone(),
            Some(Value::Number(n)) => n.to_string(),
            Some(Value::Bool(b)) => b.to_string(),
            Some(other) => {
                if let Err(error) = crate::transport::validate_json_depth(other) {
                    tracing::warn!(%error, "accessibility value is too deeply nested");
                    return "[invalid accessibility value: JSON nesting exceeds the limit]"
                        .to_string();
                }
                other.to_string()
            }
        }
    }

    /// Boolean payload.
    ///
    /// Chromium is inconsistent: `readonly` arrives as a JSON `true`, while
    /// `checked` arrives as the string `"true"`. Treating only the JSON bool
    /// as true silently dropped every checked checkbox and radio.
    pub fn as_bool(&self) -> bool {
        match self.value.as_ref() {
            Some(Value::Bool(b)) => *b,
            Some(Value::String(s)) => s.eq_ignore_ascii_case("true"),
            _ => false,
        }
    }

    /// Integer payload.
    pub fn as_i64(&self) -> Option<i64> {
        match self.value.as_ref() {
            Some(Value::Number(n)) => n.as_i64(),
            Some(Value::String(s)) => s.parse::<i64>().ok(),
            _ => None,
        }
    }
}

/// AXTree node property (states like `level`, `checked`, `disabled`, `value`).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AXProperty {
    pub name: String,
    #[serde(default)]
    pub value: Option<AXValue>,
}

/// Reason a node was excluded from the AX tree.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AXIgnoredReason {
    pub name: String,
    #[serde(default)]
    pub value: Option<AXValue>,
}

/// Single accessibility-tree node returned by `Accessibility.getFullAXTree`.
///
/// `Serialize` exists for the raw dump (`dump --format raw`) and for tests
/// that need a faithful copy of what Chromium sent.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AXNode {
    pub node_id: String,
    #[serde(default)]
    pub ignored: bool,
    #[serde(default)]
    pub role: Option<AXValue>,
    #[serde(default)]
    pub name: Option<AXValue>,
    /// Node value (input value, selected option text, ...).
    #[serde(default)]
    pub value: Option<AXValue>,
    /// CDP sends `backendDOMNodeId` (not camelCase).
    #[serde(rename = "backendDOMNodeId", alias = "backendNodeId", default)]
    pub backend_node_id: Option<i64>,
    #[serde(default)]
    pub child_ids: Vec<String>,
    #[serde(default)]
    pub ignored_reasons: Option<Vec<AXIgnoredReason>>,
    #[serde(default)]
    pub properties: Option<Vec<AXProperty>>,
}

impl AXNode {
    /// Property lookup by name.
    pub fn property(&self, name: &str) -> Option<&AXProperty> {
        self.properties.as_ref()?.iter().find(|p| p.name == name)
    }

    /// Boolean property value.
    pub fn prop_bool(&self, name: &str) -> bool {
        self.property(name)
            .and_then(|p| p.value.as_ref())
            .map(|v| v.as_bool())
            .unwrap_or(false)
    }

    /// String property value.
    pub fn prop_string(&self, name: &str) -> Option<String> {
        self.property(name)
            .and_then(|p| p.value.as_ref())
            .map(|v| v.as_text())
            .filter(|s| !s.is_empty())
    }

    /// Integer property value.
    pub fn prop_i64(&self, name: &str) -> Option<i64> {
        self.property(name)
            .and_then(|p| p.value.as_ref())
            .and_then(|v| v.as_i64())
    }
}

/// Params for `Accessibility.getFullAXTree`.
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GetFullAXTreeParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub depth: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_depth: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_id: Option<String>,
}

/// Response of `Accessibility.getFullAXTree`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetFullAXTreeResponse {
    pub nodes: Vec<AXNode>,
}

/// `Accessibility.getFullAXTree` - the compression engine input.
pub async fn get_full_ax_tree(
    client: &CdpClient,
    session_id: &str,
) -> Result<GetFullAXTreeResponse, CdpError> {
    client
        .send_typed(
            Some(session_id),
            "Accessibility.getFullAXTree",
            &GetFullAXTreeParams::default(),
        )
        .await
}

/// `Accessibility.getFullAXTree` scoped to a specific child `frameId`.
pub async fn get_full_ax_tree_for_frame(
    client: &CdpClient,
    session_id: &str,
    frame_id: &str,
) -> Result<GetFullAXTreeResponse, CdpError> {
    client
        .send_typed(
            Some(session_id),
            "Accessibility.getFullAXTree",
            &GetFullAXTreeParams {
                frame_id: Some(frame_id.to_string()),
                ..Default::default()
            },
        )
        .await
}

#[cfg(test)]
mod tests {
    #[test]
    fn deeply_nested_accessibility_value_does_not_overflow() {
        let deep = (0..10_000).fold(serde_json::Value::Null, |value, _| {
            serde_json::Value::Array(vec![value])
        });
        let mut value = super::AXValue {
            type_: None,
            value: Some(deep),
        };
        assert!(value.as_text().contains("JSON nesting exceeds"));
        super::super::super::transport::drop_json_iteratively(value.value.take().unwrap());
    }
}
