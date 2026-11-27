//! `Page` domain types (navigate, addScriptToEvaluateOnNewDocument, lifecycle).

use serde::{Deserialize, Serialize};

use crate::error::CdpError;
use crate::transport::CdpClient;

/// Params for `Page.navigate`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NavigateParams {
    pub url: String,
}

/// Response of `Page.navigate`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NavigateResponse {
    pub frame_id: String,
    #[serde(default)]
    pub loader_id: Option<String>,
}

/// Params for `Page.addScriptToEvaluateOnNewDocument`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AddScriptParams {
    pub source: String,
}

/// Response of `Page.addScriptToEvaluateOnNewDocument`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddScriptResponse {
    #[serde(default)]
    pub identifier: Option<String>,
}

/// Clip region for `Page.captureScreenshot`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Viewport {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub scale: f64,
}

/// Params for `Page.captureScreenshot`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureScreenshotParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capture_beyond_viewport: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clip: Option<Viewport>,
}

/// Response of `Page.captureScreenshot` (base64-encoded PNG when format=png).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureScreenshotResponse {
    pub data: String,
}

/// `Page.navigate` within a flattened session.
pub async fn navigate(
    client: &CdpClient,
    session_id: &str,
    url: &str,
) -> Result<NavigateResponse, CdpError> {
    client
        .send_typed(
            Some(session_id),
            "Page.navigate",
            &NavigateParams {
                url: url.to_string(),
            },
        )
        .await
}

/// Entry from `Page.getNavigationHistory`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NavigationEntry {
    pub id: i64,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub title: String,
}

/// Response of `Page.getNavigationHistory`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetNavigationHistoryResponse {
    pub current_index: usize,
    #[serde(default)]
    pub entries: Vec<NavigationEntry>,
}

/// `Page.getNavigationHistory` for back/forward navigation.
pub async fn get_navigation_history(
    client: &CdpClient,
    session_id: &str,
) -> Result<GetNavigationHistoryResponse, CdpError> {
    client
        .send_typed(
            Some(session_id),
            "Page.getNavigationHistory",
            &serde_json::json!({}),
        )
        .await
}

/// `Page.navigateToHistoryEntry`.
pub async fn navigate_to_history_entry(
    client: &CdpClient,
    session_id: &str,
    entry_id: i64,
) -> Result<(), CdpError> {
    let _: serde_json::Value = client
        .send(
            Some(session_id),
            "Page.navigateToHistoryEntry",
            serde_json::json!({ "entryId": entry_id }),
        )
        .await?;
    Ok(())
}

/// `Page.reload`.
pub async fn reload(
    client: &CdpClient,
    session_id: &str,
    ignore_cache: bool,
) -> Result<(), CdpError> {
    let _: serde_json::Value = client
        .send(
            Some(session_id),
            "Page.reload",
            serde_json::json!({ "ignoreCache": ignore_cache }),
        )
        .await?;
    Ok(())
}

/// `Page.addScriptToEvaluateOnNewDocument` (stealth injection before page scripts).
pub async fn add_script_on_new_document(
    client: &CdpClient,
    session_id: &str,
    source: &str,
) -> Result<AddScriptResponse, CdpError> {
    client
        .send_typed(
            Some(session_id),
            "Page.addScriptToEvaluateOnNewDocument",
            &AddScriptParams {
                source: source.to_string(),
            },
        )
        .await
}

/// `Page.enable`.
pub async fn enable(client: &CdpClient, session_id: &str) -> Result<(), CdpError> {
    let _: serde_json::Value = client
        .send(Some(session_id), "Page.enable", serde_json::json!({}))
        .await?;
    // Lifecycle events are opt-in: without this, `Page.lifecycleEvent` never
    // fires and every navigation would wait for the full timeout.
    let _: serde_json::Value = client
        .send(
            Some(session_id),
            "Page.setLifecycleEventsEnabled",
            serde_json::json!({ "enabled": true }),
        )
        .await?;
    Ok(())
}

/// Set this page's download destination and enable progress notifications.
pub async fn set_download_behavior(
    client: &CdpClient,
    session_id: &str,
    download_path: &str,
) -> Result<(), CdpError> {
    let _: serde_json::Value = client
        .send(
            Some(session_id),
            "Page.setDownloadBehavior",
            serde_json::json!({
                "behavior": "allow",
                "downloadPath": download_path,
                "eventsEnabled": true,
            }),
        )
        .await?;
    Ok(())
}

/// Frame metadata in `Page.getFrameTree`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FrameInfo {
    pub id: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub url: String,
}

/// Recursive frame tree node returned by `Page.getFrameTree`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FrameTree {
    pub frame: FrameInfo,
    #[serde(default)]
    pub child_frames: Vec<FrameTree>,
}

/// Response of `Page.getFrameTree`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetFrameTreeResponse {
    pub frame_tree: FrameTree,
}

/// `Page.getFrameTree` - inspect main and child frames.
pub async fn get_frame_tree(
    client: &CdpClient,
    session_id: &str,
) -> Result<GetFrameTreeResponse, CdpError> {
    client
        .send_typed(
            Some(session_id),
            "Page.getFrameTree",
            &serde_json::json!({}),
        )
        .await
}

/// `Page.captureScreenshot`.
pub async fn capture_screenshot(
    client: &CdpClient,
    session_id: &str,
    full_page: bool,
) -> Result<CaptureScreenshotResponse, CdpError> {
    capture_screenshot_clipped(client, session_id, full_page, None).await
}

/// `Page.captureScreenshot` with an optional element clip viewport.
pub async fn capture_screenshot_clipped(
    client: &CdpClient,
    session_id: &str,
    full_page: bool,
    clip: Option<Viewport>,
) -> Result<CaptureScreenshotResponse, CdpError> {
    client
        .send_typed(
            Some(session_id),
            "Page.captureScreenshot",
            &CaptureScreenshotParams {
                format: Some("png".to_string()),
                capture_beyond_viewport: Some(full_page),
                clip,
            },
        )
        .await
}

/// Params for `Page.printToPDF`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrintToPdfParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub landscape: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub print_background: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scale: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paper_width: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paper_height: Option<f64>,
}

/// Response of `Page.printToPDF` (base64-encoded PDF bytes).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrintToPdfResponse {
    pub data: String,
}

/// `Page.printToPDF`.
pub async fn print_to_pdf(
    client: &CdpClient,
    session_id: &str,
    params: &PrintToPdfParams,
) -> Result<PrintToPdfResponse, CdpError> {
    client
        .send_typed(Some(session_id), "Page.printToPDF", params)
        .await
}
