//! `Browser` domain types.

use serde::{Deserialize, Serialize};

use crate::error::CdpError;
use crate::transport::CdpClient;

/// Empty params for `Browser.getVersion`.
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GetVersionParams {}

/// Response of `Browser.getVersion`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetVersionResponse {
    pub protocol_version: String,
    pub product: String,
    #[serde(default)]
    pub revision: Option<String>,
    #[serde(default)]
    pub user_agent: Option<String>,
    #[serde(default)]
    pub js_version: Option<String>,
}

/// `Browser.getVersion` - verifies handshake against a fresh Chromium.
pub async fn get_version(client: &CdpClient) -> Result<GetVersionResponse, CdpError> {
    client
        .send_typed(None, "Browser.getVersion", &GetVersionParams::default())
        .await
}

/// Resolve a Chrome debugging endpoint (`http(s)://host[:port]`) or accept a
/// direct `ws(s)://` browser WebSocket URL.
pub async fn resolve_websocket_url(endpoint: &str) -> Result<String, CdpError> {
    crate::runtime::run(resolve_websocket_url_inner(endpoint)).await?
}

async fn resolve_websocket_url_inner(endpoint: &str) -> Result<String, CdpError> {
    if endpoint.starts_with("ws://") || endpoint.starts_with("wss://") {
        return Ok(endpoint.to_string());
    }
    let trimmed = endpoint.trim_end_matches('/');
    if !trimmed.starts_with("http://") && !trimmed.starts_with("https://") {
        return Err(CdpError::InvalidCdpEndpoint {
            endpoint: endpoint.to_string(),
            detail: "expected http(s) debugging address or ws(s) browser URL".into(),
        });
    }
    let url = format!("{trimmed}/json/version");
    let client =
        reqwest::Client::builder()
            .build()
            .map_err(|error| CdpError::InvalidCdpEndpoint {
                endpoint: endpoint.to_string(),
                detail: error.to_string(),
            })?;
    crate::transport::require_runtime()?;
    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|error| CdpError::InvalidCdpEndpoint {
            endpoint: endpoint.to_string(),
            detail: error.to_string(),
        })?;
    if !response.status().is_success() {
        return Err(CdpError::InvalidCdpEndpoint {
            endpoint: endpoint.to_string(),
            detail: format!("debug endpoint returned HTTP {}", response.status()),
        });
    }
    let value: serde_json::Value =
        response
            .json()
            .await
            .map_err(|error| CdpError::InvalidCdpEndpoint {
                endpoint: endpoint.to_string(),
                detail: format!("invalid /json/version response: {error}"),
            })?;
    value
        .get("webSocketDebuggerUrl")
        .and_then(serde_json::Value::as_str)
        .filter(|url| url.starts_with("ws://") || url.starts_with("wss://"))
        .map(str::to_string)
        .ok_or_else(|| CdpError::InvalidCdpEndpoint {
            endpoint: endpoint.to_string(),
            detail: "response has no browser WebSocket URL".into(),
        })
}

/// Configure browser-context downloads and enable Browser download events.
pub async fn set_download_behavior(
    client: &CdpClient,
    download_path: &str,
) -> Result<(), CdpError> {
    let _: serde_json::Value = client
        .send(
            None,
            "Browser.setDownloadBehavior",
            serde_json::json!({
                "behavior": "allow",
                "downloadPath": download_path,
                "eventsEnabled": true,
            }),
        )
        .await?;
    Ok(())
}

/// Grant browser permissions (e.g. `geolocation`, `notifications`, `clipboardReadWrite`).
pub async fn grant_permissions(
    client: &CdpClient,
    origin: Option<&str>,
    permissions: &[String],
    browser_context_id: Option<&str>,
) -> Result<(), CdpError> {
    let mut params =
        serde_json::Map::from_iter([("permissions".into(), serde_json::to_value(permissions)?)]);
    if let Some(o) = origin {
        params.insert("origin".into(), serde_json::Value::String(o.to_string()));
    }
    if let Some(cid) = browser_context_id {
        params.insert(
            "browserContextId".into(),
            serde_json::Value::String(cid.to_string()),
        );
    }
    let _: serde_json::Value = client
        .send(
            None,
            "Browser.grantPermissions",
            serde_json::Value::Object(params),
        )
        .await?;
    Ok(())
}

/// Reset granted permissions for the default or isolated browser context.
pub async fn reset_permissions(
    client: &CdpClient,
    browser_context_id: Option<&str>,
) -> Result<(), CdpError> {
    let mut params = serde_json::Map::new();
    if let Some(cid) = browser_context_id {
        params.insert(
            "browserContextId".into(),
            serde_json::Value::String(cid.to_string()),
        );
    }
    let _: serde_json::Value = client
        .send(
            None,
            "Browser.resetPermissions",
            serde_json::Value::Object(params),
        )
        .await?;
    Ok(())
}
