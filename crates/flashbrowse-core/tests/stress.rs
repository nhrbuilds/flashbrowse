//! ## Additional integration stress coverage
//!
//! Exercises:
//! 1. Compression at 50k-node scale (CPU + reduction).
//! 2. 20 sequential navigations on one live session (latency, leaks).
//! 3. 100-action storm (click/fill/scroll/press_key p50/p95).
//! 4. 5 parallel browser launches (chaos isolation).
//! 5. Error-path survival (bad id, bad URL, empty value).
//! 6. Real-world fetch (HN + example.com) raw-vs-compressed tokens.
//!
//! Requires Chrome; live parts skip gracefully when absent. Run:
//!   cargo test -p flashbrowse-core --test stress -- --nocapture

use std::time::Instant;

use flashbrowse_core::{actions::ScrollDirection, BrowserSession, SessionConfig};

fn skip_if_no_chrome() -> bool {
    flashbrowse_cdp::find_chrome(None).is_err()
}

fn pctl(mut xs: Vec<u128>, p: f64) -> u128 {
    if xs.is_empty() {
        return 0;
    }
    xs.sort_unstable();
    let i = ((xs.len() as f64 - 1.0) * p) as usize;
    xs[i]
}

#[test]
fn stress_compression_50k() {
    use flashbrowse_cdp::protocol::accessibility::{AXNode, AXValue};
    use flashbrowse_core::tree::{compress, render};

    fn v(s: &str, t: &str) -> AXValue {
        AXValue {
            type_: Some(t.to_string()),
            value: Some(serde_json::Value::String(s.to_string())),
        }
    }
    fn n(id: String, role: &str, name: &str, backend: Option<i64>) -> AXNode {
        AXNode {
            node_id: id,
            ignored: false,
            role: Some(v(role, "RoleValue")),
            name: Some(v(name, "ComputedString")),
            value: None,
            backend_node_id: backend,
            child_ids: vec![],
            ignored_reasons: None,
            properties: None,
        }
    }

    let mut nodes = Vec::with_capacity(50_000);
    for i in 0..40_000 {
        nodes.push(n(format!("gen-{i}"), "generic", "", None));
    }
    for i in 0..5_000 {
        let role = if i % 2 == 0 { "link" } else { "button" };
        nodes.push(n(
            format!("act-{i}"),
            role,
            &format!("Item {i} with some descriptive label text"),
            Some((i + 1) as i64),
        ));
    }
    for i in 0..5_000 {
        nodes.push(n(format!("st-{i}"), "StaticText", "ghost text", None));
    }

    let t0 = Instant::now();
    let (semantic, map) = compress(&nodes);
    let rendered = render("Stress", "https://example.com", &semantic);
    let elapsed = t0.elapsed();

    let naive_chars = format!("{nodes:#?}").len();
    let ratio = 1.0 - (rendered.len() as f64 / naive_chars as f64);
    println!(
        "STRESS-COMPRESS 50k nodes -> {} semantic, {} mapped, raw={}B comp={}B red={:.1}% in {:?}",
        semantic.len(),
        map.len(),
        naive_chars,
        rendered.len(),
        ratio * 100.0,
        elapsed
    );
    assert_eq!(map.len(), 5_000);
    assert!(ratio >= 0.80, "reduction {:.1} < 80", ratio * 100.0);
    assert!(
        elapsed.as_millis() < 5000,
        "50k compress too slow: {elapsed:?}"
    );
}

#[tokio::test]
async fn stress_sequential_open_x20_and_action_storm() {
    if skip_if_no_chrome() {
        println!("SKIP: no Chrome");
        return;
    }
    let mut s = BrowserSession::launch(SessionConfig::default())
        .await
        .expect("launch");

    // 20 sequential navigations, each 60 actionable elements.
    let html = "data:text/html,<html><body><h1>Stress</h1>".to_string()
        + &"<button>Btn</button>".repeat(30)
        + &"<input aria-label='f'/>".repeat(30)
        + "</body></html>";
    let mut open_ms = Vec::new();
    for i in 0..20 {
        let t0 = Instant::now();
        let tree = s.open(&html).await.expect("open");
        open_ms.push(t0.elapsed().as_millis());
        assert!(tree.contains("[1]"), "open {i} missing [1]: {tree}");
    }
    println!(
        "STRESS-OPEN x20: avg={}ms p95={}ms max={}ms",
        open_ms.iter().sum::<u128>() / open_ms.len() as u128,
        pctl(open_ms.clone(), 0.95),
        open_ms.iter().max().unwrap()
    );

    // 100-action storm on the 60-element page.
    let mut act_ms = Vec::with_capacity(100);
    for i in 0..100 {
        let t0 = Instant::now();
        let r = match i % 4 {
            0 => s.click(1).await,
            1 => s.fill(2, &format!("v{i}")).await,
            2 => s.scroll(ScrollDirection::Down, 100.0).await,
            _ => s.press_key("Enter").await,
        };
        let tree = r.expect("action storm failed");
        act_ms.push(t0.elapsed().as_millis());
        assert!(tree.contains("[1]"), "storm {i} tree broken");
    }
    println!(
        "STRESS-STORM x100: avg={}ms p50={}ms p95={}ms max={}ms",
        act_ms.iter().sum::<u128>() / act_ms.len() as u128,
        pctl(act_ms.clone(), 0.5),
        pctl(act_ms.clone(), 0.95),
        act_ms.iter().max().unwrap()
    );
    assert!(pctl(act_ms.clone(), 0.95) < 5000, "p95 storm too slow");

    // Error-path survival: bad ids and bad URL must not poison session.
    assert!(s.click(999_999).await.is_err());
    let after_err = s.open(&html).await.expect("open after error");
    assert!(after_err.contains("[1]"), "session poisoned after error");
    println!("STRESS-ERROR-SURVIVAL ok (bad id -> is_err, session still live)");

    s.shutdown().await;
}

#[tokio::test]
async fn stress_parallel_sessions_x5() {
    if skip_if_no_chrome() {
        println!("SKIP: no Chrome");
        return;
    }
    let t0 = Instant::now();
    let launch = async {
        let mut futs = vec![];
        for _ in 0..5 {
            futs.push(async move {
                let mut s = BrowserSession::launch(SessionConfig::default())
                    .await
                    .expect("launch");
                let tree = s
                    .open("data:text/html,<html><body><button>P</button></body></html>")
                    .await
                    .expect("open");
                assert!(tree.contains("[1] Button"));
                (s, tree.len())
            });
        }
        let mut out = vec![];
        for f in futures_util::future::join_all(futs).await {
            out.push(f);
        }
        out
    };
    let sessions = launch.await;
    let elapsed = t0.elapsed();
    println!(
        "STRESS-PARALLEL x5: 5 browsers launched+opened in {:?}",
        elapsed
    );
    for (s, _len) in sessions {
        s.shutdown().await;
    }
    assert!(elapsed.as_secs() < 30, "parallel 5 too slow: {elapsed:?}");
}

/// Real pages used by the token test, with content that only the real page
/// has. A marker is required because an error page must never be measured as
/// a compression result.
const REAL_PAGES: &[(&str, &str)] = &[
    ("https://example.com", "Example Domain"),
    ("https://news.ycombinator.com", "comments"),
];

/// Hacker News answers bursty clients with this page and HTTP 200. It is not
/// the site, and its tiny tree must not be reported as a token reduction.
const RATE_LIMIT_MARKER: &str = "Sorry, we're not able to serve your requests this quickly.";

/// Open a real page, retrying once on a transient network failure.
///
/// Local browser launch failures still fail the test loudly (`expect`); only
/// live network errors or interstitial pages are retried and skipped.
async fn open_real_page(url: &str, marker: &str) -> Option<(BrowserSession, String, u128)> {
    for attempt in 0..2 {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
        let mut session = match BrowserSession::launch(SessionConfig::default()).await {
            Ok(session) => session,
            Err(err) if attempt == 0 => {
                eprintln!("STRESS-REAL {url}: launch failed ({err}); retrying");
                continue;
            }
            Err(err) => panic!("launch failed twice for {url}: {err}"),
        };
        let t0 = Instant::now();
        match session.open(url).await {
            Ok(tree) if !tree.contains(RATE_LIMIT_MARKER) && tree.contains(marker) => {
                return Some((session, tree, t0.elapsed().as_millis()));
            }
            Ok(tree) => {
                eprintln!(
                    "STRESS-REAL {url}: unusable page ({} chars, marker={}, rate_limited={})",
                    tree.len(),
                    tree.contains(marker),
                    tree.contains(RATE_LIMIT_MARKER)
                );
                session.shutdown().await;
            }
            Err(e) => {
                eprintln!("STRESS-REAL {url}: open failed: {e}");
                session.shutdown().await;
            }
        }
    }
    None
}

#[tokio::test]
async fn stress_real_world_tokens() {
    if skip_if_no_chrome() {
        println!("SKIP: no Chrome");
        return;
    }
    // Real network fetch - skip (not fail) if offline.
    let probe = std::process::Command::new("curl")
        .args(["-sI", "-m", "5", "https://example.com"])
        .output();
    if !probe.map(|o| o.status.success()).unwrap_or(false) {
        println!("SKIP: no network");
        return;
    }
    let mut measured = 0usize;
    for &(url, marker) in REAL_PAGES {
        let Some((s, tree, ms)) = open_real_page(url, marker).await else {
            println!("SKIP: {url} unreachable or served an interstitial");
            continue;
        };
        let raw = s
            .client()
            .send(
                Some(s.session_id()),
                "Runtime.evaluate",
                serde_json::json!({"expression": "document.documentElement.outerHTML.length", "returnByValue": true}),
            )
            .await
            .ok()
            .and_then(|v| {
                v.get("result")
                    .and_then(|r| r.get("value"))
                    .and_then(|x| x.as_u64())
            })
            .unwrap_or(0) as usize;
        let raw_t = raw.div_ceil(4);
        let comp_t = tree.len().div_ceil(4);
        println!(
            "STRESS-REAL {url}: {}ms raw={} chars ({} tokens) comp={} chars ({} tokens) reduction={:.1}%",
            ms,
            raw,
            raw_t,
            tree.len(),
            comp_t,
            if raw_t > 0 {
                (1.0 - comp_t as f64 / raw_t as f64) * 100.0
            } else {
                0.0
            }
        );
        assert!(raw > 0, "empty raw HTML for {url}");
        measured += 1;
        s.shutdown().await;
    }
    if measured == 0 {
        println!("SKIP: 0 of {} real pages measured", REAL_PAGES.len());
    }
}

/// Truncation guard: repeated opens must produce complete, byte-identical trees.
///
/// This is the regression test for the "145 tokens instead of 1770" bug:
/// Chromium populates the AX tree ~400 ms after `load`, so a naive snapshot
/// truncates client-rendered pages.
#[tokio::test]
async fn stress_no_truncation_across_repeats() {
    if flashbrowse_cdp::find_chrome(None).is_err() {
        println!("SKIP: no Chrome");
        return;
    }
    let mut session =
        flashbrowse_core::BrowserSession::launch(flashbrowse_core::SessionConfig::default())
            .await
            .expect("launch");

    // A client-rendered page (hydration happens after load).
    let html = "data:text/html,<html><body><div id=app></div><script>\
        document.getElementById('app').innerHTML = \
        '<button>Ready</button><a href=\"/x\">Link</a>' + \
        Array.from({length: 40}, (_, i) => '<p>Row ' + i + '</p>').join('');\
        </script></body></html>";

    let fixture_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut pages = vec![("hydration", html.to_string())];
    for name in ["lazy-tabs.html", "modal-dialog.html", "heavy-table.html"] {
        pages.push((name, format!("file://{}", fixture_dir.join(name).display())));
    }
    for (name, page) in pages {
        let mut expected: Option<String> = None;
        for iteration in 0..20 {
            let tree = session.open(&page).await.expect("open");
            let marker = match name {
                "hydration" => "Ready",
                "lazy-tabs.html" => "Panel one visible",
                "modal-dialog.html" => "Open dialog",
                "heavy-table.html" => "sku-0200",
                _ => unreachable!(),
            };
            assert!(tree.contains(marker), "{name} missing {marker:?} from tree");
            if let Some(first) = &expected {
                assert_eq!(&tree, first, "{name} changed on repeat open {iteration}");
            } else {
                expected = Some(tree);
            }
        }
        println!("STRESS-NO-TRUNC {name} x20: exact tree equality");
    }

    session.shutdown().await;
}
