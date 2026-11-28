//! End-to-end MCP JSON-RPC validation over duplex pipes.
//! Drives `serve()` over in-memory I/O without spawning the CLI binary.
//! Requires system Chromium - skipped when absent.

#[path = "mcp_stdio_e2e/actions.rs"]
mod actions;
#[path = "mcp_stdio_e2e/basic.rs"]
mod basic;
#[path = "mcp_stdio_e2e/network.rs"]
mod network;
#[path = "mcp_stdio_e2e/state.rs"]
mod state;

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
