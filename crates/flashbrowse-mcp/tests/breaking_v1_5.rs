//! Breaking-point stress and torture tests for v1.5.0 capabilities
//! over the full MCP -> Core -> POSIX pipe -> Chrome stack.

use std::io::{BufRead, BufReader as StdBufReader, Write};
use std::net::{Shutdown, TcpListener};
use std::sync::Arc;
use std::time::Duration;

use flashbrowse_core::SessionConfig;
use flashbrowse_mcp::server::serve;
use serde_json::Value;
use tokio::io::{duplex, AsyncBufReadExt, AsyncWriteExt, BufReader};

async fn roundtrip<W, R>(client_out: &mut W, client_lines: &mut R, frame: Value) -> Value
where
    W: AsyncWriteExt + Unpin,
    R: AsyncBufReadExt + Unpin,
{
    client_out
        .write_all(frame.to_string().as_bytes())
        .await
        .unwrap();
    client_out.write_all(b"\n").await.unwrap();
    client_out.flush().await.unwrap();
    let mut line = String::new();
    client_lines.read_line(&mut line).await.unwrap();
    serde_json::from_str(line.trim()).expect("pure JSON stdout")
}

fn tool_text(resp: &Value) -> String {
    resp.get("result")
        .and_then(|r| r.get("content"))
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .and_then(|c| c.get("text"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn assert_tool_ok(resp: &Value) {
    assert!(resp.get("error").is_none(), "JSON-RPC error: {resp}");
    let is_error = resp
        .get("result")
        .and_then(|r| r.get("isError"))
        .and_then(|v| v.as_bool());
    assert_ne!(
        is_error,
        Some(true),
        "tool returned isError: {}",
        tool_text(resp)
    );
}

fn action_id_for(tree: &str, needle: &str) -> u32 {
    tree.lines()
        .find(|l| l.contains(needle))
        .and_then(|l| l.trim_start().strip_prefix('['))
        .and_then(|r| r.split(']').next())
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("missing action_id for {needle} in:\n{tree}"))
}

fn spawn_raw_http<F>(handler: F) -> u16
where
    F: Fn(&str, &str) -> (&'static str, &'static str, String) + Send + Sync + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    let handler = Arc::new(handler);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let handler = Arc::clone(&handler);
            std::thread::spawn(move || {
                let mut stream = stream;
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let mut head = String::new();
                if let Ok(cloned) = stream.try_clone() {
                    let mut reader = StdBufReader::new(cloned);
                    let mut line = String::new();
                    while reader.read_line(&mut line).unwrap_or(0) > 0 {
                        if line == "\r\n" || line == "\n" {
                            break;
                        }
                        head.push_str(&line);
                        line.clear();
                    }
                }
                if head.is_empty() {
                    return;
                }
                let path = head
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/");
                let (status, content_type, body) = handler(path, &head);
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
                let _ = stream.shutdown(Shutdown::Write);
            });
        }
    });
    port
}

async fn init_mcp_with_config(
    config: SessionConfig,
) -> (
    tokio::io::DuplexStream,
    BufReader<tokio::io::DuplexStream>,
    tokio::task::JoinHandle<()>,
) {
    let (server_in, mut client_out) = duplex(1024 * 1024);
    let (server_out, client_in) = duplex(1024 * 1024);
    let mut client_lines = BufReader::new(client_in);
    let server = tokio::spawn(async move {
        serve(server_in, server_out, config).await.expect("serve");
    });
    let init = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}),
    )
    .await;
    assert!(init.get("error").is_none(), "init failed: {init}");
    (client_out, client_lines, server)
}

#[tokio::test]
async fn torture_console_and_network_ring_buffer_overflow_and_large_body_truncation() {
    if flashbrowse_cdp::find_chrome(None).is_err() {
        eprintln!("SKIP: no Chromium binary found");
        return;
    }

    let big_payload = format!("{{\"blob\":\"{}\"}}", "Z".repeat(128 * 1024));
    let port = spawn_raw_http(move |path, _| {
        if path.starts_with("/api/big") {
            ("200 OK", "application/json", big_payload.clone())
        } else if path == "/api/fail" {
            (
                "500 Internal Server Error",
                "application/json",
                "{\"error\":\"boom500\"}".to_string(),
            )
        } else if path.starts_with("/api/ping/") {
            ("200 OK", "application/json", "{\"ok\":true}".to_string())
        } else {
            (
                "200 OK",
                "text/html; charset=utf-8",
                r#"<!doctype html><html><head><title>OverflowTest</title></head><body><h1>RingBuffer</h1><script>
for (let i = 0; i < 290; i++) {
  console.log('burst-log-' + i);
}
console.warn('tail-warn');
console.error('tail-error');
for (let i = 0; i < 220; i++) {
  const x = new XMLHttpRequest();
  x.open('GET', '/api/ping/' + i, false);
  x.send();
}
const b = new XMLHttpRequest();
b.open('GET', '/api/big', false);
b.send();
const f = new XMLHttpRequest();
f.open('GET', '/api/fail', false);
f.send();
setTimeout(() => { throw new Error('uncaught-ring-tail'); }, 15);
</script></body></html>"#
                    .to_string(),
            )
        }
    });

    let (mut out, mut lines, server) = init_mcp_with_config(SessionConfig::default()).await;
    let open = roundtrip(
        &mut out,
        &mut lines,
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"browser_open","arguments":{"url":format!("http://127.0.0.1:{port}/")}}}),
    )
    .await;
    assert_tool_ok(&open);

    // Wait briefly so setTimeout exception settles cleanly if still in flight.
    let _ = roundtrip(
        &mut out,
        &mut lines,
        serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"browser_wait_for","arguments":{"network_idle_ms":150,"timeout_ms":3000}}}),
    )
    .await;

    let cons_all = roundtrip(
        &mut out,
        &mut lines,
        serde_json::json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"browser_console","arguments":{"action":"list"}}}),
    )
    .await;
    assert_tool_ok(&cons_all);
    let cons_arr: Vec<Value> = serde_json::from_str(&tool_text(&cons_all)).expect("console JSON");
    assert_eq!(
        cons_arr.len(),
        200,
        "console ring buffer must stay capped at 200"
    );
    assert!(
        cons_arr
            .iter()
            .any(|e| e["text"].as_str() == Some("tail-error")),
        "newest console error evicted unexpectedly"
    );

    let cons_err = roundtrip(
        &mut out,
        &mut lines,
        serde_json::json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"browser_console","arguments":{"action":"list","level":"error"}}}),
    )
    .await;
    assert_tool_ok(&cons_err);
    let err_arr: Vec<Value> = serde_json::from_str(&tool_text(&cons_err)).expect("err JSON");
    assert!(err_arr.iter().all(|e| e["level"] == "error"));
    assert!(err_arr.iter().any(|e| e["text"] == "tail-error"));

    // Network idle and the network log observe different signals. Wait for the
    // actual MCP result, while retaining a strict assertion and useful diagnostics.
    let mut net_arr: Vec<Value> = Vec::new();
    let mut failed: Vec<Value> = Vec::new();
    let arrived = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let net_fail = roundtrip(
                &mut out,
                &mut lines,
                serde_json::json!({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"browser_network","arguments":{"action":"list","failed_only":true}}}),
            )
            .await;
            assert_tool_ok(&net_fail);
            failed = serde_json::from_str(&tool_text(&net_fail)).expect("fail JSON");
            let net_all = roundtrip(
                &mut out,
                &mut lines,
                serde_json::json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"browser_network","arguments":{"action":"list"}}}),
            )
            .await;
            assert_tool_ok(&net_all);
            net_arr = serde_json::from_str(&tool_text(&net_all)).expect("network JSON");

            if failed.iter().any(|entry| {
                entry["url"].as_str().is_some_and(|url| url.ends_with("/api/fail"))
                    && entry["status"] == 500
                    && entry["failed"] == true
            }) {
                assert!(failed.iter().all(|entry| entry["failed"] == true));
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    if arrived.is_err() {
        let tail: Vec<_> = net_arr
            .iter()
            .filter(|entry| {
                entry["url"]
                    .as_str()
                    .is_some_and(|url| url.ends_with("/api/fail") || url.ends_with("/api/big"))
            })
            .collect();
        panic!("failed_only missing /api/fail 500 entry; network log: {tail:?}; failed_only: {failed:?}");
    }
    assert_eq!(
        net_arr.len(),
        200,
        "network ring buffer must stay capped at 200"
    );

    let big_req_id = net_arr
        .iter()
        .find(|e| e["url"].as_str().is_some_and(|u| u.contains("/api/big")))
        .and_then(|e| e["request_id"].as_str())
        .expect("/api/big request_id")
        .to_string();
    let big_body = roundtrip(
        &mut out,
        &mut lines,
        serde_json::json!({"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"browser_network","arguments":{"action":"body","request_id":big_req_id}}}),
    )
    .await;
    assert_tool_ok(&big_body);
    let big_txt = tool_text(&big_body);
    assert!(
        big_txt.ends_with("...(truncated)") && big_txt.len() <= 64 * 1024 + 32,
        "128KiB response body not safely truncated: len={}",
        big_txt.len()
    );

    // Invalid request_id must fail cleanly without poisoning the session.
    let bad_body = roundtrip(
        &mut out,
        &mut lines,
        serde_json::json!({"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"browser_network","arguments":{"action":"body","request_id":"nonexistent-req-999"}}}),
    )
    .await;
    assert_eq!(bad_body["result"]["isError"], true);

    assert_tool_ok(
        &roundtrip(
            &mut out,
            &mut lines,
            serde_json::json!({"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"browser_console","arguments":{"action":"clear"}}}),
        )
        .await,
    );
    assert_tool_ok(
        &roundtrip(
            &mut out,
            &mut lines,
            serde_json::json!({"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"browser_network","arguments":{"action":"clear"}}}),
        )
        .await,
    );
    let cons_empty = roundtrip(
        &mut out,
        &mut lines,
        serde_json::json!({"jsonrpc":"2.0","id":12,"method":"tools/call","params":{"name":"browser_console","arguments":{"action":"list"}}}),
    )
    .await;
    assert_eq!(tool_text(&cons_empty), "[]");

    drop(out);
    drop(lines);
    server.await.unwrap();
}

#[tokio::test]
async fn torture_fetch_route_burst_interception_abort_fulfill_and_invalid_inputs() {
    if flashbrowse_cdp::find_chrome(None).is_err() {
        eprintln!("SKIP: no Chromium binary found");
        return;
    }

    let port = spawn_raw_http(|path, _| {
        if path == "/api/live.json" || path == "/api/mocked.json" {
            ("200 OK", "application/json", "{\"live\":true}".to_string())
        } else {
            (
                "200 OK",
                "text/html; charset=utf-8",
                "<!doctype html><html><body><h1>RouteBurst</h1></body></html>".to_string(),
            )
        }
    });

    let (mut out, mut lines, server) = init_mcp_with_config(SessionConfig::default()).await;
    assert_tool_ok(
        &roundtrip(
            &mut out,
            &mut lines,
            serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"browser_open","arguments":{"url":format!("http://127.0.0.1:{port}/")}}}),
        )
        .await,
    );

    // Blank pattern, out-of-range HTTP status, and CRLF content_type must be rejected at param boundary.
    let bad_route = roundtrip(
        &mut out,
        &mut lines,
        serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"browser_route","arguments":{"action":"add","url_pattern":"   "}}}),
    )
    .await;
    assert_eq!(bad_route["error"]["code"], -32602);

    let bad_status_low = roundtrip(
        &mut out,
        &mut lines,
        serde_json::json!({"jsonrpc":"2.0","id":301,"method":"tools/call","params":{"name":"browser_route","arguments":{"action":"add","url_pattern":"*/api/x","status":0}}}),
    )
    .await;
    assert_eq!(bad_status_low["error"]["code"], -32602);

    let bad_status_high = roundtrip(
        &mut out,
        &mut lines,
        serde_json::json!({"jsonrpc":"2.0","id":302,"method":"tools/call","params":{"name":"browser_route","arguments":{"action":"add","url_pattern":"*/api/x","status":999}}}),
    )
    .await;
    assert_eq!(bad_status_high["error"]["code"], -32602);

    let bad_ct = roundtrip(
        &mut out,
        &mut lines,
        serde_json::json!({"jsonrpc":"2.0","id":303,"method":"tools/call","params":{"name":"browser_route","arguments":{"action":"add","url_pattern":"*/api/x","content_type":"text/plain\r\nX-Injected: 1"}}}),
    )
    .await;
    assert_eq!(bad_ct["error"]["code"], -32602);

    for (id, pat, abort, body) in [
        (4, "*/api/mocked.json", false, "{\"mocked\":1}"),
        (5, "*a*b.json", false, "{\"glob\":true}"),
        (6, "*/api/aborted", true, ""),
    ] {
        assert_tool_ok(
            &roundtrip(
                &mut out,
                &mut lines,
                serde_json::json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"browser_route","arguments":{"action":"add","url_pattern":pat,"abort":abort,"body":body}}}),
            )
            .await,
        );
    }

    let list_r = roundtrip(
        &mut out,
        &mut lines,
        serde_json::json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"browser_route","arguments":{"action":"list"}}}),
    )
    .await;
    let rules: Vec<Value> = serde_json::from_str(&tool_text(&list_r)).expect("routes JSON");
    assert_eq!(rules.len(), 3);

    // 30-request synchronous burst across fulfill, multi-wildcard glob, abort, and passthrough.
    let burst_js = r#"(() => {
      const get = (u) => {
        try {
          const x = new XMLHttpRequest();
          x.open('GET', u, false);
          x.send();
          return x.status + ':' + x.responseText;
        } catch (_) {
          return 'ABORTED';
        }
      };
      for (let i = 0; i < 10; i++) {
        if (!get('/api/mocked.json').includes('"mocked":1')) return 'fail-mock-' + i;
        if (!get('/api/item_a_x_b.json').includes('"glob":true')) return 'fail-glob-' + i;
        if (get('/api/aborted') !== 'ABORTED') return 'fail-abort-' + i;
        if (!get('/api/live.json').includes('"live":true')) return 'fail-live-' + i;
      }
      return 'OK_40';
    })()"#;
    let burst = roundtrip(
        &mut out,
        &mut lines,
        serde_json::json!({"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"browser_evaluate","arguments":{"expression":burst_js}}}),
    )
    .await;
    assert_tool_ok(&burst);
    assert!(
        tool_text(&burst).contains("OK_40"),
        "route burst failed: {burst}"
    );

    assert_tool_ok(
        &roundtrip(
            &mut out,
            &mut lines,
            serde_json::json!({"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"browser_route","arguments":{"action":"clear"}}}),
        )
        .await,
    );

    let after = roundtrip(
        &mut out,
        &mut lines,
        serde_json::json!({"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"browser_evaluate","arguments":{"expression":"(() => { const x = new XMLHttpRequest(); x.open('GET', '/api/mocked.json', false); x.send(); return x.responseText; })()"}}}),
    )
    .await;
    assert_tool_ok(&after);
    let after_str: String = serde_json::from_str(&tool_text(&after))
        .unwrap_or_else(|e| panic!("route clear returned non-JSON ({e}): {after}"));
    assert_eq!(
        after_str, "{\"live\":true}",
        "route clear did not restore live requests: {after}"
    );

    drop(out);
    drop(lines);
    server.await.unwrap();
}

#[tokio::test]
async fn torture_context_churn_x10_isolation_and_error_recovery() {
    if flashbrowse_cdp::find_chrome(None).is_err() {
        eprintln!("SKIP: no Chromium binary found");
        return;
    }

    let port = spawn_raw_http(|_, _| {
        (
            "200 OK",
            "text/html; charset=utf-8",
            "<!doctype html><html><body><h1>ContextChurn</h1></body></html>".to_string(),
        )
    });
    let url = format!("http://127.0.0.1:{port}/");

    let (mut out, mut lines, server) = init_mcp_with_config(SessionConfig::default()).await;
    assert_tool_ok(
        &roundtrip(
            &mut out,
            &mut lines,
            serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"browser_open","arguments":{"url":url}}}),
        )
        .await,
    );
    assert_tool_ok(
        &roundtrip(
            &mut out,
            &mut lines,
            serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"browser_evaluate","arguments":{"expression":"document.cookie='user=default'; localStorage.setItem('ctx','default'); 'ok'"}}}),
        )
        .await,
    );

    for i in 1..=10 {
        let cname = format!("c{i}");
        assert_tool_ok(
            &roundtrip(
                &mut out,
                &mut lines,
                serde_json::json!({"jsonrpc":"2.0","id":100 + i * 10,"method":"tools/call","params":{"name":"browser_contexts","arguments":{"action":"create","name":cname}}}),
            )
            .await,
        );
        assert_tool_ok(
            &roundtrip(
                &mut out,
                &mut lines,
                serde_json::json!({"jsonrpc":"2.0","id":101 + i * 10,"method":"tools/call","params":{"name":"browser_open","arguments":{"url":url}}}),
            )
            .await,
        );
        let iso = roundtrip(
            &mut out,
            &mut lines,
            serde_json::json!({"jsonrpc":"2.0","id":102 + i * 10,"method":"tools/call","params":{"name":"browser_evaluate","arguments":{"expression":format!("const before = document.cookie + '|' + (localStorage.getItem('ctx') || 'none'); document.cookie='user={cname}'; localStorage.setItem('ctx','{cname}'); before")}}}),
        )
        .await;
        assert_tool_ok(&iso);
        assert!(
            tool_text(&iso).contains("|none"),
            "context {cname} saw leaked storage: {iso}"
        );
        assert_tool_ok(
            &roundtrip(
                &mut out,
                &mut lines,
                serde_json::json!({"jsonrpc":"2.0","id":103 + i * 10,"method":"tools/call","params":{"name":"browser_contexts","arguments":{"action":"close","name":cname}}}),
            )
            .await,
        );
    }

    // Invalid context mutations must fail cleanly without corrupting active default.
    for (id, action, name) in [
        (500, "create", "default"),
        (501, "create", ""),
        (502, "select", "missing-ctx"),
        (503, "close", "missing-ctx"),
        (504, "close", "default"),
    ] {
        let r = roundtrip(
            &mut out,
            &mut lines,
            serde_json::json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"browser_contexts","arguments":{"action":action,"name":name}}}),
        )
        .await;
        assert!(
            r.get("error").is_some() || r["result"]["isError"] == true,
            "expected error for {action}({name}): {r}"
        );
    }

    let check_def = roundtrip(
        &mut out,
        &mut lines,
        serde_json::json!({"jsonrpc":"2.0","id":600,"method":"tools/call","params":{"name":"browser_evaluate","arguments":{"expression":"document.cookie + '|' + localStorage.getItem('ctx')"}}}),
    )
    .await;
    assert_tool_ok(&check_def);
    assert!(
        tool_text(&check_def).contains("user=default|default"),
        "default state corrupted after 10 context churns: {check_def}"
    );

    drop(out);
    drop(lines);
    server.await.unwrap();
}

#[tokio::test]
async fn torture_navigation_history_wait_for_timeouts_and_error_recovery() {
    if flashbrowse_cdp::find_chrome(None).is_err() {
        eprintln!("SKIP: no Chromium binary found");
        return;
    }

    let port = spawn_raw_http(|path, _| {
        let body = if path == "/step4" {
            r#"<!doctype html><html><head><title>Step 4</title></head><body><h1>Step 4</h1><div id="host"></div><script>
setTimeout(() => {
  const h = document.getElementById('host');
  const r = h.attachShadow({mode: 'open'});
  r.innerHTML = '<button class="shadow-ready" onclick="this.textContent=\'Done\'">ShadowGo</button>';
}, 180);
</script></body></html>"#
                .to_string()
        } else {
            let n = path.trim_start_matches("/step");
            format!(
                "<!doctype html><html><head><title>Step {n}</title></head><body><h1>Step {n}</h1><button onclick=\"this.textContent='Clicked {n}'\">Btn {n}</button></body></html>"
            )
        };
        ("200 OK", "text/html; charset=utf-8", body)
    });

    let (mut out, mut lines, server) = init_mcp_with_config(SessionConfig::default()).await;
    for step in 1..=4 {
        assert_tool_ok(
            &roundtrip(
                &mut out,
                &mut lines,
                serde_json::json!({"jsonrpc":"2.0","id":10 + step,"method":"tools/call","params":{"name":"browser_open","arguments":{"url":format!("http://127.0.0.1:{port}/step{step}")}}}),
            )
            .await,
        );
    }

    for i in (1..=3).rev() {
        let b = roundtrip(
            &mut out,
            &mut lines,
            serde_json::json!({"jsonrpc":"2.0","id":20 + i,"method":"tools/call","params":{"name":"browser_navigate","arguments":{"action":"back"}}}),
        )
        .await;
        assert_tool_ok(&b);
        assert!(tool_text(&b).contains(&format!("Step {i}")));
    }

    // Drain back through the initial about:blank entry (index 0) and confirm back past 0 errors cleanly.
    let mut hit_start = false;
    for extra in 0..3 {
        let b = roundtrip(
            &mut out,
            &mut lines,
            serde_json::json!({"jsonrpc":"2.0","id":30 + extra,"method":"tools/call","params":{"name":"browser_navigate","arguments":{"action":"back"}}}),
        )
        .await;
        if b["result"]["isError"] == true {
            hit_start = true;
            break;
        }
    }
    assert!(
        hit_start,
        "expected back past history start to return isError"
    );

    // Forward back up to Step 4.
    for extra in 0..5 {
        let f = roundtrip(
            &mut out,
            &mut lines,
            serde_json::json!({"jsonrpc":"2.0","id":40 + extra,"method":"tools/call","params":{"name":"browser_navigate","arguments":{"action":"forward"}}}),
        )
        .await;
        assert_tool_ok(&f);
        if tool_text(&f).contains("Step 4") {
            break;
        }
    }

    // Forward past tip must fail cleanly.
    let past_end = roundtrip(
        &mut out,
        &mut lines,
        serde_json::json!({"jsonrpc":"2.0","id":50,"method":"tools/call","params":{"name":"browser_navigate","arguments":{"action":"forward"}}}),
    )
    .await;
    assert_eq!(past_end["result"]["isError"], true);

    // Reload ignores cache cleanly.
    assert_tool_ok(
        &roundtrip(
            &mut out,
            &mut lines,
            serde_json::json!({"jsonrpc":"2.0","id":51,"method":"tools/call","params":{"name":"browser_navigate","arguments":{"action":"reload","ignore_cache":true}}}),
        )
        .await,
    );

    // Invalid wait_for param combos (-32602).
    let two_args = roundtrip(
        &mut out,
        &mut lines,
        serde_json::json!({"jsonrpc":"2.0","id":52,"method":"tools/call","params":{"name":"browser_wait_for","arguments":{"text":"x","url":"y"}}}),
    )
    .await;
    assert_eq!(two_args["error"]["code"], -32602);

    // Timeout on missing text and invalid CSS selector must recover cleanly.
    let miss_txt = roundtrip(
        &mut out,
        &mut lines,
        serde_json::json!({"jsonrpc":"2.0","id":53,"method":"tools/call","params":{"name":"browser_wait_for","arguments":{"text":"never-there-xyz","timeout_ms":200}}}),
    )
    .await;
    assert_eq!(miss_txt["result"]["isError"], true);

    let bad_sel = roundtrip(
        &mut out,
        &mut lines,
        serde_json::json!({"jsonrpc":"2.0","id":54,"method":"tools/call","params":{"name":"browser_wait_for","arguments":{"selector":"[unclosed","timeout_ms":200}}}),
    )
    .await;
    assert_eq!(bad_sel["result"]["isError"], true);

    // Shadow DOM selector still succeeds after prior timeouts.
    let ok_sel = roundtrip(
        &mut out,
        &mut lines,
        serde_json::json!({"jsonrpc":"2.0","id":55,"method":"tools/call","params":{"name":"browser_wait_for","arguments":{"selector":".shadow-ready","timeout_ms":3000}}}),
    )
    .await;
    assert_tool_ok(&ok_sel);
    assert!(tool_text(&ok_sel).contains("ShadowGo"));

    drop(out);
    drop(lines);
    server.await.unwrap();
}

#[tokio::test]
async fn torture_rich_inputs_framework_clickables_bounds_and_screenshot_restore() {
    if flashbrowse_cdp::find_chrome(None).is_err() {
        eprintln!("SKIP: no Chromium binary found");
        return;
    }

    let root = std::env::temp_dir().join(format!("flashbrowse-break-v15-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let cfg = SessionConfig {
        mcp_file_root: Some(root.clone()),
        ..Default::default()
    };

    let mut divs = String::new();
    for i in 1..=40 {
        divs.push_str(&format!(
            "<div id=\"ael{i}\" style=\"cursor:pointer;display:inline-block;margin:2px\">AEL {i}</div>"
        ));
    }
    let page = format!(
        r#"<!doctype html><html><head><meta name="viewport" content="width=device-width,initial-scale=1"><title>Break15</title></head><body>
{divs}
<span id="reactSpan" style="display:inline-block;padding:4px">ReactClick</span>
<button id="dbl">DblTarget</button>
<button id="ctx">CtxTarget</button>
<button id="drag">DragSrc</button>
<button id="drop">DropDst</button>
<div id="scroller" tabindex="0" style="width:200px;height:60px;overflow:auto"><div style="height:500px">ScrollBody</div></div>
<pre id="state"></pre>
<script>
const log = (s) => {{ document.getElementById('state').textContent += s + ';'; }};
for (let i = 1; i <= 40; i++) {{
  document.getElementById('ael' + i).addEventListener('click', () => log('ael' + i));
}}
const rs = document.getElementById('reactSpan');
rs.__reactProps$test99 = {{ onClick: () => log('reactOk') }};
rs.addEventListener('click', () => {{}});
document.getElementById('dbl').addEventListener('dblclick', () => log('dblOk'));
document.getElementById('ctx').addEventListener('contextmenu', (e) => {{ e.preventDefault(); log('ctxOk'); }});
let sawDragMoveButtons = false;
window.addEventListener('mousemove', (e) => {{ if (e.buttons === 1) sawDragMoveButtons = true; }});
document.getElementById('drop').addEventListener('mouseup', () => log(sawDragMoveButtons ? 'dropOk' : 'dropNoBtn'));
window.addEventListener('keydown', (e) => {{
  if (e.ctrlKey && e.key.toLowerCase() === 'k') {{ e.preventDefault(); log('ctrlK'); }}
}});
</script></body></html>"#
    );

    let port = spawn_raw_http(move |_, _| ("200 OK", "text/html; charset=utf-8", page.clone()));
    let (mut out, mut lines, server) = init_mcp_with_config(cfg).await;

    let open = roundtrip(
        &mut out,
        &mut lines,
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"browser_open","arguments":{"url":format!("http://127.0.0.1:{port}/")}}}),
    )
    .await;
    assert_tool_ok(&open);
    let tree = tool_text(&open);
    assert!(
        tree.contains("AEL 40") && tree.contains("ReactClick"),
        "framework clickables missing:\n{tree}"
    );

    let ael40_id = action_id_for(&tree, "AEL 40");
    let dbl_id = action_id_for(&tree, "DblTarget");
    let ctx_id = action_id_for(&tree, "CtxTarget");
    let drag_id = action_id_for(&tree, "DragSrc");
    let drop_id = action_id_for(&tree, "DropDst");

    assert_tool_ok(
        &roundtrip(
            &mut out,
            &mut lines,
            serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"browser_act","arguments":{"action":"click","action_id":ael40_id}}}),
        )
        .await,
    );
    assert_tool_ok(
        &roundtrip(
            &mut out,
            &mut lines,
            serde_json::json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"browser_act","arguments":{"action":"double_click","action_id":dbl_id}}}),
        )
        .await,
    );
    assert_tool_ok(
        &roundtrip(
            &mut out,
            &mut lines,
            serde_json::json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"browser_act","arguments":{"action":"right_click","action_id":ctx_id}}}),
        )
        .await,
    );
    assert_tool_ok(
        &roundtrip(
            &mut out,
            &mut lines,
            serde_json::json!({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"browser_act","arguments":{"action":"drag_and_drop","action_id":drag_id,"target_action_id":drop_id}}}),
        )
        .await,
    );
    assert_tool_ok(
        &roundtrip(
            &mut out,
            &mut lines,
            serde_json::json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"browser_act","arguments":{"action":"press_key","value":"Control+k"}}}),
        )
        .await,
    );

    // Invalid input bounds must be cleanly rejected.
    let bad_drag = roundtrip(
        &mut out,
        &mut lines,
        serde_json::json!({"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"browser_act","arguments":{"action":"drag_and_drop","action_id":drag_id,"target_action_id":99999}}}),
    )
    .await;
    assert_eq!(bad_drag["result"]["isError"], true);

    let bad_scroll = roundtrip(
        &mut out,
        &mut lines,
        serde_json::json!({"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"browser_act","arguments":{"action":"scroll","value":"down","amount":-100}}}),
    )
    .await;
    assert_eq!(bad_scroll["result"]["isError"], true);

    let bad_chord = roundtrip(
        &mut out,
        &mut lines,
        serde_json::json!({"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"browser_act","arguments":{"action":"press_key","value":"BogusMod+k"}}}),
    )
    .await;
    assert_eq!(bad_chord["result"]["isError"], true);

    let bad_vp = roundtrip(
        &mut out,
        &mut lines,
        serde_json::json!({"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"browser_emulate","arguments":{"viewport":{"width":0,"height":800}}}}),
    )
    .await;
    assert_eq!(bad_vp["error"]["code"], -32602);

    let bad_geo = roundtrip(
        &mut out,
        &mut lines,
        serde_json::json!({"jsonrpc":"2.0","id":12,"method":"tools/call","params":{"name":"browser_emulate","arguments":{"geolocation":{"latitude":120.0,"longitude":0.0}}}}),
    )
    .await;
    assert_eq!(bad_geo["error"]["code"], -32602);

    let bad_perms = roundtrip(
        &mut out,
        &mut lines,
        serde_json::json!({"jsonrpc":"2.0","id":120,"method":"tools/call","params":{"name":"browser_emulate","arguments":{"permissions":{"grants":[123]}}}}),
    )
    .await;
    assert_eq!(bad_perms["error"]["code"], -32602);

    let bad_pdf_scale = roundtrip(
        &mut out,
        &mut lines,
        serde_json::json!({"jsonrpc":"2.0","id":13,"method":"tools/call","params":{"name":"browser_pdf","arguments":{"name":"bad-scale","scale":9.0}}}),
    )
    .await;
    assert_eq!(bad_pdf_scale["error"]["code"], -32602);

    let bad_pdf_name = roundtrip(
        &mut out,
        &mut lines,
        serde_json::json!({"jsonrpc":"2.0","id":14,"method":"tools/call","params":{"name":"browser_pdf","arguments":{"name":"../evil"}}}),
    )
    .await;
    assert_eq!(bad_pdf_name["error"]["code"], -32602);

    let bad_pdf_con = roundtrip(
        &mut out,
        &mut lines,
        serde_json::json!({"jsonrpc":"2.0","id":140,"method":"tools/call","params":{"name":"browser_pdf","arguments":{"name":"CON"}}}),
    )
    .await;
    assert_eq!(bad_pdf_con["error"]["code"], -32602);

    let bad_clip = roundtrip(
        &mut out,
        &mut lines,
        serde_json::json!({"jsonrpc":"2.0","id":15,"method":"tools/call","params":{"name":"browser_screenshot","arguments":{"action_id":99999}}}),
    )
    .await;
    assert_eq!(bad_clip["result"]["isError"], true);

    // Valid emulate + reset clears timezone back from Asia/Tokyo + clipped screenshot + PDF export.
    let tz_before = roundtrip(
        &mut out,
        &mut lines,
        serde_json::json!({"jsonrpc":"2.0","id":159,"method":"tools/call","params":{"name":"browser_evaluate","arguments":{"expression":"Intl.DateTimeFormat().resolvedOptions().timeZone"}}}),
    )
    .await;
    let orig_tz = tool_text(&tz_before);
    let test_tz = if orig_tz.contains("Asia/Tokyo") {
        "America/New_York"
    } else {
        "Asia/Tokyo"
    };
    assert_tool_ok(
        &roundtrip(
            &mut out,
            &mut lines,
            serde_json::json!({"jsonrpc":"2.0","id":16,"method":"tools/call","params":{"name":"browser_emulate","arguments":{"viewport":{"width":412,"height":915,"device_scale_factor":2.0,"mobile":true},"timezone":test_tz,"locale":"ja-JP"}}}),
        )
        .await,
    );
    let tz_overridden = roundtrip(
        &mut out,
        &mut lines,
        serde_json::json!({"jsonrpc":"2.0","id":161,"method":"tools/call","params":{"name":"browser_evaluate","arguments":{"expression":"Intl.DateTimeFormat().resolvedOptions().timeZone"}}}),
    )
    .await;
    assert!(
        tool_text(&tz_overridden).contains(test_tz),
        "timezone override not applied: {tz_overridden}"
    );
    assert_tool_ok(
        &roundtrip(
            &mut out,
            &mut lines,
            serde_json::json!({"jsonrpc":"2.0","id":162,"method":"tools/call","params":{"name":"browser_emulate","arguments":{"reset":true}}}),
        )
        .await,
    );
    let tz_reset = roundtrip(
        &mut out,
        &mut lines,
        serde_json::json!({"jsonrpc":"2.0","id":163,"method":"tools/call","params":{"name":"browser_evaluate","arguments":{"expression":"Intl.DateTimeFormat().resolvedOptions().timeZone"}}}),
    )
    .await;
    assert_eq!(
        tool_text(&tz_reset),
        orig_tz,
        "reset_emulation did not restore timezone"
    );
    let ok_clip = roundtrip(
        &mut out,
        &mut lines,
        serde_json::json!({"jsonrpc":"2.0","id":17,"method":"tools/call","params":{"name":"browser_screenshot","arguments":{"action_id":dbl_id}}}),
    )
    .await;
    assert_tool_ok(&ok_clip);
    assert_eq!(ok_clip["result"]["content"][0]["type"], "image");

    let ok_pdf = roundtrip(
        &mut out,
        &mut lines,
        serde_json::json!({"jsonrpc":"2.0","id":18,"method":"tools/call","params":{"name":"browser_pdf","arguments":{"name":"torture-v15"}}}),
    )
    .await;
    assert_tool_ok(&ok_pdf);

    let state = roundtrip(
        &mut out,
        &mut lines,
        serde_json::json!({"jsonrpc":"2.0","id":19,"method":"tools/call","params":{"name":"browser_evaluate","arguments":{"expression":"document.getElementById('state').textContent"}}}),
    )
    .await;
    let st = tool_text(&state);
    for want in ["ael40", "dblOk", "ctxOk", "dropOk", "ctrlK"] {
        assert!(st.contains(want), "missing {want} in state: {st}");
    }

    drop(out);
    drop(lines);
    server.await.unwrap();
    let _ = std::fs::remove_dir_all(root);
}
