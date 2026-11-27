//! Screenshot regression tests.
//!
//! These cover the failure modes found in the screenshot rework:
//!   * a screenshot must NOT burn the full prepare timeout on a page whose
//!     images are blocked (the readiness check used to require
//!     `naturalWidth > 0`, which a permanently broken image never reaches);
//!   * lifting the blocklist must actually make the image load (Chrome does
//!     not retry a blocked request on its own - the request has to be
//!     re-issued);
//!   * the agent's scroll position must survive the lazy-content prepare.

use std::path::PathBuf;

use flashbrowse_core::{
    BrowserSession, ResourcePolicy, ScreenshotOptions, SessionConfig, SCREENSHOT_PREPARE_MS_MAX,
};

/// A 4x4 blue PNG (small, but unmistakably not white).
const BLUE_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x04, 0x08, 0x02, 0x00, 0x00, 0x00, 0x2C, 0x60, 0xF7,
    0xFF, 0x00, 0x00, 0x00, 0x18, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];

/// Minimal HTTP server that serves one HTML page and the PNG above.
fn spawn_server() -> u16 {
    spawn_server_with(
        "<html><body><h1>Shot</h1><img src=\"/p.png\" alt=\"p\"><p>below</p></body></html>",
    )
}

/// HTTP server that serves `html` on `/` and the blue PNG on `/p.png`.
fn spawn_server_with(html: &str) -> u16 {
    use std::io::{BufRead, BufReader, Write};
    use std::net::{Shutdown, TcpListener};
    use std::time::Duration;

    let html = html.to_string();
    let png = BLUE_PNG.to_vec();

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let html = html.clone();
            let png = png.clone();
            std::thread::spawn(move || {
                let mut stream = stream;
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let mut path = String::from("/");
                if let Ok(cloned) = stream.try_clone() {
                    let mut reader = BufReader::new(cloned);
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    if let Some(rest) = line.strip_prefix("GET ") {
                        path = rest.split_whitespace().next().unwrap_or("/").to_string();
                    }
                    while line != "\r\n" && line != "\n" {
                        line.clear();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 {
                            break;
                        }
                    }
                }
                let (body, ctype): (Vec<u8>, &str) = if path.starts_with("/p.png") {
                    (png, "image/png")
                } else {
                    (html.into_bytes(), "text/html")
                };
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&body);
                let _ = stream.flush();
                let _ = stream.shutdown(Shutdown::Write);
            });
        }
    });
    port
}

fn fixture_file_url() -> String {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    format!("file://{}", dir.join("index.html").display())
}

fn no_chrome() -> bool {
    flashbrowse_cdp::find_chrome(None).is_err()
}

/// A screenshot of a page whose image is blocked must not wait for the
/// full timeout.
///
/// The image is blocked at the network layer, so it can never load; a
/// readiness check that requires `naturalWidth > 0` would spin until the
/// deadline on EVERY such page.
#[tokio::test]
async fn screenshot_does_not_burn_the_timeout_on_a_blocked_image() {
    if no_chrome() {
        eprintln!("SKIP: no Chromium binary found");
        return;
    }
    let port = spawn_server();
    let url = format!("http://127.0.0.1:{port}/");

    let mut session = BrowserSession::launch(SessionConfig {
        resource_policy: ResourcePolicy::AgentDefault,
        ..Default::default()
    })
    .await
    .expect("launch");
    session.open(&url).await.expect("open");

    let started = std::time::Instant::now();
    let png = session
        .screenshot(ScreenshotOptions {
            full_page: false,
            prepare_lazy_content: true,
            timeout_ms: 8_000,
            ..Default::default()
        })
        .await
        .expect("screenshot");
    let elapsed = started.elapsed();

    assert!(!png.is_empty(), "screenshot must return data");
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "screenshot took {elapsed:?} - it waited for an image that can never load"
    );
    session.shutdown().await;
}

/// Lifting the blocklist must make the image actually load.
///
/// `Network.setBlockedURLs` only affects future requests and Chrome never
/// retries a blocked one, so the image has to be re-requested explicitly.
/// The proof: an agent-default capture must be byte-identical to a
/// full-render capture of the same page. Before the fix the agent-default
/// capture showed a broken-image glyph.
#[tokio::test]
async fn blocked_image_is_rendered_after_the_lift() {
    if no_chrome() {
        eprintln!("SKIP: no Chromium binary found");
        return;
    }
    let port = spawn_server();
    let url = format!("http://127.0.0.1:{port}/");

    async fn capture(url: &str, policy: ResourcePolicy) -> String {
        let mut session = BrowserSession::launch(SessionConfig {
            resource_policy: policy,
            ..Default::default()
        })
        .await
        .expect("launch");
        session.open(url).await.expect("open");
        let png = session
            .screenshot(ScreenshotOptions {
                full_page: false,
                prepare_lazy_content: false,
                timeout_ms: 3_000,
                ..Default::default()
            })
            .await
            .expect("screenshot");
        session.shutdown().await;
        png
    }

    let agent_default = capture(&url, ResourcePolicy::AgentDefault).await;
    let full_render = capture(&url, ResourcePolicy::FullRender).await;

    assert_eq!(
        agent_default, full_render,
        "the agent-default capture must show the same pixels as full-render \
         (the blocked image was not re-requested)"
    );
}

/// The AX tree must not change because of the screenshot's blocklist lift.
#[tokio::test]
async fn screenshot_does_not_change_the_tree() {
    if no_chrome() {
        eprintln!("SKIP: no Chromium binary found");
        return;
    }
    let port = spawn_server();
    let url = format!("http://127.0.0.1:{port}/");

    let mut session = BrowserSession::launch(SessionConfig {
        resource_policy: ResourcePolicy::AgentDefault,
        ..Default::default()
    })
    .await
    .expect("launch");
    let before = session.open(&url).await.expect("open");
    session
        .screenshot(ScreenshotOptions::default())
        .await
        .expect("screenshot");
    let after = session.open(&url).await.expect("re-open");
    assert_eq!(before, after, "the screenshot changed the compressed tree");
    session.shutdown().await;
}

/// The agent's scroll position must survive the lazy-content prepare.
#[tokio::test]
async fn screenshot_preserves_the_scroll_position() {
    if no_chrome() {
        eprintln!("SKIP: no Chromium binary found");
        return;
    }
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let url = format!("file://{}", dir.join("index.html").display());

    let mut session = BrowserSession::launch(SessionConfig::default())
        .await
        .expect("launch");
    session.open(&url).await.expect("open");

    // Set a known scroll position directly. This test covers screenshot
    // position preservation; synthetic wheel input is covered by the action
    // tests and can vary with platform/browser viewport behavior.
    session
        .evaluate("window.scrollTo(0, 2000); true")
        .await
        .expect("set initial scroll position");

    let pos_before = session.evaluate_scroll_y().await;
    assert!(pos_before > 0.0, "scroll did not move the page");

    session
        .screenshot(ScreenshotOptions {
            full_page: false,
            prepare_lazy_content: true,
            timeout_ms: 3_000,
            ..Default::default()
        })
        .await
        .expect("screenshot");

    let pos_after = session.evaluate_scroll_y().await;
    assert!(
        (pos_after - pos_before).abs() < 5.0,
        "the screenshot moved the viewport: {pos_before} -> {pos_after}"
    );
    session.shutdown().await;
}

#[tokio::test]
async fn cancelled_screenshot_invalidates_session_and_restores_scroll() {
    if no_chrome() {
        return;
    }
    let config = SessionConfig {
        persistent: true,
        ..Default::default()
    };
    let mut session = BrowserSession::launch(config.clone()).await.unwrap();
    session
        .open("data:text/html,<h1>Cancel</h1><div style='height:8000px'></div>")
        .await
        .unwrap();
    session
        .evaluate("window.scrollTo(0, 120); true")
        .await
        .unwrap();
    let before = session.evaluate_scroll_y().await;
    let ws = session.browser_ws_url().to_string();
    let target = session.target_id().to_string();
    {
        let screenshot = session.screenshot(ScreenshotOptions::default());
        let wait_for_scroll = async {
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    if session.evaluate_scroll_y().await > before + 100.0 {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("screenshot did not scroll the page");
        };
        tokio::select! {
            _ = wait_for_scroll => {},
            result = screenshot => panic!("screenshot finished before cancellation: {result:?}"),
        }
    }
    assert!(!session.client().is_connected());
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let attached = BrowserSession::attach(ws, Some(target), &config)
        .await
        .unwrap();
    let after = attached.evaluate_scroll_y().await;
    assert!(
        (before - after).abs() < 5.0,
        "scroll changed from {before} to {after}"
    );
    attached.shutdown().await;
    session.shutdown().await;
}

/// The prepare timeout is capped, whatever the caller asks for.
#[tokio::test]
async fn screenshot_timeout_is_capped() {
    if no_chrome() {
        eprintln!("SKIP: no Chromium binary found");
        return;
    }
    let mut session = BrowserSession::launch(SessionConfig::default())
        .await
        .expect("launch");
    session.open(&fixture_file_url()).await.expect("open");
    let started = std::time::Instant::now();
    let _ = session
        .screenshot(ScreenshotOptions {
            full_page: false,
            prepare_lazy_content: true,
            timeout_ms: u64::MAX,
            ..Default::default()
        })
        .await;
    assert!(
        started.elapsed() < std::time::Duration::from_millis(SCREENSHOT_PREPARE_MS_MAX + 5_000),
        "the prepare timeout was not capped"
    );
    session.shutdown().await;
}

/// In shadow mode, the legacy decision remains authoritative until the
/// candidate clears the accuracy and latency gates. Both decisions must
/// settle without hitting the 15 second deadline.
///
/// The settle loop used to require `nodes.len() >= 3` before accepting a
/// tree, so a minimal page ("<h1>Hi</h1>") never satisfied the exit
/// condition and every open waited out the whole settle deadline (15 s).
#[tokio::test]
async fn minimal_page_settles_quickly() {
    if no_chrome() {
        eprintln!("SKIP: no Chromium binary found");
        return;
    }
    let port = spawn_server_with("<html><body><h1>Hi</h1></body></html>");
    let url = format!("http://127.0.0.1:{port}/");

    let mut session = BrowserSession::launch(SessionConfig::default())
        .await
        .expect("launch");
    let started = std::time::Instant::now();
    let tree = session.open(&url).await.expect("open");
    let elapsed = started.elapsed();

    assert!(!tree.is_empty(), "the tree must not be empty");
    assert!(
        elapsed < std::time::Duration::from_secs(3),
        "a minimal page took {elapsed:?} to settle - shadow acceptance did not converge"
    );
    session.shutdown().await;
}

#[tokio::test]
async fn sparse_complete_ax_tree_does_not_hit_settle_deadline() {
    if no_chrome() {
        eprintln!("SKIP: no Chromium binary found");
        return;
    }
    let html = "<html><body><div id='root'></div><script>let el=document.getElementById('root');for(let i=0;i<80;i++){const next=document.createElement('el-'+i);el.appendChild(next);el=next;}el.textContent='deep-leaf-token';</script></body></html>";
    let port = spawn_server_with(html);
    let url = format!("http://127.0.0.1:{port}/");

    let mut session = BrowserSession::launch(SessionConfig::default())
        .await
        .expect("launch");
    let started = std::time::Instant::now();
    let tree = session.open(&url).await.expect("open");
    let elapsed = started.elapsed();

    assert!(
        tree.contains("deep-leaf-token"),
        "the only page text must survive"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(3),
        "a complete sparse AX tree took {elapsed:?} to settle"
    );
    assert_eq!(
        session
            .last_settle()
            .map(|settle| settle.exit_reason.as_str()),
        Some("settled"),
        "the open must settle from a stable AX snapshot, not the deadline"
    );
    session.shutdown().await;
}

/// Content added by a hydration timer shortly after `readyState` becomes
/// "complete" must appear in the tree.
///
/// The settle loop used to exit on the quiet window that starts when the
/// document finishes loading - which is *before* a `setTimeout(..., 1200)`
/// fires. The small-tree quiet window must cover that range, otherwise the
/// late content is truncated (the worst kind of wrong answer: a confident
/// tree that is missing the very thing the agent is looking for).
#[tokio::test]
async fn late_hydration_is_captured() {
    if no_chrome() {
        eprintln!("SKIP: no Chromium binary found");
        return;
    }
    let html = "<html><body><h1>Slow page</h1><div id=\"t\">placeholder</div>\
<script>setTimeout(function(){\
document.getElementById('t').innerHTML = '<a href=\"/late\">late link</a>';\
}, 1200);</script></body></html>";
    let port = spawn_server_with(html);
    let url = format!("http://127.0.0.1:{port}/");

    let mut session = BrowserSession::launch(SessionConfig::default())
        .await
        .expect("launch");
    let tree = session.open(&url).await.expect("open");

    assert!(
        tree.contains("late link"),
        "content hydrated 1.2 s after load was truncated:\n{tree}"
    );
    session.shutdown().await;
}

/// Same-length text and ARIA changes must reset settling through mutations.
#[tokio::test]
async fn same_length_aria_change_is_captured() {
    if no_chrome() {
        eprintln!("SKIP: no Chromium binary found");
        return;
    }
    let html = "<html><body><button id='b' aria-label='Before'>Click</button>\
<script>setTimeout(() => document.getElementById('b').setAttribute('aria-label', 'After!'), 500)</script>\
</body></html>";
    let port = spawn_server_with(html);
    let url = format!("http://127.0.0.1:{port}/");
    let mut session = BrowserSession::launch(SessionConfig::default())
        .await
        .expect("launch");
    let tree = session.open(&url).await.expect("open");
    assert!(
        tree.contains("After!"),
        "same-length aria-label update was missed: {tree}"
    );
    assert!(
        !tree.contains("Before"),
        "stale aria-label remained in tree: {tree}"
    );
    session.shutdown().await;
}
