//! Stdio transport loop (newline-delimited JSON-RPC 2.0).
//!
//! Strict stdout discipline: stdout is pure JSON-RPC, one line per frame.
//! All `tracing` output goes to stderr (initialized by `serve_stdio`, not
//! here, so in-process tests can install their own subscriber).

use std::future::Future;
use std::sync::{Arc, Once};
use std::time::Duration;

use futures_util::FutureExt;
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

use flashbrowse_core::SessionConfig;

use crate::protocol::{
    negotiate, JsonRpcError, JsonRpcErrorResponse, JsonRpcRequest, JsonRpcResult, McpError,
    ToolResult,
};
use crate::tools;

/// Generic serve loop over any `AsyncRead` stdin / `AsyncWrite` stdout.
///
/// This shape lets integration tests drive the server over in-memory pipes
/// without spawning a binary (see `tests/mcp_stdio_e2e.rs`).
pub async fn serve<R, W>(reader: R, writer: W, config: SessionConfig) -> Result<(), McpError>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let reader = flashbrowse_core::async_runtime::PanicSafeIo::new(reader);
    let writer = flashbrowse_core::async_runtime::PanicSafeIo::new(writer);
    flashbrowse_core::async_runtime::run(serve_inner(reader, writer, config))
        .await
        .map_err(|error| McpError::Internal(error.to_string()))?
}

async fn serve_inner<R, W>(
    reader: flashbrowse_core::async_runtime::PanicSafeIo<R>,
    writer: flashbrowse_core::async_runtime::PanicSafeIo<W>,
    config: SessionConfig,
) -> Result<(), McpError>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut reader = BufReader::new(reader);
    let mut writer = writer;
    let mut handle: tools::SessionHandle = Arc::new(Mutex::new(tools::SessionManager::new()));
    let mut output_started = false;

    let outcome = match std::panic::AssertUnwindSafe(serve_loop(
        &mut reader,
        &mut writer,
        &mut handle,
        &config,
        &mut output_started,
    ))
    .catch_unwind()
    .await
    {
        Ok(outcome) => outcome,
        Err(payload) => {
            flashbrowse_core::async_runtime::discard_panic_payload(payload);
            Err(McpError::Internal(
                "MCP input or output failed unexpectedly".to_string(),
            ))
        }
    };
    if outcome.is_err() && !output_started {
        let response = JsonRpcErrorResponse {
            jsonrpc: "2.0".to_string(),
            id: Value::Null,
            error: JsonRpcError::new(-32603, "MCP input or output failed".to_string()),
        };
        let response_result = std::panic::AssertUnwindSafe(write_frame_tracked(
            &mut writer,
            &response,
            &mut output_started,
        ))
        .catch_unwind()
        .await;
        match response_result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                tracing::warn!(%error, "could not write MCP transport error response")
            }
            Err(payload) => {
                flashbrowse_core::async_runtime::discard_panic_payload(payload);
                tracing::warn!("panic while writing MCP transport error response");
            }
        }
    }
    flashbrowse_core::async_runtime::run(cleanup_sessions(&handle, "server shutdown"))
        .await
        .map_err(|error| McpError::Internal(error.to_string()))?;
    outcome
}

async fn serve_loop<R, W>(
    mut reader: &mut R,
    mut writer: &mut W,
    handle: &mut tools::SessionHandle,
    config: &SessionConfig,
    output_started: &mut bool,
) -> Result<(), McpError>
where
    R: AsyncBufRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    // Track whether `initialize` was exchanged (best-effort lifecycle).
    let mut initialized = false;

    // Enforce the 1 MiB cap while reading, before an untrusted line can grow
    // the input buffer without bound.
    const MAX_LINE: usize = 1024 * 1024;
    loop {
        *output_started = false;
        let Some(frame) = read_bounded_line(&mut reader, MAX_LINE).await? else {
            break;
        };
        let mut request_id = None;
        let mut request_is_notification = None;
        let handled = std::panic::AssertUnwindSafe(async {
            'frame: {
        #[cfg(test)]
        if matches!(&frame, BoundedLine::Line(bytes) if bytes.as_slice() == b"__panic_frame__") {
            panic!("injected panic while handling a complete MCP frame");
        }
        let line = match frame {
            BoundedLine::TooLong => {
                let resp = JsonRpcErrorResponse {
                    jsonrpc: "2.0".to_string(),
                    id: Value::Null,
                    error: JsonRpcError::new(-32700, "Parse error: line too large".to_string()),
                };
                write_frame_tracked(&mut writer, &resp, output_started).await?;
                break 'frame;
            }
            BoundedLine::Line(bytes) => match String::from_utf8(bytes) {
                Ok(line) => line,
                Err(_) => {
                    let resp = JsonRpcErrorResponse {
                        jsonrpc: "2.0".to_string(),
                        id: Value::Null,
                        error: JsonRpcError::new(-32700, "Parse error".to_string()),
                    };
                    write_frame_tracked(&mut writer, &resp, output_started).await?;
                    break 'frame;
                }
            },
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break 'frame;
        }
        let req: JsonRpcRequest = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(_) => {
                // Parse error → error response with id null (per JSON-RPC §5.1).
                let resp = JsonRpcErrorResponse {
                    jsonrpc: "2.0".to_string(),
                    id: Value::Null,
                    error: JsonRpcError::new(-32700, "Parse error".to_string()),
                };
                write_frame_tracked(&mut writer, &resp, output_started).await?;
                break 'frame;
            }
        };
        request_id = req.id.clone();
        request_is_notification = Some(req.is_notification());
        // Strict JSON-RPC 2.0 tag (M5): reject non-2.0 frames.
        if let Some(tag) = req.jsonrpc.as_deref() {
            if tag != "2.0" {
                if !req.is_notification() {
                    let resp = JsonRpcErrorResponse {
                        jsonrpc: "2.0".to_string(),
                        id: req.id.clone().unwrap_or(Value::Null),
                        error: JsonRpcError::new(
                            -32600,
                            "Invalid Request: jsonrpc must be \"2.0\"".to_string(),
                        ),
                    };
                    write_frame_tracked(&mut writer, &resp, output_started).await?;
                }
                break 'frame;
            }
        }
        // Lifecycle enforcement (M5): tools/* before initialize is an error.
        if (req.method == "tools/list" || req.method == "tools/call") && !initialized {
            if !req.is_notification() {
                let resp = JsonRpcErrorResponse {
                    jsonrpc: "2.0".to_string(),
                    id: req.id.clone().unwrap_or(Value::Null),
                    error: JsonRpcError::new(-32002, "Server not initialized".to_string()),
                };
                write_frame_tracked(&mut writer, &resp, output_started).await?;
            }
            break 'frame;
        }
        let dispatch = std::panic::AssertUnwindSafe(async {
            match req.method.as_str() {
                "initialize" => {
                    let client_version = req
                        .params
                        .as_ref()
                        .and_then(|p| p.get("protocolVersion"))
                        .and_then(|v| v.as_str());
                    let version = negotiate(client_version);
                    if !req.is_notification() {
                        let result = serde_json::json!({
                            "protocolVersion": version,
                            "capabilities": {"tools": {}},
                            "serverInfo": {"name": "flashbrowse", "version": env!("CARGO_PKG_VERSION")}
                        });
                        let resp = JsonRpcResult {
                            jsonrpc: "2.0".to_string(),
                            id: req.id.clone().unwrap_or(Value::Null),
                            result,
                        };
                        write_frame_tracked(&mut writer, &resp, output_started).await?;
                        initialized = true;
                    }
                }
                "notifications/initialized" => {
                    // Notification: acknowledge silently, never respond.
                    initialized = true;
                }
                "ping" => {
                    if !req.is_notification() {
                        let resp = JsonRpcResult {
                            jsonrpc: "2.0".to_string(),
                            id: req.id.clone().unwrap_or(Value::Null),
                            result: Value::Null,
                        };
                        write_frame_tracked(&mut writer, &resp, output_started).await?;
                    }
                }
                "tools/list" => {
                    if req.is_notification() {
                        return Ok(());
                    }
                    let result = serde_json::json!({"tools": tools::tool_definitions()});
                    let resp = JsonRpcResult {
                        jsonrpc: "2.0".to_string(),
                        id: req.id.clone().unwrap_or(Value::Null),
                        result,
                    };
                    write_frame_tracked(&mut writer, &resp, output_started).await?;
                }
                "tools/call" => {
                    if req.is_notification() {
                        return Ok(());
                    }
                    // NEVER let a malformed frame terminate the server: reply
                    // with a JSON-RPC error and keep serving.
                    let Some(params) = req.params.clone() else {
                        let resp = JsonRpcErrorResponse {
                            jsonrpc: "2.0".to_string(),
                            id: req.id.clone().unwrap_or(Value::Null),
                            error: JsonRpcError::new(-32602, "tools/call missing params".to_string()),
                        };
                        write_frame_tracked(&mut writer, &resp, output_started).await?;
                        return Ok(());
                    };
                    let Some(name) = params
                        .get("name")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                    else {
                        let resp = JsonRpcErrorResponse {
                            jsonrpc: "2.0".to_string(),
                            id: req.id.clone().unwrap_or(Value::Null),
                            error: JsonRpcError::new(-32602, "tools/call missing 'name'".to_string()),
                        };
                        write_frame_tracked(&mut writer, &resp, output_started).await?;
                        return Ok(());
                    };
                    let arguments = params
                        .get("arguments")
                        .cloned()
                        .unwrap_or(serde_json::json!({}));
                    if !arguments.is_object() {
                        let resp = JsonRpcErrorResponse {
                            jsonrpc: "2.0".to_string(),
                            id: req.id.clone().unwrap_or(Value::Null),
                            error: JsonRpcError::new(
                                -32602,
                                "tools/call arguments must be an object".to_string(),
                            ),
                        };
                        write_frame_tracked(&mut writer, &resp, output_started).await?;
                        return Ok(());
                    }
                    // Outer budget (M4/3.6): must exceed the worst inner
                    // latency (navigate 15s + load wait 15s + tree fetches),
                    // otherwise the browser navigates while the agent gets a
                    // tree for the previous page. Each call runs in its own
                    // spawned task so a panic unwinds at the task boundary
                    // instead of killing the MCP stdio server.
                    let call_name = name.clone();
                    let call_handle = Arc::clone(handle);
                    let call_config = config.clone();
                    let result = flashbrowse_core::async_runtime::run(run_isolated_tool_call(
                        Duration::from_secs(120),
                        &name,
                        RecoveryScope::for_tool(&name, &arguments),
                        handle,
                        async move {
                            tools::call_tool(&call_name, &arguments, &call_handle, &call_config).await
                        },
                    ))
                    .await
                    .map_err(|error| McpError::Internal(error.to_string()))?;
                    let id = req.id.clone().unwrap_or(Value::Null);
                    match result {
                        Ok(tr) => {
                            let resp = JsonRpcResult {
                                jsonrpc: "2.0".to_string(),
                                id,
                                result: serde_json::to_value(tr)?,
                            };
                            write_frame_tracked(&mut writer, &resp, output_started).await?;
                        }
                        Err(McpError::InvalidParams(msg)) => {
                            let resp = JsonRpcErrorResponse {
                                jsonrpc: "2.0".to_string(),
                                id,
                                error: JsonRpcError::new(-32602, msg),
                            };
                            write_frame_tracked(&mut writer, &resp, output_started).await?;
                        }
                        Err(e) => {
                            let resp = JsonRpcErrorResponse {
                                jsonrpc: "2.0".to_string(),
                                id,
                                error: JsonRpcError::new(-32603, e.to_string()),
                            };
                            write_frame_tracked(&mut writer, &resp, output_started).await?;
                        }
                    }
                }
                "notifications/cancelled" => {
                    // Best-effort: browser ops are short; ignore.
                }
                #[cfg(test)]
                "test/panic" => panic!("injected request dispatch panic"),
                other => {
                    if req.is_notification() {
                        // Unknown notification: ignore silently.
                        return Ok(());
                    }
                    let resp = JsonRpcErrorResponse {
                        jsonrpc: "2.0".to_string(),
                        id: req.id.clone().unwrap_or(Value::Null),
                        error: JsonRpcError::new(-32601, format!("Method not found: {other}")),
                    };
                    write_frame_tracked(&mut writer, &resp, output_started).await?;
                }
            }
            Ok::<(), McpError>(())
        }).catch_unwind().await;
        let failure = match dispatch {
            Ok(Ok(())) => false,
            Ok(Err(error)) if *output_started => {
                return Err(error);
            }
            Err(payload) if *output_started => {
                flashbrowse_core::async_runtime::discard_panic_payload(payload);
                return Err(McpError::Internal(
                    "response output failed unexpectedly".to_string(),
                ));
            }
            Ok(Err(error)) => {
                tracing::error!(%error, "MCP request failed before responding");
                true
            }
            Err(payload) => {
                flashbrowse_core::async_runtime::discard_panic_payload(payload);
                tracing::error!("MCP request panicked before its response");
                true
            }
        };
        if failure {
            let recovery = RecoveryScope::RefreshActiveTree.recover(handle, "request failure");
            if let Some(id) = request_id.clone() {
                let response = JsonRpcErrorResponse {
                    jsonrpc: "2.0".to_string(),
                    id,
                    error: JsonRpcError::new(-32603, format!("internal failure ({recovery})")),
                };
                write_frame_tracked(&mut writer, &response, output_started).await?;
            }
        }
            }
            Ok::<(), McpError>(())
        })
        .catch_unwind()
        .await;
        match handled {
            Ok(Ok(())) => {}
            Ok(Err(error)) if *output_started => {
                return Err(error);
            }
            Ok(Err(error)) => {
                tracing::error!(%error, "MCP frame handling failed before responding");
                let recovery = RecoveryScope::RefreshActiveTree.recover(handle, "frame failure");
                if request_is_notification != Some(true) {
                    let response = JsonRpcErrorResponse {
                        jsonrpc: "2.0".to_string(),
                        id: request_id.unwrap_or(Value::Null),
                        error: JsonRpcError::new(-32603, format!("internal failure ({recovery})")),
                    };
                    write_frame_tracked(&mut writer, &response, output_started).await?;
                }
            }
            Err(payload) if *output_started => {
                flashbrowse_core::async_runtime::discard_panic_payload(payload);
                return Err(McpError::Internal(
                    "response output failed unexpectedly".to_string(),
                ));
            }
            Err(payload) => {
                flashbrowse_core::async_runtime::discard_panic_payload(payload);
                tracing::error!("MCP frame handler panicked; keeping the server available");
                let recovery = RecoveryScope::RefreshActiveTree.recover(handle, "frame panic");
                if request_is_notification != Some(true) {
                    let response = JsonRpcErrorResponse {
                        jsonrpc: "2.0".to_string(),
                        id: request_id.unwrap_or(Value::Null),
                        error: JsonRpcError::new(-32603, format!("internal failure ({recovery})")),
                    };
                    write_frame_tracked(&mut writer, &response, output_started).await?;
                }
            }
        }
    }

    Ok(())
}

#[derive(Debug)]
enum BoundedLine {
    Line(Vec<u8>),
    TooLong,
}

/// Read one newline-delimited frame without ever retaining more than `max`
/// bytes of untrusted input. An oversized frame is drained through its newline
/// so the next request starts at a clean frame boundary.
async fn read_bounded_line<R>(reader: &mut R, max: usize) -> std::io::Result<Option<BoundedLine>>
where
    R: AsyncBufRead + Unpin,
{
    let mut bytes = Vec::new();
    let mut too_long = false;

    loop {
        let (consumed, ended) = {
            let available = reader.fill_buf().await?;
            if available.is_empty() {
                return if too_long {
                    Ok(Some(BoundedLine::TooLong))
                } else if bytes.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(BoundedLine::Line(bytes)))
                };
            }

            let newline = available.iter().position(|byte| *byte == b'\n');
            let content_len = newline.unwrap_or(available.len());
            if !too_long {
                if content_len > max.saturating_sub(bytes.len()) {
                    too_long = true;
                    bytes.clear();
                } else {
                    let content = available.get(..content_len).ok_or_else(|| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "invalid MCP frame length",
                        )
                    })?;
                    bytes.extend_from_slice(content);
                }
            }
            (
                newline.map_or(available.len(), |index| index.saturating_add(1)),
                newline.is_some(),
            )
        };
        reader.consume(consumed);

        if ended {
            return Ok(Some(if too_long {
                BoundedLine::TooLong
            } else {
                BoundedLine::Line(bytes)
            }));
        }
    }
}

#[derive(Clone, Copy)]
enum RecoveryScope {
    ReadOnly,
    RefreshActiveTree,
    ResetSessions,
}

impl RecoveryScope {
    fn for_tool(name: &str, arguments: &Value) -> Self {
        let action = arguments.get("action").and_then(Value::as_str);
        match (name, action) {
            ("browser_contexts", Some("create" | "close")) => Self::RefreshActiveTree,
            ("browser_extract_markdown" | "browser_downloads", _)
            | ("browser_contexts" | "browser_console" | "browser_route", None | Some("list"))
            | ("browser_network", None | Some("list" | "body"))
            | ("browser_tabs", Some("list")) => Self::ReadOnly,
            _ => Self::RefreshActiveTree,
        }
    }

    fn recover(self, handle: &mut tools::SessionHandle, reason: &'static str) -> &'static str {
        if !matches!(self, Self::ResetSessions) {
            if let Ok(mut sessions) = handle.try_lock() {
                if sessions.as_ref().is_none_or(|s| s.client().is_connected()) {
                    if matches!(self, Self::RefreshActiveTree) {
                        if let Some(session) = sessions.as_mut() {
                            session.invalidate_tree();
                        }
                        return "browser contexts preserved; refresh the active tree before using action IDs";
                    }
                    return "browser contexts preserved";
                }
            }
        }
        retire_sessions(handle, reason);
        "browser session reset"
    }
}

/// Recover only after the failed task has stopped accessing the manager.
async fn run_isolated_tool_call<F>(
    timeout_dur: Duration,
    tool_name: &str,
    scope: RecoveryScope,
    handle: &mut tools::SessionHandle,
    fut: F,
) -> Result<ToolResult, McpError>
where
    F: Future<Output = Result<ToolResult, McpError>> + Send + 'static,
{
    let mut task = tokio::spawn(flashbrowse_core::async_runtime::track_contained_panics(fut));
    let _abort_on_drop = AbortOnDrop(task.abort_handle());
    match tokio::time::timeout(timeout_dur, &mut task).await {
        Err(_) => {
            task.abort();
            tracing::warn!(
                tool = tool_name,
                "tools/call timed out; cancellation requested"
            );
            let stopped = match tokio::time::timeout(Duration::from_millis(250), task).await {
                Ok(Err(error)) if error.is_panic() => {
                    flashbrowse_core::async_runtime::discard_panic_payload(error.into_panic());
                    true
                }
                Ok(_) => true,
                Err(_) => false,
            };
            let scope = if stopped {
                scope
            } else {
                RecoveryScope::ResetSessions
            };
            let recovery = scope.recover(handle, "tool timeout");
            Err(McpError::Internal(format!(
                "tools/call timed out ({recovery})"
            )))
        }
        Ok(Err(join_err)) if join_err.is_panic() => {
            tracing::error!(tool = tool_name, "tools/call panicked");
            flashbrowse_core::async_runtime::discard_panic_payload(join_err.into_panic());
            let recovery = scope.recover(handle, "tool panic");
            Err(McpError::Internal(format!("tool panicked ({recovery})")))
        }
        Ok(Err(_)) => {
            let recovery = scope.recover(handle, "tool cancellation");
            Err(McpError::Internal(format!(
                "tools/call cancelled ({recovery})"
            )))
        }
        Ok(Ok((_, true))) => {
            tracing::error!(tool = tool_name, "tools/call contained an internal panic");
            let recovery = scope.recover(handle, "tool panic");
            Err(McpError::Internal(format!("tool panicked ({recovery})")))
        }
        Ok(Ok((res, false))) => res,
    }
}

struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn retire_sessions(handle: &mut tools::SessionHandle, reason: &'static str) {
    let retired = std::mem::replace(handle, Arc::new(Mutex::new(tools::SessionManager::new())));
    tokio::spawn(async move {
        cleanup_sessions(&retired, reason).await;
    });
}

/// Run browser cleanup in a supervised task. A stuck CDP peer or panic during
/// teardown must not prevent the MCP server from returning its error response.
async fn cleanup_sessions(handle: &tools::SessionHandle, reason: &'static str) {
    let cleanup_handle = Arc::clone(handle);
    let mut cleanup = tokio::spawn(async move {
        cleanup_handle.lock().await.shutdown_all().await;
    });
    match tokio::time::timeout(Duration::from_secs(10), &mut cleanup).await {
        Ok(Ok(())) => {}
        Ok(Err(join_error)) => {
            tracing::error!(%join_error, reason, "browser session cleanup task failed");
            if join_error.is_panic() {
                flashbrowse_core::async_runtime::discard_panic_payload(join_error.into_panic());
            }
        }
        Err(_) => {
            cleanup.abort();
            tracing::warn!(
                reason,
                "browser session cleanup exceeded 10 seconds and was aborted"
            );
        }
    }
}

/// Install a stderr-only panic hook once so panics inside spawned MCP tasks
/// never pollute stdout (which must remain pure JSON-RPC).
fn install_stderr_panic_hook() {
    static HOOK: Once = Once::new();
    HOOK.call_once(|| {
        std::panic::set_hook(Box::new(|info| {
            use std::io::Write;
            let mut stderr = std::io::stderr().lock();
            if let Some(location) = info.location() {
                let _ = writeln!(
                    stderr,
                    "flashbrowse mcp: internal panic at {}:{}:{}",
                    location.file(),
                    location.line(),
                    location.column()
                );
            } else {
                let _ = writeln!(
                    stderr,
                    "flashbrowse mcp: internal panic at unknown location"
                );
            }
        }));
    });
}

/// Entry point wiring real stdin/stdout + stderr-only logging.
///
/// Call this from the `flashbrowse mcp` CLI command.
pub async fn serve_stdio(config: SessionConfig) -> Result<(), McpError> {
    install_stderr_panic_hook();
    tracing_subscriber::fmt()
        .log_internal_errors(false)
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init()
        .ok();
    flashbrowse_core::async_runtime::run(serve_stdio_inner(config))
        .await
        .map_err(|error| McpError::Internal(error.to_string()))?
}

async fn serve_stdio_inner(config: SessionConfig) -> Result<(), McpError> {
    let server = tokio::spawn(serve(tokio::io::stdin(), tokio::io::stdout(), config));
    let _abort_on_drop = AbortOnDrop(server.abort_handle());
    match server.await {
        Ok(result) => result,
        Err(join_error) if join_error.is_panic() => {
            tracing::error!(%join_error, "MCP server task stopped after an internal panic");
            flashbrowse_core::async_runtime::discard_panic_payload(join_error.into_panic());
            Err(McpError::Internal(
                "MCP server stopped after an internal failure".to_string(),
            ))
        }
        Err(join_error) => {
            tracing::error!(%join_error, "MCP server task was cancelled");
            Err(McpError::Internal(
                "MCP server task was cancelled".to_string(),
            ))
        }
    }
}

async fn write_frame_tracked<W>(
    writer: &mut W,
    frame: &impl serde::Serialize,
    started: &mut bool,
) -> Result<(), McpError>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut line = serde_json::to_string(frame)?;
    line.push('\n');
    let bytes = line.as_bytes();
    let mut offset = 0usize;
    while offset < bytes.len() {
        let remaining = bytes.get(offset..).ok_or_else(|| {
            McpError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid MCP response write offset",
            ))
        })?;
        let written = match writer.write(remaining).await {
            Ok(written) => written,
            Err(error) => {
                // A writer that panics during poll may have changed its
                // destination before panicking. Treat the output as partial
                // and close this connection rather than append a new frame.
                if flashbrowse_core::async_runtime::is_io_poll_panic(&error) {
                    *started = true;
                }
                return Err(McpError::Io(error));
            }
        };
        if written == 0 {
            return Err(McpError::Io(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "MCP response writer accepted zero bytes",
            )));
        }
        if written > remaining.len() {
            *started = true;
            return Err(McpError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "MCP response writer reported more bytes than provided",
            )));
        }
        *started = true;
        offset = offset.checked_add(written).ok_or_else(|| {
            McpError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "MCP response write offset overflow",
            ))
        })?;
    }
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
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
                RecoveryScope::for_tool(
                    "browser_contexts",
                    &serde_json::json!({ "action": action })
                ),
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

        let storage =
            || json!({"expression":"document.cookie + '|' + localStorage.getItem('ctx')"});
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
            serde_json::from_str(&call("browser_contexts", json!({ "action": "list" })).await)
                .unwrap();
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
                    .all(|tab| tab.browser_context_id.as_deref()
                        != Some(isolated_context_id.as_str())),
                "default tab listing included an isolated context: {tabs:?}"
            );
        }
        manager.shutdown_all().await;
        http.abort();
    }
}
