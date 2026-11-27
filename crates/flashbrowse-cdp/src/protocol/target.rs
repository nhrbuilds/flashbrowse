//! `Target` domain types (createTarget, attachToTarget).

use serde::{Deserialize, Serialize};

use crate::error::CdpError;
use crate::transport::CdpClient;

/// Params for `Target.createTarget`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateTargetParams {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headless: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub browser_context_id: Option<String>,
}

/// Response of `Target.createTarget`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateTargetResponse {
    pub target_id: String,
}

/// Params for `Target.attachToTarget`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachToTargetParams {
    pub target_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flatten: Option<bool>,
}

/// Response of `Target.attachToTarget`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachToTargetResponse {
    pub session_id: String,
}

/// Create a new target (tab) at `url`.
pub async fn create_target(
    client: &CdpClient,
    url: &str,
) -> Result<CreateTargetResponse, CdpError> {
    create_target_in_context(client, url, None).await
}

/// Create a new target (tab) at `url` inside an optional isolated `browserContextId`.
pub async fn create_target_in_context(
    client: &CdpClient,
    url: &str,
    browser_context_id: Option<&str>,
) -> Result<CreateTargetResponse, CdpError> {
    client
        .send_typed(
            None,
            "Target.createTarget",
            &CreateTargetParams {
                url: url.to_string(),
                headless: None,
                browser_context_id: browser_context_id.map(str::to_string),
            },
        )
        .await
}

/// Create an isolated incognito-like context that lives for this CDP connection.
pub async fn create_browser_context(client: &CdpClient) -> Result<String, CdpError> {
    let resp: serde_json::Value = client
        .send(
            None,
            "Target.createBrowserContext",
            serde_json::json!({ "disposeOnDetach": true }),
        )
        .await?;
    resp.get("browserContextId")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| CdpError::ProtocolError {
            method: "Target.createBrowserContext".to_string(),
            code: -1,
            message: "missing browserContextId".to_string(),
        })
}

/// Dispose an isolated browser context (`Target.disposeBrowserContext`).
pub async fn dispose_browser_context(
    client: &CdpClient,
    browser_context_id: &str,
) -> Result<(), CdpError> {
    let _: serde_json::Value = client
        .send(
            None,
            "Target.disposeBrowserContext",
            serde_json::json!({ "browserContextId": browser_context_id }),
        )
        .await?;
    Ok(())
}

/// Target metadata from `Target.getTargets`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetInfo {
    pub target_id: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub opener_id: Option<String>,
    #[serde(default)]
    pub browser_context_id: Option<String>,
}

/// Response of `Target.getTargets`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetTargetsResponse {
    pub target_infos: Vec<TargetInfo>,
}

/// List active CDP targets (used to discover out-of-process iframe targets).
pub async fn get_targets(client: &CdpClient) -> Result<GetTargetsResponse, CdpError> {
    client
        .send_typed(None, "Target.getTargets", &serde_json::json!({}))
        .await
}

pub async fn set_discover_targets(client: &CdpClient, discover: bool) -> Result<(), CdpError> {
    client
        .send(
            None,
            "Target.setDiscoverTargets",
            serde_json::json!({"discover": discover}),
        )
        .await?;
    Ok(())
}

/// Enable or disable auto-attaching to new targets in flatten mode.
pub async fn set_auto_attach(
    client: &CdpClient,
    auto_attach: bool,
    wait_for_debugger_on_start: bool,
    flatten: bool,
) -> Result<(), CdpError> {
    client
        .send(
            None,
            "Target.setAutoAttach",
            serde_json::json!({
                "autoAttach": auto_attach,
                "waitForDebuggerOnStart": wait_for_debugger_on_start,
                "flatten": flatten,
            }),
        )
        .await?;
    Ok(())
}

/// Payload for `Target.targetCreated` events.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetCreatedEvent {
    pub target_info: TargetInfo,
}

/// Payload for `Target.attachedToTarget` events.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachedToTargetEvent {
    pub session_id: String,
    pub target_info: TargetInfo,
}

pub async fn close_target(client: &CdpClient, target_id: &str) -> Result<bool, CdpError> {
    let response: serde_json::Value = client
        .send(
            None,
            "Target.closeTarget",
            serde_json::json!({"targetId": target_id}),
        )
        .await?;
    Ok(response
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false))
}

/// Attach to a target in flatten mode. Returns `sessionId` for all page-level calls.
pub async fn attach_to_target(
    client: &CdpClient,
    target_id: &str,
) -> Result<AttachToTargetResponse, CdpError> {
    client
        .send_typed(
            None,
            "Target.attachToTarget",
            &AttachToTargetParams {
                target_id: target_id.to_string(),
                flatten: Some(true),
            },
        )
        .await
}

/// Detach an active flatten-mode session from its target.
pub async fn detach_from_target(client: &CdpClient, session_id: &str) -> Result<(), CdpError> {
    client
        .send(
            None,
            "Target.detachFromTarget",
            serde_json::json!({"sessionId": session_id}),
        )
        .await?;
    Ok(())
}
