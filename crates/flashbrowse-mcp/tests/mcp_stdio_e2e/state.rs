use super::*;

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
