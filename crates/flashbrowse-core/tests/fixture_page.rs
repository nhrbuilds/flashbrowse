//! Fixture-page integration test.
//!
//! Serves `tests/fixtures/index.html` over a throwaway HTTP server and
//! drives a real browser against it: open, click, fill, select, scroll.
//! This is the same page the soak harness uses, so a regression here shows
//! up in the soak too.
//!
//! Skips gracefully when no Chromium binary is available.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::Arc;

use flashbrowse_core::actions::ScrollDirection;
use flashbrowse_core::{BrowserSession, SessionConfig};

/// Minimal single-file HTTP server: serves the fixture for any path.
fn spawn_fixture_server(html: &'static str) -> (u16, Arc<std::thread::JoinHandle<()>>) {
    use std::net::Shutdown;
    use std::time::Duration;

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let handle = std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            std::thread::spawn(move || {
                let mut stream = stream;
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                // Drain the request head.
                if let Ok(cloned) = stream.try_clone() {
                    let mut reader = BufReader::new(cloned);
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    while line != "\r\n" && line != "\n" {
                        line.clear();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 {
                            break;
                        }
                    }
                }
                let body = html;
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(body.as_bytes());
                let _ = stream.flush();
                let _ = stream.shutdown(Shutdown::Write);
            });
        }
    });
    (port, Arc::new(handle))
}

fn fixture_html() -> &'static str {
    // Embedded copy so the test does not depend on the working directory.
    include_str!("fixtures/index.html")
}

#[tokio::test]
async fn fixture_page_exposes_actions_and_accepts_interactions() {
    if flashbrowse_cdp::find_chrome(None).is_err() {
        eprintln!("SKIP: no Chromium binary found");
        return;
    }

    let (port, _server) = spawn_fixture_server(fixture_html());
    let url = format!("http://127.0.0.1:{port}/");

    let mut session = BrowserSession::launch(SessionConfig::default())
        .await
        .expect("launch");

    let tree = session.open(&url).await.expect("open");

    // The fixture must yield a numbered action graph.
    assert!(tree.contains("[1]"), "expected numbered actions:\n{tree}");
    // The form controls must be reachable by the agent.
    assert!(
        tree.contains("Query") || tree.contains("input") || tree.contains("Textbox"),
        "expected the text input to be addressable:\n{tree}"
    );
    // The table rows should be present as text.
    assert!(
        tree.contains("alpha") || tree.contains("gamma"),
        "expected table content in the tree:\n{tree}"
    );

    // Fill: find an action whose role is a textbox, else fall back to id 1.
    let input_id = find_role_id(&tree, &["textbox", "searchbox", "input"]);
    let fill_id = input_id.unwrap_or(1);
    let after_fill = session
        .fill(fill_id, "flashbrowse")
        .await
        .expect("fill should succeed on the fixture input");
    assert!(
        after_fill.contains("flashbrowse") || after_fill.contains("["),
        "tree after fill should still be a valid action graph"
    );

    // Click the submit button.
    let button_id = find_role_id(&tree, &["button"]);
    if let Some(id) = button_id {
        let after = session.click(id).await.expect("click submit");
        assert!(
            after.contains("["),
            "tree after click should stay an action graph"
        );
    }

    // Scroll must work and rebuild the tree.
    let after_scroll = session
        .scroll(ScrollDirection::Down, 400.0)
        .await
        .expect("scroll");
    assert!(
        after_scroll.contains("["),
        "tree after scroll should stay an action graph"
    );

    // Markdown extraction must yield the heading.
    let md = session.extract_markdown().await.expect("markdown");
    assert!(
        md.to_lowercase().contains("flashbrowse fixture"),
        "markdown should contain the heading:\n{md}"
    );

    session.shutdown().await;
}

/// Find the lowest numbered action id whose rendered role matches `roles`.
fn find_role_id(tree: &str, roles: &[&str]) -> Option<u32> {
    for line in tree.lines() {
        let t = line.trim_start();
        let Some(rest) = t.strip_prefix('[') else {
            continue;
        };
        let Some((id_str, rest)) = rest.split_once(']') else {
            continue;
        };
        let Ok(id) = id_str.trim().parse::<u32>() else {
            continue;
        };
        let lower = rest.to_lowercase();
        if roles.iter().any(|r| lower.contains(r)) {
            return Some(id);
        }
    }
    None
}

#[tokio::test]
async fn fixture_server_serves_html() {
    let (port, _server) = spawn_fixture_server("<html><body><h1>hi</h1></body></html>");
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .expect("write");
    let mut buf = String::new();
    stream.read_to_string(&mut buf).expect("read");
    assert!(buf.starts_with("HTTP/1.1 200 OK"), "bad response: {buf}");
    assert!(buf.contains("<h1>hi</h1>"), "missing body: {buf}");
}
