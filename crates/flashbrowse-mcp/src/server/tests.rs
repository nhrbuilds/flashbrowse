use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

#[tokio::test]
async fn reader_destructor_panic_is_contained_by_library_entrypoint() {
    struct PanicDropReader;
    impl tokio::io::AsyncRead for PanicDropReader {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            _: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }
    impl Drop for PanicDropReader {
        fn drop(&mut self) {
            panic!("injected reader destructor panic");
        }
    }

    let result = serve(PanicDropReader, tokio::io::sink(), SessionConfig::default()).await;
    assert!(
        result.is_ok(),
        "reader destructor panic escaped: {result:?}"
    );
}

#[tokio::test]
async fn multiple_embedded_destructor_panics_are_contained_by_library_entrypoint() {
    struct PanicDropReader;
    impl tokio::io::AsyncRead for PanicDropReader {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            _: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }
    impl Drop for PanicDropReader {
        fn drop(&mut self) {
            panic!("injected reader destructor panic");
        }
    }

    struct PanicDropWriter;
    impl tokio::io::AsyncWrite for PanicDropWriter {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            buffer: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            std::task::Poll::Ready(Ok(buffer.len()))
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }
    impl Drop for PanicDropWriter {
        fn drop(&mut self) {
            panic!("injected writer destructor panic");
        }
    }

    let result = serve(PanicDropReader, PanicDropWriter, SessionConfig::default()).await;
    assert!(
        result.is_ok(),
        "embedded destructor panic escaped: {result:?}"
    );
}

#[tokio::test]
async fn dispatch_panic_returns_error_and_preserves_connection() {
    let input = &b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"test/panic\"}\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"}\n"[..];
    let mut output = Vec::new();
    serve(input, &mut output, SessionConfig::default())
        .await
        .unwrap();
    let responses: Vec<Value> = String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(responses[0]["id"], 1);
    assert_eq!(responses[0]["error"]["code"], -32603);
    assert_eq!(responses[1]["id"], 2);
    assert!(responses[1].get("result").is_some());
}

#[tokio::test]
async fn frame_handler_panic_returns_error_and_keeps_connection() {
    let input = &b"__panic_frame__\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"}\n"[..];
    let mut output = Vec::new();
    serve(input, &mut output, SessionConfig::default())
        .await
        .unwrap();
    let responses: Vec<Value> = String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(responses.len(), 2);
    assert_eq!(responses[0]["id"], Value::Null);
    assert_eq!(responses[0]["error"]["code"], -32603);
    assert_eq!(responses[1]["id"], 2);
    assert_eq!(responses[1]["result"], Value::Null);
}

#[test]
fn context_failures_preserve_other_sessions_when_the_browser_is_connected() {
    for action in ["create", "close"] {
        assert!(matches!(
            RecoveryScope::for_tool("browser_contexts", &serde_json::json!({ "action": action })),
            RecoveryScope::RefreshActiveTree
        ));
    }
}

#[tokio::test]
async fn writer_panic_stops_stream_without_second_response() {
    struct PanicWriter;
    impl tokio::io::AsyncWrite for PanicWriter {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            _: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            panic!("injected writer panic");
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }
    impl Drop for PanicWriter {
        fn drop(&mut self) {
            panic!("injected writer destructor panic after write panic");
        }
    }
    let input = &b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n"[..];
    let error = serve(input, PanicWriter, SessionConfig::default())
        .await
        .unwrap_err();
    assert!(matches!(error, McpError::Io(error) if error.kind() == std::io::ErrorKind::Other));
}

#[tokio::test]
async fn writer_poll_panic_after_a_partial_write_does_not_append_a_frame() {
    use std::sync::{Arc as StdArc, Mutex as StdMutex};

    struct PartialThenPanicWriter {
        output: StdArc<StdMutex<Vec<u8>>>,
    }
    impl tokio::io::AsyncWrite for PartialThenPanicWriter {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            bytes: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            let partial = bytes.len().min(8);
            if let Ok(mut output) = self.output.lock() {
                output.extend_from_slice(&bytes[..partial]);
            }
            panic!("injected writer poll panic after a partial write");
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    let input = &b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"}\n"[..];
    let output = StdArc::new(StdMutex::new(Vec::new()));
    let error = serve(
        input,
        PartialThenPanicWriter {
            output: StdArc::clone(&output),
        },
        SessionConfig::default(),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, McpError::Io(error) if error.kind() == std::io::ErrorKind::Other));

    let output = output.lock().unwrap();
    assert_eq!(output.as_slice(), b"{\"jsonrp");
}

#[tokio::test]
async fn reader_poll_panic_returns_a_transport_error_frame() {
    struct PanicReader;
    impl tokio::io::AsyncRead for PanicReader {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            _: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            panic!("injected reader poll panic");
        }
    }

    let mut output = Vec::new();
    let error = serve(PanicReader, &mut output, SessionConfig::default())
        .await
        .unwrap_err();
    assert!(matches!(error, McpError::Io(_)));
    let response: Value = serde_json::from_slice(&output[..output.len() - 1]).unwrap();
    assert_eq!(response["id"], Value::Null);
    assert_eq!(response["error"]["code"], -32603);
    assert_eq!(response["error"]["message"], "MCP input or output failed");
}

#[test]
fn serves_tool_calls_without_caller_timer() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
            let input = &b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"unknown\"}}\n{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"ping\"}\n"[..];
            let mut output = Vec::new();
            serve(input, &mut output, SessionConfig::default()).await.unwrap();
            let response = String::from_utf8(output).unwrap();
            assert!(response.contains("\"id\":3,\"result\":null"), "{response}");
        });
}

#[tokio::test]
async fn disconnected_output_returns_an_io_error() {
    let (output, peer) = tokio::io::duplex(64);
    drop(peer);
    let input = &b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n"[..];
    let err = serve(input, output, SessionConfig::default())
        .await
        .unwrap_err();
    assert!(matches!(err, McpError::Io(err) if err.kind() == std::io::ErrorKind::BrokenPipe));
}

#[tokio::test]
async fn bounded_line_reader_drains_oversize_frame_and_resumes() {
    let (mut writer, reader) = tokio::io::duplex(64);
    writer.write_all(b"0123456789abcdef\nnext\n").await.unwrap();
    drop(writer);

    let mut reader = BufReader::with_capacity(3, reader);
    assert!(matches!(
        read_bounded_line(&mut reader, 8).await.unwrap(),
        Some(BoundedLine::TooLong)
    ));
    match read_bounded_line(&mut reader, 8).await.unwrap() {
        Some(BoundedLine::Line(bytes)) => assert_eq!(bytes, b"next"),
        other => panic!("expected the next frame, got {other:?}"),
    }
    assert!(read_bounded_line(&mut reader, 8).await.unwrap().is_none());
}

#[tokio::test]
async fn isolated_tool_call_catches_panic_and_resets_session_manager() {
    struct PanicOnDrop;
    impl Drop for PanicOnDrop {
        fn drop(&mut self) {
            panic!("simulated panic payload destructor");
        }
    }

    let mut handle: tools::SessionHandle = Arc::new(Mutex::new(tools::SessionManager::new()));
    let err = run_isolated_tool_call(
        Duration::from_secs(5),
        "panic_test",
        RecoveryScope::ResetSessions,
        &mut handle,
        async move {
            std::panic::panic_any(PanicOnDrop);
        },
    )
    .await
    .expect_err("panic must map to McpError::Internal");

    match err {
        McpError::Internal(msg) => {
            assert!(msg.contains("session reset"), "unexpected message: {msg}");
        }
        other => panic!("expected Internal error, got {other:?}"),
    }
    let guard = handle.lock().await;
    assert_eq!(guard.active_name(), "default");
    assert!(guard.is_none());
    drop(guard);

    let recovered = run_isolated_tool_call(
        Duration::from_secs(5),
        "after_panic",
        RecoveryScope::ReadOnly,
        &mut handle,
        async { Ok(ToolResult::text("server recovered".to_string())) },
    )
    .await
    .expect("a later tool call should still run");
    assert_eq!(
        recovered
            .content
            .first()
            .and_then(|item| item.get("text"))
            .and_then(Value::as_str),
        Some("server recovered")
    );
}

#[tokio::test]
async fn isolated_tool_call_aborts_task_on_timeout() {
    let mut handle: tools::SessionHandle = Arc::new(Mutex::new(tools::SessionManager::new()));
    let old_handle = Arc::clone(&handle);
    let old_guard = old_handle.lock().await;
    let dropped = Arc::new(AtomicBool::new(false));
    let dropped_flag = Arc::clone(&dropped);

    struct DropFlag(Arc<AtomicBool>);
    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    let err = run_isolated_tool_call(
        Duration::from_millis(25),
        "timeout_test",
        RecoveryScope::ReadOnly,
        &mut handle,
        async move {
            let _guard = DropFlag(dropped_flag);
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(ToolResult::text("unreachable".to_string()))
        },
    )
    .await
    .expect_err("timeout must return McpError::Internal");

    assert!(!Arc::ptr_eq(&old_handle, &handle));
    drop(old_guard);

    match err {
        McpError::Internal(msg) => assert!(msg.contains("timed out"), "{msg}"),
        other => panic!("expected timeout Internal error, got {other:?}"),
    }
    tokio::time::timeout(Duration::from_secs(1), async {
        while !dropped.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("timed-out async task must be cancelled");
    let guard = handle.lock().await;
    assert_eq!(guard.active_name(), "default");
    assert!(guard.is_none(), "timeout cleanup leaves no stale session");
}

#[tokio::test]
async fn dropping_request_supervisor_cancels_the_tool() {
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (finished_tx, finished_rx) = tokio::sync::oneshot::channel::<()>();
    let supervisor = tokio::spawn(async move {
        let mut handle = Arc::new(Mutex::new(tools::SessionManager::new()));
        run_isolated_tool_call(
            Duration::from_secs(60),
            "wait",
            RecoveryScope::ReadOnly,
            &mut handle,
            async move {
                let _finished = finished_tx;
                let _ = started_tx.send(());
                std::future::pending::<Result<ToolResult, McpError>>().await
            },
        )
        .await
    });
    started_rx.await.unwrap();
    supervisor.abort();
    assert!(supervisor.await.unwrap_err().is_cancelled());
    assert!(tokio::time::timeout(Duration::from_secs(1), finished_rx)
        .await
        .expect("tool must stop when its supervisor is dropped")
        .is_err());
}

#[tokio::test]
async fn interrupted_page_calls_preserve_isolated_storage() {
    use flashbrowse_cdp::protocol::target;
    use serde_json::json;

    if flashbrowse_cdp::find_chrome(None).is_err() {
        return;
    }
    let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("SKIP: local HTTP listener unavailable: {error}");
            return;
        }
    };
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let http = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut reader = BufReader::new(stream);
                let mut header = String::new();
                while reader.read_line(&mut header).await.is_ok_and(|n| n > 0) {
                    if header == "\r\n" {
                        let _ = reader.get_mut().write_all(
                                b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n<h1>Context test</h1><button>Click</button>"
                            ).await;
                        let _ = reader.get_mut().shutdown().await;
                        break;
                    }
                    header.clear();
                }
            });
        }
    });
    let config = SessionConfig::default();
    let mut handle: tools::SessionHandle = Arc::new(Mutex::new(tools::SessionManager::new()));
    let original = Arc::clone(&handle);
    let call = async |name: &str, args: Value| {
        let result = tools::call_tool(name, &args, &original, &config)
            .await
            .unwrap();
        assert_ne!(result.is_error, Some(true), "{:?}", result.content);
        result.content.first().unwrap()["text"]
            .as_str()
            .unwrap()
            .to_string()
    };
    call("browser_open", json!({"url": url})).await;
    call("browser_evaluate", json!({"expression":"document.cookie='user=default'; localStorage.setItem('ctx','default'); 'ok'"})).await;
    call(
        "browser_contexts",
        json!({"action":"create", "name":"isolated"}),
    )
    .await;
    call("browser_open", json!({"url": url})).await;
    call("browser_evaluate", json!({"expression":"document.cookie='user=isolated'; localStorage.setItem('ctx','isolated'); 'ok'"})).await;
    assert!(!original
        .lock()
        .await
        .as_ref()
        .unwrap()
        .action_map()
        .is_empty());

    let err = run_isolated_tool_call(
        Duration::from_secs(5),
        "browser_act",
        RecoveryScope::for_tool("browser_act", &json!({ "action": "click" })),
        &mut handle,
        async {
            // Reproduce a nested runtime boundary returning an error that
            // the calling layer might otherwise treat as ordinary.
            let _ = flashbrowse_core::async_runtime::run(async {
                panic!("injected nested page tool panic");
            })
            .await;
            Ok(ToolResult::text("must not report success".into()))
        },
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("browser contexts preserved"));
    assert!(Arc::ptr_eq(&original, &handle));
    assert!(original
        .lock()
        .await
        .as_ref()
        .unwrap()
        .action_map()
        .is_empty());

    let original_for_timeout = Arc::clone(&original);
    let err = run_isolated_tool_call(
        Duration::from_millis(100),
        "browser_downloads",
        RecoveryScope::for_tool("browser_downloads", &json!({})),
        &mut handle,
        async move {
            let _guard = original_for_timeout.lock().await;
            std::future::pending::<Result<ToolResult, McpError>>().await
        },
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("browser contexts preserved"));
    assert!(Arc::ptr_eq(&original, &handle));

    for action in ["create", "close"] {
        let err = run_isolated_tool_call(
            Duration::from_secs(5),
            "browser_contexts",
            RecoveryScope::for_tool("browser_contexts", &json!({ "action": action })),
            &mut handle,
            async {
                panic!("injected context management panic");
                #[allow(unreachable_code)]
                Ok(ToolResult::text("must not report success".into()))
            },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("browser contexts preserved"));
        assert!(Arc::ptr_eq(&original, &handle));
    }

    let storage = || json!({"expression":"document.cookie + '|' + localStorage.getItem('ctx')"});
    assert_eq!(
        call("browser_evaluate", storage()).await,
        "\"user=isolated|isolated\""
    );
    call(
        "browser_contexts",
        json!({"action":"select", "name":"default"}),
    )
    .await;
    assert_eq!(
        call("browser_evaluate", storage()).await,
        "\"user=default|default\""
    );

    let contexts: Value =
        serde_json::from_str(&call("browser_contexts", json!({ "action": "list" })).await).unwrap();
    let isolated_context_id = contexts
        .as_array()
        .and_then(|contexts| contexts.iter().find(|item| item["name"] == "isolated"))
        .and_then(|item| item["browser_context_id"].as_str())
        .expect("isolated context id")
        .to_string();

    let mut manager = handle.lock().await;
    let default = manager.as_ref().unwrap();
    assert!(target::close_target(&default.client(), default.target_id())
        .await
        .unwrap());
    if let Ok(tabs) = default.list_tabs().await {
        assert!(
            tabs.iter()
                .all(|tab| tab.browser_context_id.as_deref() != Some(isolated_context_id.as_str())),
            "default tab listing included an isolated context: {tabs:?}"
        );
    }
    manager.shutdown_all().await;
    http.abort();
}
