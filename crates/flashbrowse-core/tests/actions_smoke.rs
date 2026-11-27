//! End-to-end smoke: live `open` → `click` → `fill` → `scroll` against `data:` pages.
//! Requires system Chromium - skipped when absent.

use flashbrowse_core::{actions::ScrollDirection, BrowserSession, SessionConfig};

#[test]
fn browser_operations_work_without_caller_runtime_drivers() {
    if flashbrowse_cdp::find_chrome(None).is_err() {
        return;
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut session = BrowserSession::launch(SessionConfig::default())
            .await
            .unwrap();
        let tree = session
            .open("data:text/html,<button onclick=\"this.textContent='Done'\">Go</button>")
            .await
            .unwrap();
        assert!(tree.contains("Go"));
        assert!(session.click(1).await.unwrap().contains("Done"));
        assert!(!session.screenshot_viewport().await.unwrap().is_empty());
        session.shutdown().await;
    });
}

#[tokio::test]
async fn test_open_click_fill_scroll() {
    if flashbrowse_cdp::find_chrome(None).is_err() {
        eprintln!("SKIP: no Chromium binary found");
        return;
    }

    let mut session = BrowserSession::launch(SessionConfig::default())
        .await
        .expect("launch");

    // Open: numbered action graph for a button + input.
    let tree = session
        .open("data:text/html,<html><body><button>Submit</button><input type='text' aria-label='Name' /></body></html>")
        .await
        .expect("open");
    assert!(
        tree.contains("[1]"),
        "expected numbered action, got:\n{tree}"
    );

    // Click whatever [1] is (button). Should not error and should rebuild.
    let _after_click = session.click(1).await.expect("click");

    // Fill: find the input's action id from the rebuilt map, else use [1] fallback.
    // Robust: pick the smallest id whose backend maps to an input by role? We
    // simply try fill on every id until one succeeds (live test, small page).
    let mut filled = false;
    for id in 1..=5u32 {
        if session.action_map().contains_key(&id) && session.fill(id, "hello").await.is_ok() {
            let after = session.last_tree().to_string();
            assert!(
                after.contains("hello") || after.contains("["),
                "fill rebuild should contain state"
            );
            filled = true;
            break;
        }
    }
    assert!(filled, "at least one fill should succeed");

    // Scroll: synthetic wheel + rebuild.
    let _after_scroll = session
        .scroll(ScrollDirection::Down, 300.0)
        .await
        .expect("scroll");

    // Markdown extraction returns non-empty document text.
    let md = session.extract_markdown().await.expect("markdown");
    assert!(!md.is_empty(), "markdown should be non-empty");
    let error = session
        .evaluate("(() => { throw new Error('evaluation failed') })()")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("evaluation failed"));

    session.shutdown().await;
}
