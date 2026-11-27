//! MCP wire types (JSON-RPC 2.0 + MCP 2025-06-18).

use serde::{Deserialize, Serialize};
use serde_json::Value;

const LATEST_VERSION: &str = "2025-06-18";

/// Supported protocol versions (highest first).
pub const SUPPORTED_VERSIONS: &[&str] = &[LATEST_VERSION, "2025-03-26", "2024-11-05"];

/// Negotiate the highest mutually supported version.
pub fn negotiate(client_version: Option<&str>) -> String {
    match client_version {
        None => LATEST_VERSION.to_string(),
        Some(v) => {
            if SUPPORTED_VERSIONS.contains(&v) {
                v.to_string()
            } else {
                // Unknown/newer client version: offer latest we speak.
                LATEST_VERSION.to_string()
            }
        }
    }
}

/// Incoming JSON-RPC request/notification (`id` absent = notification).
#[derive(Debug, Deserialize)]
pub struct JsonRpcRequest {
    #[serde(default)]
    pub jsonrpc: Option<String>,
    #[serde(default)]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
}

impl JsonRpcRequest {
    /// `true` when this is a notification (no `id` → never respond).
    pub fn is_notification(&self) -> bool {
        self.id.is_none()
    }
}

/// Outgoing JSON-RPC success response.
#[derive(Debug, Serialize)]
pub struct JsonRpcResult {
    pub jsonrpc: String,
    pub id: Value,
    pub result: Value,
}

/// Outgoing JSON-RPC error response.
#[derive(Debug, Serialize)]
pub struct JsonRpcErrorResponse {
    pub jsonrpc: String,
    pub id: Value,
    pub error: JsonRpcError,
}

/// JSON-RPC error object.
#[derive(Debug, Serialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl JsonRpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }
}

/// Tool result payload (`{content, isError?}`). Content items are JSON:
/// `{"type":"text","text":...}` or `{"type":"image","data":...,"mimeType":"image/png"}`.
#[derive(Debug, Serialize)]
pub struct ToolResult {
    pub content: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "isError")]
    pub is_error: Option<bool>,
}

impl ToolResult {
    pub fn text(text: String) -> Self {
        Self {
            content: vec![serde_json::json!({"type": "text", "text": text})],
            is_error: None,
        }
    }

    pub fn error(message: String) -> Self {
        Self {
            content: vec![serde_json::json!({"type": "text", "text": message})],
            is_error: Some(true),
        }
    }

    pub fn image(data: String) -> Self {
        Self {
            content: vec![serde_json::json!({
                "type": "image",
                "data": data,
                "mimeType": "image/png"
            })],
            is_error: None,
        }
    }
}

/// MCP error type for transport/dispatch failures.
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("parse error: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid params: {0}")]
    InvalidParams(String),
    #[error("internal error: {0}")]
    Internal(String),
}

#[cfg(test)]
mod tests {
    use super::negotiate;

    #[test]
    fn negotiates_highest_mutual() {
        assert_eq!(negotiate(Some("2024-11-05")), "2024-11-05");
        assert_eq!(negotiate(Some("2025-06-18")), "2025-06-18");
        assert_eq!(negotiate(Some("2099-01-01")), "2025-06-18");
        assert_eq!(negotiate(None), "2025-06-18");
    }
}
