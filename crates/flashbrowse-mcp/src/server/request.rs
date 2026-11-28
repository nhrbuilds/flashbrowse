use super::recovery::{run_isolated_tool_call, RecoveryScope};
use super::*;

pub(super) async fn serve_loop<R, W>(
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
                tracing::error!(target: "flashbrowse_mcp::server", %error, "MCP request failed before responding");
                true
            }
            Err(payload) => {
                flashbrowse_core::async_runtime::discard_panic_payload(payload);
                tracing::error!(target: "flashbrowse_mcp::server", "MCP request panicked before its response");
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
                tracing::error!(target: "flashbrowse_mcp::server", %error, "MCP frame handling failed before responding");
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
                tracing::error!(target: "flashbrowse_mcp::server", "MCP frame handler panicked; keeping the server available");
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
