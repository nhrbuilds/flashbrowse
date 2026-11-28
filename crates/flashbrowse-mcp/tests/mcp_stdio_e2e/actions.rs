use super::*;

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
    // A wheel event resolves once Chromium has queued it, not once the offset
    // has been applied (Playwright documents the same for `mouse.wheel`). On
    // macOS the compositor can land the offset a frame after the CDP response,
    // so poll instead of reading once and racing the scroll.
    let mut applied = false;
    for _ in 0..40 {
        let st = roundtrip(
            &mut client_out,
            &mut client_lines,
            serde_json::json!({"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"browser_evaluate","arguments":{"expression":"document.getElementById('scroller').scrollTop > 0"}}}),
        ).await;
        if tool_text(&st).contains("true") {
            applied = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(applied, "inner container did not scroll");

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
