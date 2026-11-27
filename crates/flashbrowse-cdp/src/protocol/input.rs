//! `Input` domain types (dispatchMouseEvent, dispatchKeyEvent, insertText).

use serde::Serialize;

use crate::error::CdpError;
use crate::transport::CdpClient;

/// Params for `Input.dispatchMouseEvent`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DispatchMouseEventParams {
    #[serde(rename = "type")]
    pub type_: String,
    pub x: f64,
    pub y: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub button: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub buttons: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub click_count: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta_x: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta_y: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modifiers: Option<i64>,
}

/// Params for `Input.insertText`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InsertTextParams {
    pub text: String,
}

/// Params for `Input.dispatchKeyEvent`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DispatchKeyEventParams {
    #[serde(rename = "type")]
    pub type_: String,
    pub key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modifiers: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub windows_virtual_key_code: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_virtual_key_code: Option<i64>,
}

/// Dispatch a mouse event (`mouseMoved` | `mousePressed` | `mouseReleased` | `mouseWheel`).
pub async fn dispatch_mouse_event(
    client: &CdpClient,
    session_id: &str,
    params: &DispatchMouseEventParams,
) -> Result<(), CdpError> {
    let _: serde_json::Value = client
        .send_typed(Some(session_id), "Input.dispatchMouseEvent", params)
        .await?;
    Ok(())
}

/// `Input.insertText` - synthetic text entry (fill).
pub async fn insert_text(client: &CdpClient, session_id: &str, text: &str) -> Result<(), CdpError> {
    let _: serde_json::Value = client
        .send_typed(
            Some(session_id),
            "Input.insertText",
            &InsertTextParams {
                text: text.to_string(),
            },
        )
        .await?;
    Ok(())
}

/// `Input.dispatchKeyEvent` (press_key, Ctrl+A clear, etc).
pub async fn dispatch_key_event(
    client: &CdpClient,
    session_id: &str,
    params: &DispatchKeyEventParams,
) -> Result<(), CdpError> {
    let _: serde_json::Value = client
        .send_typed(Some(session_id), "Input.dispatchKeyEvent", params)
        .await?;
    Ok(())
}
