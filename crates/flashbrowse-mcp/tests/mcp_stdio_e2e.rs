//! End-to-end MCP JSON-RPC validation over duplex pipes.
//! Drives `serve()` over in-memory I/O without spawning the CLI binary.
//! Requires system Chromium - skipped when absent.

use flashbrowse_core::SessionConfig;
use flashbrowse_mcp::server::serve;
use serde_json::Value;
use tokio::io::{duplex, AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Send one JSON-RPC line and await the next response line as JSON.
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
    assert!(!line.is_empty(), "empty response (EOF?)");
    // Stdout purity: every line must be valid JSON.
    let v: Value = serde_json::from_str(line.trim()).expect("stdout line must be JSON");
    assert_eq!(v.get("jsonrpc").and_then(|x| x.as_str()), Some("2.0"));
    v
}

/// Spawn a Windows-safe test HTTP server that drains full request headers,
/// handles concurrent preconnect sockets per thread, and performs a graceful write shutdown.
fn spawn_raw_http<F>(handler: F) -> u16
where
    F: Fn(&str, &str) -> (&'static str, &'static str, String) + Send + Sync + 'static,
{
    use std::io::{BufRead, BufReader as StdBufReader, Write};
    use std::net::{Shutdown, TcpListener};
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

#[tokio::test]
async fn test_mcp_stdio_e2e() {
    if flashbrowse_cdp::find_chrome(None).is_err() {
        eprintln!("SKIP: no Chromium binary found");
        return;
    }

    let (server_in, mut client_out) = duplex(64 * 1024);
    let (server_out, client_in) = duplex(64 * 1024);
    let mut client_lines = BufReader::new(client_in);

    let server = tokio::spawn(async move {
        serve(server_in, server_out, SessionConfig::default())
            .await
            .expect("serve")
    });

    // initialize
    let resp = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "e2e"}} }),
    )
    .await;
    assert!(resp.get("error").is_none(), "init error: {resp}");
    let result = resp.get("result").expect("init result");
    assert!(
        result
            .get("protocolVersion")
            .and_then(|v| v.as_str())
            .is_some(),
        "missing protocolVersion: {result}"
    );
    assert_eq!(
        result
            .get("serverInfo")
            .and_then(|s| s.get("name"))
            .and_then(|v| v.as_str()),
        Some("flashbrowse")
    );

    // tools/list
    let resp = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
    )
    .await;
    let tools = resp
        .get("result")
        .and_then(|r| r.get("tools"))
        .and_then(|t| t.as_array())
        .expect("tools list");
    assert_eq!(tools.len(), 18);
    let names: Vec<String> = tools
        .iter()
        .filter_map(|t| t.get("name").and_then(|v| v.as_str()).map(str::to_string))
        .collect();
    for want in [
        "browser_open",
        "browser_act",
        "browser_fill_form",
        "browser_extract_markdown",
        "browser_evaluate",
        "browser_screenshot",
        "browser_tabs",
        "browser_storage_state",
        "browser_upload",
        "browser_downloads",
    ] {
        assert!(names.contains(&want.to_string()), "missing tool {want}");
    }

    // browser_open → numbered tree contains [1] Button: "Submit"
    let url = "data:text/html,<html><body><button>Submit</button></body></html>";
    let resp = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "browser_open", "arguments": {"url": url}}}),
    )
    .await;
    let content = tool_text(&resp);
    assert!(
        content.contains("[1] Button: \"Submit\""),
        "browser_open tree missing [1] Button: {content}"
    );

    // browser_act click [1] → success
    let resp = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "browser_act", "arguments": {"action_id": 1, "action": "click"}}}),
    )
    .await;
    assert_tool_ok(&resp);

    // browser_act hover + scroll + press_key should also succeed (no error).
    for (id, action, value) in [
        (5, "hover", serde_json::json!(null)),
        (6, "scroll", serde_json::json!("down")),
        (7, "press_key", serde_json::json!("Enter")),
    ] {
        let args = serde_json::json!({"action_id": 1, "action": action, "value": value});
        let resp = roundtrip(
            &mut client_out,
            &mut client_lines,
            serde_json::json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": "browser_act", "arguments": args}}),
        )
        .await;
        assert_tool_ok(&resp);
    }

    // browser_extract_markdown → text
    let resp = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc": "2.0", "id": 8, "method": "tools/call", "params": {"name": "browser_extract_markdown", "arguments": {}}}),
    )
    .await;
    assert_tool_ok(&resp);

    // browser_screenshot → image content block
    let resp = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc": "2.0", "id": 9, "method": "tools/call", "params": {"name": "browser_screenshot", "arguments": {"full_page": false}}}),
    )
    .await;
    let first = resp
        .get("result")
        .and_then(|r| r.get("content"))
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .expect("image content");
    assert_eq!(
        first.get("type").and_then(|v| v.as_str()),
        Some("image"),
        "screenshot should be image content: {first}"
    );

    // Opening a popup by user gesture focuses it and leaves the opener selectable.
    let popup_page = "data:text/html,<html><body><button onclick=\"const w=window.open('about:blank');w.document.body.innerHTML='<h1>Popup content</h1>'\">Open popup</button></body></html>";
    let resp = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"browser_open","arguments":{"url":popup_page}}}),
    ).await;
    assert!(tool_text(&resp).contains("Open popup"));
    let resp = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"browser_act","arguments":{"action_id":1,"action":"click"}}}),
    ).await;
    assert!(
        tool_text(&resp).contains("Popup content"),
        "popup was not focused: {resp}"
    );

    let resp = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":12,"method":"tools/call","params":{"name":"browser_tabs","arguments":{"action":"list"}}}),
    ).await;
    let tabs: Value = serde_json::from_str(&tool_text(&resp)).expect("tab list JSON");
    let tabs = tabs.as_array().expect("tab list array");
    assert!(tabs.len() >= 2, "expected opener and popup: {tabs:?}");
    let popup_id = tabs
        .iter()
        .find(|t| t.get("active").and_then(Value::as_bool) == Some(true))
        .and_then(|t| t.get("target_id"))
        .and_then(Value::as_str)
        .expect("active popup target id")
        .to_string();
    let opener_id = tabs
        .iter()
        .find(|t| {
            t.get("url")
                .and_then(Value::as_str)
                .is_some_and(|u| u.contains("Open popup"))
        })
        .and_then(|t| t.get("target_id"))
        .and_then(Value::as_str)
        .expect("opener target id")
        .to_string();
    let resp = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":13,"method":"tools/call","params":{"name":"browser_tabs","arguments":{"action":"select","target_id":opener_id}}}),
    ).await;
    assert!(
        tool_text(&resp).contains("Open popup"),
        "failed to return to opener: {resp}"
    );
    let resp = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":14,"method":"tools/call","params":{"name":"browser_tabs","arguments":{"action":"close","target_id":popup_id}}}),
    ).await;
    assert_tool_ok(&resp);

    let dialog_page = "data:text/html,<html><body><h1>Dialog Test</h1><button onclick=\"alert('alert-note')\">Alert</button><button onclick=\"document.getElementById('result').textContent=confirm('confirm-note')?'yes':'no'\">Confirm</button><button onclick=\"document.getElementById('result').textContent=prompt('prompt-note')\">Prompt</button><p id='result'></p></body></html>";
    let resp = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":15,"method":"tools/call","params":{"name":"browser_open","arguments":{"url":dialog_page}}}),
    ).await;
    assert!(tool_text(&resp).contains("Dialog Test"), "{resp}");

    let resp = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":16,"method":"tools/call","params":{"name":"browser_act","arguments":{"action_id":1,"action":"click"}}}),
    ).await;
    assert!(
        tool_text(&resp).contains("alert dialog: accepted (alert-note)"),
        "alert hung or was not accepted: {resp}"
    );

    let resp = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":17,"method":"tools/call","params":{"name":"browser_act","arguments":{"action_id":2,"action":"click"}}}),
    ).await;
    assert!(
        tool_text(&resp).contains("confirm dialog: dismissed (confirm-note)"),
        "confirm did not use safe default: {resp}"
    );
    assert!(
        tool_text(&resp).contains("no"),
        "confirm result did not reflect dismissal: {resp}"
    );

    let resp = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":18,"method":"tools/call","params":{"name":"browser_act","arguments":{"action_id":3,"action":"click","dialog_action":"accept","prompt_text":"provided-value"}}}),
    ).await;
    assert!(
        tool_text(&resp).contains("prompt dialog: accepted (prompt-note)"),
        "prompt did not use explicit acceptance: {resp}"
    );
    assert!(
        tool_text(&resp).contains("provided-value"),
        "prompt text was not delivered: {resp}"
    );

    // ping → result
    let resp = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc": "2.0", "id": 19, "method": "ping", "params": {}}),
    )
    .await;
    assert!(resp.get("error").is_none());

    // Drop client → server loop EOF → clean teardown.
    drop(client_out);
    drop(client_lines);
    server.await.expect("server task");
}

#[tokio::test]
async fn test_mcp_delayed_http_popup_and_opener_fallback() {
    if flashbrowse_cdp::find_chrome(None).is_err() {
        eprintln!("SKIP: no Chromium binary found");
        return;
    }

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind local test server");
    let port = listener.local_addr().unwrap().port();
    let base_url = format!("http://127.0.0.1:{port}/");

    let http_task = tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 2048];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/");
                let body = if path == "/slow-popup" {
                    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                    "<html><head><title>Slow Popup</title></head><body><h1>Delayed HTTP Popup</h1><button onclick=\"document.body.append('popup-ok')\">Popup Btn</button><script>setTimeout(() => { const p = document.createElement('p'); p.textContent = 'Late Hydrated Popup Content'; document.body.append(p); }, 800)</script></body></html>"
                } else {
                    "<html><head><title>Opener Page</title></head><body><h1>Main Opener</h1><a href=\"/slow-popup\" target=\"_blank\">Open Delayed Popup</a></body></html>"
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.flush().await;
                let _ = sock.shutdown().await;
            });
        }
    });

    let (server_in, mut client_out) = duplex(64 * 1024);
    let (server_out, client_in) = duplex(64 * 1024);
    let mut client_lines = BufReader::new(client_in);

    let server = tokio::spawn(async move {
        serve(server_in, server_out, SessionConfig::default())
            .await
            .expect("serve");
    });

    let _ = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "0.1"}}
        }),
    )
    .await;
    client_out
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
        .await
        .unwrap();

    let resp = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"browser_open","arguments":{"url":base_url}}}),
    )
    .await;
    assert!(tool_text(&resp).contains("Main Opener"), "{resp}");

    // Click <a target="_blank"> pointing to a 400ms-delayed HTTP endpoint.
    let resp = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"browser_act","arguments":{"action_id":1,"action":"click"}}}),
    )
    .await;
    assert!(
        tool_text(&resp).contains("Delayed HTTP Popup"),
        "delayed HTTP popup was not focused/settled: {resp}"
    );
    assert!(
        tool_text(&resp).contains("Late Hydrated Popup Content"),
        "popup tree was returned before late hydration completed: {resp}"
    );

    // Act inside the newly-focused HTTP popup.
    let resp = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"browser_act","arguments":{"action_id":1,"action":"click"}}}),
    )
    .await;
    assert!(
        tool_text(&resp).contains("popup-ok"),
        "act inside delayed popup failed: {resp}"
    );

    // Closing the active popup must deterministically return to its opener (not chrome://newtab).
    let list_resp = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"browser_tabs","arguments":{"action":"list"}}}),
    )
    .await;
    let tabs: Value = serde_json::from_str(&tool_text(&list_resp)).expect("tab list");
    let popup_id = tabs
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t.get("active").and_then(Value::as_bool) == Some(true))
        .and_then(|t| t.get("target_id"))
        .and_then(Value::as_str)
        .unwrap()
        .to_string();

    let close_resp = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"browser_tabs","arguments":{"action":"close","target_id":popup_id}}}),
    )
    .await;
    assert!(
        tool_text(&close_resp).contains("Main Opener"),
        "closing active popup did not return to opener: {close_resp}"
    );

    drop(client_out);
    drop(client_lines);
    server.await.expect("server task");
    http_task.abort();
}

#[tokio::test]
async fn test_mcp_fill_form_and_partial_failure() {
    if flashbrowse_cdp::find_chrome(None).is_err() {
        eprintln!("SKIP: no Chromium binary found");
        return;
    }

    let (server_in, mut client_out) = duplex(64 * 1024);
    let (server_out, client_in) = duplex(64 * 1024);
    let mut client_lines = BufReader::new(client_in);

    let server = tokio::spawn(async move {
        serve(server_in, server_out, SessionConfig::default())
            .await
            .expect("serve");
    });

    let _ = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "0.1"}}
        }),
    )
    .await;
    client_out
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
        .await
        .unwrap();

    let form_page =
        "data:text/html,<form><input aria-label='First' /><input aria-label='Second' /></form>";
    let open = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"browser_open","arguments":{"url":form_page}}}),
    )
    .await;
    assert!(tool_text(&open).contains("First"));

    let filled = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({
            "jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"browser_fill_form","arguments":{"fields":[
                {"action_id": 1, "value": "Ada"},
                {"action_id": 2, "value": "Lovelace"}
            ]}}
        }),
    )
    .await;
    assert_tool_ok(&filled);
    let tree = tool_text(&filled);
    assert!(tree.contains("Ada") && tree.contains("Lovelace"), "{tree}");

    // Partial failure: field 1 succeeds ("Grace"), field 99 does not exist.
    let partial = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({
            "jsonrpc":"2.0","id":4,"method":"tools/call",
            "params":{"name":"browser_fill_form","arguments":{"fields":[
                {"action_id": 1, "value": "Grace"},
                {"action_id": 99, "value": "Hopper"}
            ]}}
        }),
    )
    .await;
    assert_eq!(
        partial
            .get("result")
            .and_then(|r| r.get("isError"))
            .and_then(Value::as_bool),
        Some(true),
        "expected partial failure error: {partial}"
    );
    let partial_text = tool_text(&partial);
    assert!(
        partial_text.contains("completed fields [1]"),
        "{partial_text}"
    );
    assert!(partial_text.contains("Grace"), "{partial_text}");

    drop(client_out);
    drop(client_lines);
    server.await.expect("server task");
}

#[tokio::test]
async fn test_mcp_storage_state_import_export() {
    if flashbrowse_cdp::find_chrome(None).is_err() {
        eprintln!("SKIP: no Chromium binary found");
        return;
    }
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let root = std::env::temp_dir().join(format!(
        "flashbrowse-storage-state-test-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).expect("create storage-state root");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind local test server");
    let port = listener.local_addr().unwrap().port();
    let url = format!("http://127.0.0.1:{port}/state");
    let http_task = tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 2048];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]);
                let path = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("/");
                let response = if path == "/download" {
                    let body = "download contents";
                    format!("HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Disposition: attachment; filename=download.txt\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body)
                } else {
                    let body = "<html><body><h1>Storage State</h1><label>Upload<input id='upload' type='file' aria-label='Upload a file'></label><a id='download' href='/download'>Download file</a></body></html>";
                    format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body)
                };
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.flush().await;
                let _ = sock.shutdown().await;
            });
        }
    });
    let state = serde_json::json!({
        "cookies": [{"name": "auth", "value": "cookie-value", "domain": "127.0.0.1", "path": "/", "expires": -1, "httpOnly": false, "secure": false, "sameSite": "Lax"}],
        "origins": [{"origin": format!("http://127.0.0.1:{port}"), "localStorage": [{"name": "token", "value": "storage-value"}]}]
    });
    std::fs::write(root.join("state.json"), serde_json::to_vec(&state).unwrap())
        .expect("write fixture state");
    std::fs::write(root.join("upload.txt"), "upload contents").expect("write upload fixture");

    let (server_in, mut client_out) = duplex(64 * 1024);
    let (server_out, client_in) = duplex(64 * 1024);
    let mut client_lines = BufReader::new(client_in);
    let config = SessionConfig {
        storage_state_root: Some(root.clone()),
        mcp_file_root: Some(root.clone()),
        ..Default::default()
    };
    let server = tokio::spawn(async move {
        serve(server_in, server_out, config).await.expect("serve");
    });
    let _ = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0.1"}}}),
    )
    .await;
    client_out
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
        .await
        .unwrap();
    let opened = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"browser_open","arguments":{"url":url}}}),
    )
    .await;
    assert!(tool_text(&opened).contains("Storage State"), "{opened}");
    let imported = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"browser_storage_state","arguments":{"action":"import","name":"state"}}}),
    )
    .await;
    assert!(tool_text(&imported).contains("Storage State"), "{imported}");
    let evaluated = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"browser_evaluate","arguments":{"expression":"[document.cookie, localStorage.getItem('token')]"}}}),
    )
    .await;
    assert!(
        tool_text(&evaluated).contains("cookie-value"),
        "cookie was not imported: {evaluated}"
    );
    assert!(
        tool_text(&evaluated).contains("storage-value"),
        "localStorage was not imported before page scripts: {evaluated}"
    );
    let exported = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"browser_storage_state","arguments":{"action":"export","name":"roundtrip"}}}),
    )
    .await;
    assert!(tool_text(&exported).contains("exported"), "{exported}");
    let exported_state: Value =
        serde_json::from_slice(&std::fs::read(root.join("roundtrip.json")).unwrap()).unwrap();
    assert!(exported_state["cookies"]
        .as_array()
        .is_some_and(|cookies| cookies.iter().any(|cookie| cookie["name"] == "auth")));
    assert!(exported_state["origins"]
        .as_array()
        .is_some_and(|origins| origins.iter().any(|origin| origin["localStorage"]
            .as_array()
            .is_some_and(|items| items
                .iter()
                .any(|item| item["name"] == "token" && item["value"] == "storage-value")))));

    let upload = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"browser_upload","arguments":{"action_id":1,"files":["upload.txt"]}}}),
    ).await;
    assert_tool_ok(&upload);
    let uploaded_name = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"browser_evaluate","arguments":{"expression":"document.querySelector('#upload').files[0]?.name"}}}),
    ).await;
    assert!(
        tool_text(&uploaded_name).contains("upload.txt"),
        "file input did not receive the selected file: {uploaded_name}"
    );

    let start_download = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"browser_evaluate","arguments":{"expression":"document.querySelector('#download').click()"}}}),
    ).await;
    assert_tool_ok(&start_download);
    let mut download_list = Value::Null;
    for _ in 0..40 {
        let response = roundtrip(
            &mut client_out,
            &mut client_lines,
            serde_json::json!({"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"browser_downloads","arguments":{"action":"list"}}}),
        ).await;
        download_list = serde_json::from_str(&tool_text(&response)).unwrap_or(Value::Null);
        if download_list["downloads"]
            .as_array()
            .is_some_and(|items| !items.is_empty())
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        download_list["downloads"]
            .as_array()
            .is_some_and(|items| !items.is_empty()),
        "download event was not recorded: {download_list}"
    );
    let guid = download_list["downloads"][0]["guid"]
        .as_str()
        .expect("download guid");
    let completed = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"browser_downloads","arguments":{"action":"wait","guid":guid,"timeout_ms":5000}}}),
    ).await;
    let completed: Value =
        serde_json::from_str(&tool_text(&completed)).expect("download status JSON");
    assert_eq!(
        completed["state"], "completed",
        "download did not complete: {completed}"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("downloads/download.txt")).unwrap(),
        "download contents"
    );

    drop(client_out);
    drop(client_lines);
    server.await.expect("MCP server");
    http_task.abort();
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn test_mcp_live_policy_switch_preserves_browser_session() {
    if flashbrowse_cdp::find_chrome(None).is_err() {
        eprintln!("SKIP: no Chromium binary found");
        return;
    }

    let (server_in, mut client_out) = duplex(64 * 1024);
    let (server_out, client_in) = duplex(64 * 1024);
    let mut client_lines = BufReader::new(client_in);
    let server = tokio::spawn(async move {
        serve(server_in, server_out, SessionConfig::default())
            .await
            .expect("serve")
    });
    let init = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}),
    ).await;
    assert!(init.get("error").is_none(), "initialize failed: {init}");

    let open1 = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"browser_open","arguments":{"url":"data:text/html,<h1>First</h1>","resource_policy":"agent-default"}}}),
    ).await;
    assert_tool_ok(&open1);

    let tabs1 = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"browser_tabs","arguments":{"action":"list"}}}),
    ).await;
    assert_tool_ok(&tabs1);
    let tabs1_json: Value = serde_json::from_str(&tool_text(&tabs1)).expect("tabs1 json");
    let first_tid = tabs1_json
        .as_array()
        .and_then(|arr| arr.iter().find(|t| t["active"] == true))
        .and_then(|t| t["target_id"].as_str())
        .expect("active target_id")
        .to_string();

    let open2 = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"browser_open","arguments":{"url":"data:text/html,<h1>Second</h1>","resource_policy":"full-render"}}}),
    ).await;
    assert_tool_ok(&open2);
    assert!(tool_text(&open2).contains("Second"));

    let tabs2 = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"browser_tabs","arguments":{"action":"list"}}}),
    ).await;
    assert_tool_ok(&tabs2);
    let tabs2_json: Value = serde_json::from_str(&tool_text(&tabs2)).expect("tabs2 json");
    let second_tid = tabs2_json
        .as_array()
        .and_then(|arr| arr.iter().find(|t| t["active"] == true))
        .and_then(|t| t["target_id"].as_str())
        .expect("active target_id")
        .to_string();

    assert_eq!(
        first_tid, second_tid,
        "switching resource_policy must update live session without restarting Chrome"
    );

    drop(client_out);
    drop(client_lines);
    server.await.expect("MCP server");
}

#[tokio::test]
async fn test_mcp_navigate_and_wait_for() {
    if flashbrowse_cdp::find_chrome(None).is_err() {
        eprintln!("SKIP: no Chromium binary found");
        return;
    }

    let port = spawn_raw_http(|path, _| {
        let body = if path.split('?').next().unwrap_or("/") == "/two" {
            r#"<!doctype html><html><head><title>Page Two</title></head><body><h1>Page Two</h1><div id="host"></div><script>
setTimeout(() => {
  const host = document.getElementById('host');
  const root = host.attachShadow({mode: 'open'});
  root.innerHTML = '<p class="delayed-ready">Shadow Ready Now</p>';
  history.pushState({}, '', '/two?ready=1');
}, 200);
</script></body></html>"#
        } else {
            r#"<!doctype html><html><head><title>Page One</title></head><body><h1>Page One</h1><a href="/two">Go Two</a></body></html>"#
        };
        ("200 OK", "text/html; charset=utf-8", body.to_string())
    });

    let (server_in, mut client_out) = duplex(64 * 1024);
    let (server_out, client_in) = duplex(64 * 1024);
    let mut client_lines = BufReader::new(client_in);
    let server = tokio::spawn(async move {
        serve(server_in, server_out, SessionConfig::default())
            .await
            .expect("serve")
    });

    let init = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}),
    ).await;
    assert!(init.get("error").is_none());

    let p1 = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"browser_open","arguments":{"url":format!("http://127.0.0.1:{port}/")}}}),
    ).await;
    assert_tool_ok(&p1);
    assert!(tool_text(&p1).contains("Page One"));

    let p2 = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"browser_open","arguments":{"url":format!("http://127.0.0.1:{port}/two")}}}),
    ).await;
    assert_tool_ok(&p2);
    assert!(tool_text(&p2).contains("Page Two"));

    let w_text = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"browser_wait_for","arguments":{"text":"Shadow Ready Now","timeout_ms":4000}}}),
    ).await;
    assert_tool_ok(&w_text);
    assert!(tool_text(&w_text).contains("Shadow Ready Now"));

    let w_sel = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"browser_wait_for","arguments":{"selector":".delayed-ready","timeout_ms":2000}}}),
    ).await;
    assert_tool_ok(&w_sel);

    let w_url = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"browser_wait_for","arguments":{"url":"ready=1","timeout_ms":2000}}}),
    ).await;
    assert_tool_ok(&w_url);

    // Go back from /two?ready=1 -> /two -> /
    let back1 = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"browser_navigate","arguments":{"action":"back"}}}),
    ).await;
    assert_tool_ok(&back1);
    let back2 = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"browser_navigate","arguments":{"action":"back"}}}),
    ).await;
    assert_tool_ok(&back2);
    assert!(
        tool_text(&back2).contains("Page One"),
        "back did not return to Page One: {back2}"
    );

    let fwd = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"browser_navigate","arguments":{"action":"forward"}}}),
    ).await;
    assert_tool_ok(&fwd);
    assert!(
        tool_text(&fwd).contains("Page Two"),
        "forward did not return to Page Two: {fwd}"
    );

    let rel = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"browser_navigate","arguments":{"action":"reload","ignore_cache":true}}}),
    ).await;
    assert_tool_ok(&rel);
    assert!(
        tool_text(&rel).contains("Page Two"),
        "reload did not return Page Two: {rel}"
    );

    drop(client_out);
    drop(client_lines);
    server.await.expect("MCP server");
}

#[tokio::test]
async fn test_mcp_rich_inputs_dblclick_rightclick_drag_chords_and_container_scroll() {
    if flashbrowse_cdp::find_chrome(None).is_err() {
        eprintln!("SKIP: no Chromium binary found");
        return;
    }

    let (server_in, mut client_out) = duplex(64 * 1024);
    let (server_out, client_in) = duplex(64 * 1024);
    let mut client_lines = BufReader::new(client_in);
    let server = tokio::spawn(async move {
        serve(server_in, server_out, SessionConfig::default())
            .await
            .expect("serve")
    });

    let init = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}),
    ).await;
    assert!(init.get("error").is_none());

    let html = r#"<!doctype html><html><body style="margin:0;padding:16px">
<button id="dbl" ondblclick="this.textContent='DBL_OK'">DoubleBtn</button>
<button id="rc" oncontextmenu="event.preventDefault();this.textContent='CTX_OK'">RightBtn</button>
<button id="src" style="display:inline-block;width:80px;height:40px">DragSrc</button>
<button id="dst" style="display:inline-block;width:80px;height:40px;margin-left:120px">DragDst</button>
<div id="scroller" style="height:80px;overflow-y:auto;border:1px solid black;margin-top:12px">
  <button id="inside">InnerBtn</button>
  <div style="height:600px">Tall inner content</div>
</div>
<p id="status">init</p>
<script>
let dragging = false;
document.getElementById('src').addEventListener('mousedown', () => { dragging = true; });
document.getElementById('dst').addEventListener('mouseup', () => {
  if (dragging) { document.getElementById('status').textContent = 'DRAG_OK'; dragging = false; }
});
window.addEventListener('keydown', (e) => {
  if (e.ctrlKey && e.key.toLowerCase() === 'k') {
    e.preventDefault();
    document.getElementById('status').textContent = 'CHORD_OK';
  }
});
</script></body></html>"#;
    let url = format!("data:text/html,{}", html.replace('\n', ""));
    let open = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"browser_open","arguments":{"url":url}}}),
    ).await;
    assert_tool_ok(&open);

    let dbl = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"browser_act","arguments":{"action_id":1,"action":"double_click"}}}),
    ).await;
    assert_tool_ok(&dbl);
    assert!(
        tool_text(&dbl).contains("DBL_OK"),
        "double_click failed: {dbl}"
    );

    let rc = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"browser_act","arguments":{"action_id":2,"action":"right_click"}}}),
    ).await;
    assert_tool_ok(&rc);
    assert!(
        tool_text(&rc).contains("CTX_OK"),
        "right_click failed: {rc}"
    );

    let drag = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"browser_act","arguments":{"action_id":3,"target_action_id":4,"action":"drag_and_drop"}}}),
    ).await;
    assert_tool_ok(&drag);
    assert!(
        tool_text(&drag).contains("DRAG_OK"),
        "drag_and_drop failed: {drag}"
    );

    let chord = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"browser_act","arguments":{"action":"press_key","value":"Control+k"}}}),
    ).await;
    assert_tool_ok(&chord);
    assert!(
        tool_text(&chord).contains("CHORD_OK"),
        "Control+k chord failed: {chord}"
    );

    let scroll = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"browser_act","arguments":{"action_id":5,"action":"scroll","value":"down","amount":200}}}),
    ).await;
    assert_tool_ok(&scroll);
    let st = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"browser_evaluate","arguments":{"expression":"document.getElementById('scroller').scrollTop > 0"}}}),
    ).await;
    assert!(
        tool_text(&st).contains("true"),
        "inner container did not scroll: {st}"
    );

    drop(client_out);
    drop(client_lines);
    server.await.expect("MCP server");
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

#[tokio::test]
async fn test_mcp_console_network_headers_and_fetch_route_mocking() {
    if flashbrowse_cdp::find_chrome(None).is_err() {
        eprintln!("SKIP: no Chromium binary found");
        return;
    }

    let port = spawn_raw_http(|path, req| {
        if path == "/api/echo" {
            let has_auth = req
                .to_ascii_lowercase()
                .contains("x-test-token: secret-123");
            (
                "200 OK",
                "application/json",
                format!("{{\"source\":\"real\",\"auth\":{has_auth}}}"),
            )
        } else if path == "/api/fail" {
            (
                "404 Not Found",
                "application/json",
                "{\"error\":\"not_found\"}".to_string(),
            )
        } else {
            (
                "200 OK",
                "text/html; charset=utf-8",
                r#"<!doctype html><html><body><h1>Obs Test</h1><pre id="out"></pre><script>
console.error('boom-error');
fetch('/api/fail').catch(() => {});
setTimeout(() => { throw new Error('uncaught-boom'); }, 10);
</script></body></html>"#
                    .to_string(),
            )
        }
    });

    let (server_in, mut client_out) = duplex(64 * 1024);
    let (server_out, client_in) = duplex(64 * 1024);
    let mut client_lines = BufReader::new(client_in);
    let server = tokio::spawn(async move {
        serve(server_in, server_out, SessionConfig::default())
            .await
            .expect("serve")
    });

    let init = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}),
    ).await;
    assert!(init.get("error").is_none());

    let open = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"browser_open","arguments":{"url":format!("http://127.0.0.1:{port}/")}}}),
    ).await;
    assert_tool_ok(&open);

    let cons = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"browser_console","arguments":{"action":"list"}}}),
    ).await;
    assert_tool_ok(&cons);
    let cons_txt = tool_text(&cons);
    assert!(
        cons_txt.contains("boom-error") && cons_txt.contains("uncaught-boom"),
        "console missing expected entries: {cons_txt}"
    );

    let net_fail = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"browser_network","arguments":{"action":"list","failed_only":true}}}),
    ).await;
    assert_tool_ok(&net_fail);
    let net_entries: Value = serde_json::from_str(&tool_text(&net_fail)).expect("network JSON");
    let fail_req_id = net_entries
        .as_array()
        .and_then(|arr| {
            arr.iter()
                .find(|e| e["url"].as_str().is_some_and(|u| u.contains("/api/fail")))
        })
        .and_then(|e| e["request_id"].as_str())
        .expect("/api/fail entry")
        .to_string();

    let body = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"browser_network","arguments":{"action":"body","request_id":fail_req_id}}}),
    ).await;
    assert_tool_ok(&body);
    assert!(
        tool_text(&body).contains("not_found"),
        "body mismatch: {body}"
    );

    let set_hdr = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"browser_network","arguments":{"action":"set_headers","headers":{"X-Test-Token":"secret-123"}}}}),
    ).await;
    assert_tool_ok(&set_hdr);

    let sync_get = "(() => { const x = new XMLHttpRequest(); x.open('GET', '/api/echo', false); x.send(); return x.responseText; })()";
    let real_fetch = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"browser_evaluate","arguments":{"expression":sync_get}}}),
    ).await;
    assert_tool_ok(&real_fetch);
    let real_str: String = serde_json::from_str(&tool_text(&real_fetch)).expect("json string");
    assert!(
        real_str.contains("\"auth\":true"),
        "extra header did not reach server: {real_str}"
    );

    let add_route = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"browser_route","arguments":{"action":"add","url_pattern":"*/api/echo","status":200,"content_type":"application/json","body":"{\"source\":\"mocked\"}"}}}),
    ).await;
    assert_tool_ok(&add_route);

    let mock_fetch = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"browser_evaluate","arguments":{"expression":sync_get}}}),
    ).await;
    assert_tool_ok(&mock_fetch);
    assert!(
        tool_text(&mock_fetch).contains("mocked"),
        "route mock did not intercept /api/echo: {mock_fetch}"
    );

    let clr_route = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"browser_route","arguments":{"action":"clear"}}}),
    ).await;
    assert_tool_ok(&clr_route);

    let after_clear = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"browser_evaluate","arguments":{"expression":sync_get}}}),
    ).await;
    assert_tool_ok(&after_clear);
    let after_str: String = serde_json::from_str(&tool_text(&after_clear)).expect("json string");
    assert!(
        after_str.contains("\"source\":\"real\""),
        "real server did not respond after route clear: {after_str}"
    );

    drop(client_out);
    drop(client_lines);
    server.await.expect("MCP server");
}

#[tokio::test]
async fn test_mcp_isolated_multi_context_cookies_and_storage() {
    if flashbrowse_cdp::find_chrome(None).is_err() {
        eprintln!("SKIP: no Chromium binary found");
        return;
    }

    let port = spawn_raw_http(|_, _| {
        (
            "200 OK",
            "text/html; charset=utf-8",
            "<!doctype html><html><body><h1>Context Page</h1></body></html>".to_string(),
        )
    });

    let (server_in, mut client_out) = duplex(64 * 1024);
    let (server_out, client_in) = duplex(64 * 1024);
    let mut client_lines = BufReader::new(client_in);
    let server = tokio::spawn(async move {
        serve(server_in, server_out, SessionConfig::default())
            .await
            .expect("serve")
    });

    let init = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}),
    ).await;
    assert!(init.get("error").is_none());

    let url = format!("http://127.0.0.1:{port}/");

    // Open in default context and set cookie + localStorage
    let open_def = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"browser_open","arguments":{"url":url}}}),
    ).await;
    assert_tool_ok(&open_def);
    let set_def = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"browser_evaluate","arguments":{"expression":"document.cookie='who=alice'; localStorage.setItem('k','alice'); 'ok'"}}}),
    ).await;
    assert_tool_ok(&set_def);

    // Create isolated context 'bob'
    let create_bob = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"browser_contexts","arguments":{"action":"create","name":"bob"}}}),
    ).await;
    assert_tool_ok(&create_bob);

    let open_bob = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"browser_open","arguments":{"url":url}}}),
    ).await;
    assert_tool_ok(&open_bob);

    let read_bob = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"browser_evaluate","arguments":{"expression":"document.cookie + '|' + (localStorage.getItem('k') || 'none')"}}}),
    ).await;
    assert_tool_ok(&read_bob);
    assert!(
        tool_text(&read_bob).contains("|none"),
        "isolated context leaked default cookie/storage: {read_bob}"
    );

    let set_bob = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"browser_evaluate","arguments":{"expression":"document.cookie='who=bob'; localStorage.setItem('k','bob'); 'ok'"}}}),
    ).await;
    assert_tool_ok(&set_bob);

    // Switch back to default and confirm Alice's state is untouched
    let sel_def = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"browser_contexts","arguments":{"action":"select","name":"default"}}}),
    ).await;
    assert_tool_ok(&sel_def);
    let read_def = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"browser_evaluate","arguments":{"expression":"document.cookie + '|' + localStorage.getItem('k')"}}}),
    ).await;
    assert!(
        tool_text(&read_def).contains("who=alice|alice"),
        "default context was polluted by bob: {read_def}"
    );

    // Close bob and confirm closing default is rejected
    let close_bob = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"browser_contexts","arguments":{"action":"close","name":"bob"}}}),
    ).await;
    assert_tool_ok(&close_bob);

    let close_def = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"browser_contexts","arguments":{"action":"close","name":"default"}}}),
    ).await;
    assert_eq!(close_def["result"]["isError"], true);

    drop(client_out);
    drop(client_lines);
    server.await.expect("MCP server");
}

#[tokio::test]
async fn test_mcp_emulate_viewport_tz_geo_permissions_element_clip_and_pdf() {
    if flashbrowse_cdp::find_chrome(None).is_err() {
        eprintln!("SKIP: no Chromium binary found");
        return;
    }

    let root = std::env::temp_dir().join(format!("flashbrowse-pdf-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let cfg = SessionConfig {
        mcp_file_root: Some(root.clone()),
        ..Default::default()
    };

    let (server_in, mut client_out) = duplex(256 * 1024);
    let (server_out, client_in) = duplex(256 * 1024);
    let mut client_lines = BufReader::new(client_in);
    let server = tokio::spawn(async move {
        serve(server_in, server_out, cfg).await.expect("serve");
    });

    let init = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}),
    ).await;
    assert!(init.get("error").is_none());

    let open = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"browser_open","arguments":{"url":"data:text/html,<html><head><meta name='viewport' content='width=device-width,initial-scale=1'></head><body style='margin:0'><button style='width:120px;height:40px'>TargetBtn</button></body></html>"}}}),
    ).await;
    assert_tool_ok(&open);

    let emu = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({
            "jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{
                "name":"browser_emulate",
                "arguments":{
                    "viewport":{"width":390,"height":844,"device_scale_factor":2.0,"mobile":true},
                    "timezone":"Asia/Tokyo",
                    "locale":"ja-JP",
                    "geolocation":{"latitude":35.6762,"longitude":139.6503,"accuracy":5.0},
                    "permissions":{"grants":["geolocation"]}
                }
            }
        }),
    )
    .await;
    assert_tool_ok(&emu);

    let check = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"browser_evaluate","arguments":{"expression":"window.innerWidth + '|' + Intl.DateTimeFormat().resolvedOptions().timeZone"}}}),
    ).await;
    assert_tool_ok(&check);
    assert!(
        tool_text(&check).contains("390|Asia/Tokyo"),
        "emulation did not apply: {check}"
    );

    // Element-clipped screenshot of action_id 1
    let clip_shot = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"browser_screenshot","arguments":{"action_id":1}}}),
    ).await;
    assert_tool_ok(&clip_shot);
    assert_eq!(clip_shot["result"]["content"][0]["type"], "image");
    let b64 = clip_shot["result"]["content"][0]["data"]
        .as_str()
        .expect("png base64");
    assert!(!b64.is_empty());

    // PDF export confined under mcp_file_root/pdfs/
    let pdf = roundtrip(
        &mut client_out,
        &mut client_lines,
        serde_json::json!({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"browser_pdf","arguments":{"name":"sample-report","landscape":true}}}),
    ).await;
    assert_tool_ok(&pdf);
    let pdf_meta: Value = serde_json::from_str(&tool_text(&pdf)).expect("pdf JSON");
    let pdf_path = std::path::PathBuf::from(pdf_meta["path"].as_str().expect("pdf path"));
    let pdf_bytes = std::fs::read(&pdf_path).expect("read exported pdf");
    assert!(
        pdf_bytes.starts_with(b"%PDF-"),
        "exported file is not a PDF"
    );

    drop(client_out);
    drop(client_lines);
    server.await.expect("MCP server");
    let _ = std::fs::remove_dir_all(root);
}
