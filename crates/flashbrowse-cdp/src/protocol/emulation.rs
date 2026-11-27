//! `Emulation` domain types (viewport, device metrics, timezone, locale, geolocation).

use serde::Serialize;
use serde_json::{json, Value};

use crate::error::CdpError;
use crate::transport::CdpClient;

/// Params for `Emulation.setDeviceMetricsOverride`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceMetricsParams {
    pub width: i32,
    pub height: i32,
    pub device_scale_factor: f64,
    pub mobile: bool,
}

/// `Emulation.setDeviceMetricsOverride`.
pub async fn set_device_metrics_override(
    client: &CdpClient,
    session_id: &str,
    params: &DeviceMetricsParams,
) -> Result<(), CdpError> {
    let _: Value = client
        .send_typed(
            Some(session_id),
            "Emulation.setDeviceMetricsOverride",
            params,
        )
        .await?;
    Ok(())
}

/// `Emulation.clearDeviceMetricsOverride`.
pub async fn clear_device_metrics_override(
    client: &CdpClient,
    session_id: &str,
) -> Result<(), CdpError> {
    let _: Value = client
        .send(
            Some(session_id),
            "Emulation.clearDeviceMetricsOverride",
            json!({}),
        )
        .await?;
    Ok(())
}

/// `Emulation.setTimezoneOverride`.
pub async fn set_timezone_override(
    client: &CdpClient,
    session_id: &str,
    timezone_id: &str,
) -> Result<(), CdpError> {
    let _: Value = client
        .send(
            Some(session_id),
            "Emulation.setTimezoneOverride",
            json!({ "timezoneId": timezone_id }),
        )
        .await?;
    Ok(())
}

/// `Emulation.setLocaleOverride`.
pub async fn set_locale_override(
    client: &CdpClient,
    session_id: &str,
    locale: &str,
) -> Result<(), CdpError> {
    let _: Value = client
        .send(
            Some(session_id),
            "Emulation.setLocaleOverride",
            json!({ "locale": locale }),
        )
        .await?;
    Ok(())
}

/// `Emulation.setGeolocationOverride`.
pub async fn set_geolocation_override(
    client: &CdpClient,
    session_id: &str,
    latitude: f64,
    longitude: f64,
    accuracy: f64,
) -> Result<(), CdpError> {
    let _: Value = client
        .send(
            Some(session_id),
            "Emulation.setGeolocationOverride",
            json!({
                "latitude": latitude,
                "longitude": longitude,
                "accuracy": accuracy,
            }),
        )
        .await?;
    Ok(())
}

/// `Emulation.clearGeolocationOverride`.
pub async fn clear_geolocation_override(
    client: &CdpClient,
    session_id: &str,
) -> Result<(), CdpError> {
    let _: Value = client
        .send(
            Some(session_id),
            "Emulation.clearGeolocationOverride",
            json!({}),
        )
        .await?;
    Ok(())
}
