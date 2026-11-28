use super::*;

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
