//! Breaking-point stress tests for v1.4.0 capabilities over the full
//! MCP -> Core -> POSIX pipe -> Chrome stack.

use std::io::Write;
use std::net::TcpListener;

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

fn spawn_test_http<F>(handler: F) -> u16
where
    F: Fn(&str) -> String + Send + Sync + 'static,
{
    use std::io::{BufRead, BufReader as StdBufReader};
    use std::net::Shutdown;
    use std::sync::Arc;
    use std::time::Duration;

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    let handler = Arc::new(handler);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let handler = Arc::clone(&handler);
            std::thread::spawn(move || {
                let mut stream = stream;
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let mut request_line = String::new();
                if let Ok(cloned) = stream.try_clone() {
                    let mut reader = StdBufReader::new(cloned);
                    if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
                        return;
                    }
                    let mut line = String::new();
                    while reader.read_line(&mut line).unwrap_or(0) > 0 {
                        if line == "\r\n" || line == "\n" {
                            break;
                        }
                        line.clear();
                    }
                }
                let path = request_line.split_whitespace().nth(1).unwrap_or("/");
                let body = handler(path);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
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

async fn init_mcp() -> (
    tokio::io::DuplexStream,
    BufReader<tokio::io::DuplexStream>,
    tokio::task::JoinHandle<()>,
) {
    let (server_in, mut client_out) = duplex(1024 * 1024);
    let (server_out, client_in) = duplex(1024 * 1024);
    let mut client_lines = BufReader::new(client_in);
    let server = tokio::spawn(async move {
        serve(server_in, server_out, SessionConfig::default())
            .await
            .expect("serve");
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
async fn torture_50_field_fill_form_and_mid_batch_dom_removal() {
    if flashbrowse_cdp::find_chrome(None).is_err() {
        eprintln!("SKIP: no Chromium binary found");
        return;
    }

    let port = spawn_test_http(|path| {
        if path.starts_with("/mutate") {
            let mut inputs = String::new();
            for i in 1..=20 {
                if i == 10 {
                    inputs.push_str(
                        "<input id=\"f10\" aria-label=\"Field 10\" oninput=\"document.getElementById('f15')?.remove()\" />",
                    );
                } else {
                    inputs.push_str(&format!("<input id=\"f{i}\" aria-label=\"Field {i}\" />"));
                }
            }
            format!("<html><body><form>{inputs}</form></body></html>")
        } else {
            let mut inputs = String::new();
            for i in 1..=50 {
                inputs.push_str(&format!("<input id=\"f{i}\" aria-label=\"Field {i}\" />"));
            }
            format!("<html><body><form>{inputs}</form></body></html>")
        }
    });

    let (mut out, mut lines, server) = init_mcp().await;

    let open = roundtrip(
        &mut out,
        &mut lines,
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"browser_open","arguments":{"url":format!("http://127.0.0.1:{port}/full")}}}),
    )
    .await;
    assert_tool_ok(&open);

    let fields: Vec<Value> = (1..=50)
        .map(|i| serde_json::json!({"action_id": i, "value": format!("val-{i}")}))
        .collect();
    let fill = roundtrip(
        &mut out,
        &mut lines,
        serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"browser_fill_form","arguments":{"fields": fields}}}),
    )
    .await;
    assert_tool_ok(&fill);
    let fill_txt = tool_text(&fill);
    assert!(
        fill_txt.contains("val-1") && fill_txt.contains("val-50"),
        "50-field fill missing values"
    );

    let eval = roundtrip(
        &mut out,
        &mut lines,
        serde_json::json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"browser_evaluate","arguments":{"expression":"Array.from({length: 50}, (_, i) => document.getElementById('f' + (i + 1)).value === 'val-' + (i + 1)).every(Boolean)"}}}),
    )
    .await;
    assert_tool_ok(&eval);
    assert!(
        tool_text(&eval).contains("true"),
        "not all 50 inputs matched: {eval}"
    );

    // Now test DOM mutation mid-batch: f10 removes f15 on input, so f15 must fail cleanly.
    let open2 = roundtrip(
        &mut out,
        &mut lines,
        serde_json::json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"browser_open","arguments":{"url":format!("http://127.0.0.1:{port}/mutate")}}}),
    )
    .await;
    assert_tool_ok(&open2);

    let fields2: Vec<Value> = (1..=20)
        .map(|i| serde_json::json!({"action_id": i, "value": format!("m-{i}")}))
        .collect();
    let fill2 = roundtrip(
        &mut out,
        &mut lines,
        serde_json::json!({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"browser_fill_form","arguments":{"fields": fields2}}}),
    )
    .await;
    let txt2 = tool_text(&fill2);
    assert!(
        txt2.contains("completed fields [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14]; stopped at field 15:"),
        "expected partial failure at [15] after f10 removed f15: {txt2}"
    );
    assert!(
        !txt2.contains("Field 15"),
        "refreshed tree should reflect removed Field 15"
    );

    drop(out);
    drop(lines);
    server.await.unwrap();
}

#[tokio::test]
async fn torture_popup_storm_x10_open_switch_and_close() {
    if flashbrowse_cdp::find_chrome(None).is_err() {
        eprintln!("SKIP: no Chromium binary found");
        return;
    }

    let port = spawn_test_http(|path| {
        if let Some(id) = path.strip_prefix("/popup/") {
            format!("<html><body><h1>Popup {id}</h1><button onclick=\"document.body.append('OK')\">Act {id}</button></body></html>")
        } else {
            "<html><body><h1>Opener</h1><button onclick=\"window.__n=(window.__n||0)+1;window.open('/popup/'+window.__n,'_blank')\">Spawn</button></body></html>".to_string()
        }
    });

    let (mut out, mut lines, server) = init_mcp().await;
    let open = roundtrip(
        &mut out,
        &mut lines,
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"browser_open","arguments":{"url":format!("http://127.0.0.1:{port}/")}}}),
    )
    .await;
    assert_tool_ok(&open);

    let tabs0 = roundtrip(
        &mut out,
        &mut lines,
        serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"browser_tabs","arguments":{"action":"list"}}}),
    )
    .await;
    let opener_id = serde_json::from_str::<Value>(&tool_text(&tabs0))
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["active"] == true)
        .unwrap()["target_id"]
        .as_str()
        .unwrap()
        .to_string();

    let mut popup_ids = Vec::new();
    for i in 1..=10 {
        let sel = roundtrip(
            &mut out,
            &mut lines,
            serde_json::json!({"jsonrpc":"2.0","id":100 + i * 3,"method":"tools/call","params":{"name":"browser_tabs","arguments":{"action":"select","target_id":opener_id}}}),
        )
        .await;
        assert_tool_ok(&sel);

        let click = roundtrip(
            &mut out,
            &mut lines,
            serde_json::json!({"jsonrpc":"2.0","id":101 + i * 3,"method":"tools/call","params":{"name":"browser_act","arguments":{"action_id":1,"action":"click"}}}),
        )
        .await;
        assert_tool_ok(&click);
        assert!(
            tool_text(&click).contains(&format!("Popup {i}")),
            "popup {i} not focused: {}",
            tool_text(&click)
        );

        let tabs = roundtrip(
            &mut out,
            &mut lines,
            serde_json::json!({"jsonrpc":"2.0","id":102 + i * 3,"method":"tools/call","params":{"name":"browser_tabs","arguments":{"action":"list"}}}),
        )
        .await;
        let tid = serde_json::from_str::<Value>(&tool_text(&tabs))
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["active"] == true)
            .unwrap()["target_id"]
            .as_str()
            .unwrap()
            .to_string();
        popup_ids.push(tid);
    }

    for (idx, tid) in popup_ids.iter().enumerate() {
        let close = roundtrip(
            &mut out,
            &mut lines,
            serde_json::json!({"jsonrpc":"2.0","id":500 + idx,"method":"tools/call","params":{"name":"browser_tabs","arguments":{"action":"close","target_id":tid}}}),
        )
        .await;
        assert_tool_ok(&close);
    }

    let tabs_end = roundtrip(
        &mut out,
        &mut lines,
        serde_json::json!({"jsonrpc":"2.0","id":600,"method":"tools/call","params":{"name":"browser_tabs","arguments":{"action":"list"}}}),
    )
    .await;
    let remaining: Vec<Value> = serde_json::from_str(&tool_text(&tabs_end)).unwrap();
    assert!(
        remaining
            .iter()
            .any(|t| t["target_id"] == opener_id && t["active"] == true),
        "opener should be active after closing all popups: {remaining:?}"
    );

    drop(out);
    drop(lines);
    server.await.unwrap();
}

#[tokio::test]
async fn torture_live_policy_thrash_x30_with_screenshots() {
    if flashbrowse_cdp::find_chrome(None).is_err() {
        eprintln!("SKIP: no Chromium binary found");
        return;
    }

    let (mut out, mut lines, server) = init_mcp().await;
    let policies = ["agent-default", "text-only", "full-render"];
    let mut initial_tid = String::new();

    for i in 0..30 {
        let policy = policies[i % policies.len()];
        let open = roundtrip(
            &mut out,
            &mut lines,
            serde_json::json!({
                "jsonrpc":"2.0",
                "id": 10 + i * 2,
                "method":"tools/call",
                "params":{"name":"browser_open","arguments":{"url":format!("data:text/html,<h1>Iter {i}</h1>"),"resource_policy":policy}}
            }),
        )
        .await;
        assert_tool_ok(&open);
        assert!(tool_text(&open).contains(&format!("Iter {i}")));

        if i == 0 || i % 5 == 4 {
            let tabs = roundtrip(
                &mut out,
                &mut lines,
                serde_json::json!({"jsonrpc":"2.0","id":1000 + i,"method":"tools/call","params":{"name":"browser_tabs","arguments":{"action":"list"}}}),
            )
            .await;
            let tid = serde_json::from_str::<Value>(&tool_text(&tabs))
                .unwrap()
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["active"] == true)
                .unwrap()["target_id"]
                .as_str()
                .unwrap()
                .to_string();
            if initial_tid.is_empty() {
                initial_tid = tid;
            } else {
                assert_eq!(tid, initial_tid, "Chrome restarted at policy switch {i}");
            }

            let shot = roundtrip(
                &mut out,
                &mut lines,
                serde_json::json!({"jsonrpc":"2.0","id":2000 + i,"method":"tools/call","params":{"name":"browser_screenshot","arguments":{}}}),
            )
            .await;
            assert_tool_ok(&shot);
            assert_eq!(shot["result"]["content"][0]["type"], "image");
        }
    }

    drop(out);
    drop(lines);
    server.await.unwrap();
}

#[tokio::test]
async fn torture_deep_shadow_dom_40_clickable_divs_and_4_iframes() {
    if flashbrowse_cdp::find_chrome(None).is_err() {
        eprintln!("SKIP: no Chromium binary found");
        return;
    }

    let port = spawn_test_http(|path| {
        if let Some(id) = path.strip_prefix("/frame/") {
            format!("<html><body><p>FrameBody {id}</p><button onclick=\"document.body.append('F{id}_OK')\">FrameBtn {id}</button></body></html>")
        } else {
            let mut divs = String::new();
            for i in 1..=40 {
                divs.push_str(&format!(
                    "<div id=\"d{i}\" onclick=\"this.textContent='Div{i}Done'\">ClickDiv {i}</div>"
                ));
            }
            let mut frames = String::new();
            for i in 1..=4 {
                frames.push_str(&format!("<iframe src=\"/frame/{i}\"></iframe>"));
            }
            format!(
                "<!doctype html><html><body><div id=\"s1\"></div>{divs}{frames}<script>
                let host = document.getElementById('s1');
                for (let lvl = 1; lvl <= 4; lvl++) {{
                  const r = host.attachShadow({{mode: 'open'}});
                  r.innerHTML = '<h2>Shadow L' + lvl + '</h2><p>ShadowPara ' + lvl + '</p>' +
                    (lvl < 4 ? '<div class=\"next\"></div>' : '<button onclick=\"this.textContent=\\'DeepBtnDone\\'\">DeepShadowBtn</button>');
                  host = r.querySelector('.next');
                }}
                </script></body></html>"
            )
        }
    });

    let (mut out, mut lines, server) = init_mcp().await;
    let open = roundtrip(
        &mut out,
        &mut lines,
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"browser_open","arguments":{"url":format!("http://127.0.0.1:{port}/")}}}),
    )
    .await;
    assert_tool_ok(&open);
    let tree = tool_text(&open);

    for lvl in 1..=4 {
        assert!(
            tree.contains(&format!("Shadow L{lvl}")),
            "missing Shadow L{lvl}"
        );
    }
    assert!(tree.contains("DeepShadowBtn"));
    for i in 1..=40 {
        assert_eq!(
            tree.matches(&format!("ClickDiv {i}\"")).count(),
            1,
            "ClickDiv {i} missing or duplicated"
        );
    }
    for i in 1..=4 {
        assert!(
            tree.contains(&format!("FrameBody {i}")),
            "missing FrameBody {i}"
        );
    }

    // Click the 40th clickable div and verify it mutates.
    let d40_id: u32 = tree
        .lines()
        .find(|l| l.contains("\"ClickDiv 40\""))
        .and_then(|l| l.trim_start().strip_prefix('['))
        .and_then(|r| r.split(']').next())
        .and_then(|s| s.parse().ok())
        .expect("ClickDiv 40 action_id");
    let click = roundtrip(
        &mut out,
        &mut lines,
        serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"browser_act","arguments":{"action_id":d40_id,"action":"click"}}}),
    )
    .await;
    assert_tool_ok(&click);
    assert!(tool_text(&click).contains("Div40Done"));

    let md = roundtrip(
        &mut out,
        &mut lines,
        serde_json::json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"browser_extract_markdown","arguments":{}}}),
    )
    .await;
    assert_tool_ok(&md);
    let md_txt = tool_text(&md);
    assert!(
        md_txt.contains("ShadowPara 4") && md_txt.contains("FrameBody 4"),
        "markdown missing deep shadow or iframe content:\n{md_txt}"
    );

    drop(out);
    drop(lines);
    server.await.unwrap();
}

#[tokio::test]
async fn torture_large_pipe_payload_2000_rows() {
    if flashbrowse_cdp::find_chrome(None).is_err() {
        eprintln!("SKIP: no Chromium binary found");
        return;
    }

    let port = spawn_test_http(|_| {
        let mut rows = String::with_capacity(160_000);
        for i in 1..=2000 {
            rows.push_str(&format!(
                "<tr><td>Row {i}</td><td><a href=\"#r{i}\" onclick=\"document.title='Clicked {i}';return false;\">Go {i}</a></td></tr>"
            ));
        }
        format!("<!doctype html><html><head><title>BigTable</title></head><body><table>{rows}</table></body></html>")
    });

    let (mut out, mut lines, server) = init_mcp().await;
    let open = roundtrip(
        &mut out,
        &mut lines,
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"browser_open","arguments":{"url":format!("http://127.0.0.1:{port}/")}}}),
    )
    .await;
    assert_tool_ok(&open);
    let tree = tool_text(&open);
    assert!(
        tree.contains("Row 1") && tree.contains("Row 2000"),
        "2000-row table truncated"
    );

    let last_id: u32 = tree
        .lines()
        .find(|l| l.contains("\"Go 2000\""))
        .and_then(|l| l.trim_start().strip_prefix('['))
        .and_then(|r| r.split(']').next())
        .and_then(|s| s.parse().ok())
        .expect("Go 2000 action_id");
    let click = roundtrip(
        &mut out,
        &mut lines,
        serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"browser_act","arguments":{"action_id":last_id,"action":"click"}}}),
    )
    .await;
    assert_tool_ok(&click);
    assert!(tool_text(&click).contains("Title: Clicked 2000"));

    drop(out);
    drop(lines);
    server.await.unwrap();
}
