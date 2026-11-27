//! `Network` domain types (enable, setBlockedURLs, setExtraHTTPHeaders).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::error::CdpError;
use crate::transport::CdpClient;

/// Params for `Network.setBlockedURLs`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetBlockedURLsParams {
    pub urls: Vec<String>,
}

/// `Network.enable`.
pub async fn enable(client: &CdpClient, session_id: &str) -> Result<(), CdpError> {
    let _: serde_json::Value = client
        .send(Some(session_id), "Network.enable", serde_json::json!({}))
        .await?;
    Ok(())
}

/// `Network.domain` enable without `sessionId` (browser-level)? Not used -
/// session enable is sufficient for flattened tabs.
pub async fn set_blocked_urls(
    client: &CdpClient,
    session_id: &str,
    urls: Vec<String>,
) -> Result<(), CdpError> {
    let _: serde_json::Value = client
        .send_typed(
            Some(session_id),
            "Network.setBlockedURLs",
            &SetBlockedURLsParams { urls },
        )
        .await?;
    Ok(())
}

/// `Network.setExtraHTTPHeaders` - send additional HTTP headers on every request in this session.
pub async fn set_extra_http_headers(
    client: &CdpClient,
    session_id: &str,
    headers: &HashMap<String, String>,
) -> Result<(), CdpError> {
    let _: serde_json::Value = client
        .send(
            Some(session_id),
            "Network.setExtraHTTPHeaders",
            serde_json::json!({ "headers": headers }),
        )
        .await?;
    Ok(())
}

/// Response of `Network.getResponseBody`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetResponseBodyResponse {
    pub body: String,
    #[serde(default)]
    pub base64_encoded: bool,
}

/// `Network.getResponseBody` - retrieve the captured body for a `requestId`.
pub async fn get_response_body(
    client: &CdpClient,
    session_id: &str,
    request_id: &str,
) -> Result<GetResponseBodyResponse, CdpError> {
    client
        .send_typed(
            Some(session_id),
            "Network.getResponseBody",
            &serde_json::json!({ "requestId": request_id }),
        )
        .await
}
