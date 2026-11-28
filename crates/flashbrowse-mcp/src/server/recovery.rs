use super::*;

#[derive(Clone, Copy)]
pub(super) enum RecoveryScope {
    ReadOnly,
    RefreshActiveTree,
    ResetSessions,
}

impl RecoveryScope {
    pub(super) fn for_tool(name: &str, arguments: &Value) -> Self {
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

    pub(super) fn recover(
        self,
        handle: &mut tools::SessionHandle,
        reason: &'static str,
    ) -> &'static str {
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
pub(super) async fn run_isolated_tool_call<F>(
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
            tracing::warn!(target: "flashbrowse_mcp::server", tool = tool_name,
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
            tracing::error!(target: "flashbrowse_mcp::server", tool = tool_name, "tools/call panicked");
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
            tracing::error!(target: "flashbrowse_mcp::server", tool = tool_name, "tools/call contained an internal panic");
            let recovery = scope.recover(handle, "tool panic");
            Err(McpError::Internal(format!("tool panicked ({recovery})")))
        }
        Ok(Ok((res, false))) => res,
    }
}

pub(super) struct AbortOnDrop(pub(super) tokio::task::AbortHandle);

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
pub(super) async fn cleanup_sessions(handle: &tools::SessionHandle, reason: &'static str) {
    let cleanup_handle = Arc::clone(handle);
    let mut cleanup = tokio::spawn(async move {
        cleanup_handle.lock().await.shutdown_all().await;
    });
    match tokio::time::timeout(Duration::from_secs(10), &mut cleanup).await {
        Ok(Ok(())) => {}
        Ok(Err(join_error)) => {
            tracing::error!(target: "flashbrowse_mcp::server", %join_error, reason, "browser session cleanup task failed");
            if join_error.is_panic() {
                flashbrowse_core::async_runtime::discard_panic_payload(join_error.into_panic());
            }
        }
        Err(_) => {
            cleanup.abort();
            tracing::warn!(target: "flashbrowse_mcp::server", reason,
                "browser session cleanup exceeded 10 seconds and was aborted"
            );
        }
    }
}
