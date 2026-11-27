//! Verifies raw WebSocket connection to headless Chrome.
//! Requires a system Chromium binary - skipped if none is found.

use flashbrowse_cdp::{
    protocol::{browser, page, target},
    CdpClient, LaunchOptions,
};

#[tokio::test]
async fn test_cdp_connection_and_get_version() {
    // Skip gracefully when Chrome is absent (CI without browser).
    if flashbrowse_cdp::find_chrome(None).is_err() {
        eprintln!("SKIP: no Chromium binary found");
        return;
    }
    let mut chrome = flashbrowse_cdp::launch_chrome(LaunchOptions {
        // This test verifies the WebSocket path. Explicitly select it because
        // ephemeral launches otherwise prefer the POSIX pipe transport.
        extra_args: vec!["--remote-debugging-port=0".to_string()],
        ..Default::default()
    })
    .await
    .expect("launch chrome");

    let client = CdpClient::connect_chrome(&mut chrome)
        .await
        .expect("connect");

    // Browser.getVersion - proves handshake + protocolVersion + product.
    let ver = browser::get_version(&client).await.expect("getVersion");
    assert!(!ver.protocol_version.is_empty(), "protocolVersion empty");
    assert!(!ver.product.is_empty(), "product empty");

    // Flatten-mode session: createTarget → attachToTarget → Page.navigate.
    let t = target::create_target(&client, "about:blank")
        .await
        .expect("createTarget");
    let s = target::attach_to_target(&client, &t.target_id)
        .await
        .expect("attachToTarget");
    assert!(!s.session_id.is_empty());

    page::enable(&client, &s.session_id)
        .await
        .expect("Page.enable");
    let nav = page::navigate(&client, &s.session_id, "about:blank")
        .await
        .expect("Page.navigate");
    assert!(!nav.frame_id.is_empty());
    client.shutdown();
    chrome.shutdown().await;

    // Persistent launch retains a WebSocket endpoint for reattachment.
    let mut persistent = flashbrowse_cdp::launch_chrome(LaunchOptions {
        persistent: true,
        ..Default::default()
    })
    .await
    .expect("launch persistent chrome");
    let ws = persistent.browser_ws_url().to_string();
    assert!(ws.starts_with("ws://127.0.0.1:"), "ws url: {ws}");
    let ws_client = CdpClient::connect(&ws).await.expect("ws reattach");
    let ws_ver = browser::get_version(&ws_client)
        .await
        .expect("ws getVersion");
    assert!(!ws_ver.product.is_empty());
    ws_client.shutdown();
    persistent.shutdown().await;
}
