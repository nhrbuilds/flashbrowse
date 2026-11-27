//! Live checks: resource policy does not change tree bytes; screenshots return data.

use std::path::PathBuf;

use flashbrowse_core::{BrowserSession, ResourcePolicy, ScreenshotOptions, SessionConfig};

fn fixture_file_url() -> String {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    format!("file://{}", dir.join("index.html").display())
}

#[tokio::test]
async fn tree_stable_across_resource_policies() {
    if flashbrowse_cdp::find_chrome(None).is_err() {
        eprintln!("SKIP: no Chromium binary found");
        return;
    }
    let url = fixture_file_url();
    let mut agent = BrowserSession::launch(SessionConfig {
        resource_policy: ResourcePolicy::AgentDefault,
        ..Default::default()
    })
    .await
    .expect("launch");
    let tree_agent = agent.open(&url).await.expect("open agent");
    agent.shutdown().await;

    let mut full = BrowserSession::launch(SessionConfig {
        resource_policy: ResourcePolicy::FullRender,
        ..Default::default()
    })
    .await
    .expect("launch full");
    let tree_full = full.open(&url).await.expect("open full");
    full.shutdown().await;

    assert_eq!(
        tree_agent, tree_full,
        "resource policy must not change compressed tree"
    );
}

#[tokio::test]
async fn screenshot_returns_png_after_open() {
    if flashbrowse_cdp::find_chrome(None).is_err() {
        eprintln!("SKIP: no Chromium binary found");
        return;
    }
    let url = fixture_file_url();
    let mut session = BrowserSession::launch(SessionConfig::default())
        .await
        .expect("launch");
    session.open(&url).await.expect("open");
    let png_b64 = session
        .screenshot(ScreenshotOptions {
            full_page: false,
            prepare_lazy_content: true,
            timeout_ms: 10_000,
            ..Default::default()
        })
        .await
        .expect("screenshot");
    assert!(
        png_b64.len() > 500,
        "screenshot payload too small ({})",
        png_b64.len()
    );
    session.shutdown().await;
}
