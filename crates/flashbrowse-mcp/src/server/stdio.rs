use super::recovery::AbortOnDrop;
use super::*;

/// Install a stderr-only panic hook once so panics inside spawned MCP tasks
/// never pollute stdout (which must remain pure JSON-RPC).
pub(super) fn install_stderr_panic_hook() {
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
            tracing::error!(target: "flashbrowse_mcp::server", %join_error, "MCP server task stopped after an internal panic");
            flashbrowse_core::async_runtime::discard_panic_payload(join_error.into_panic());
            Err(McpError::Internal(
                "MCP server stopped after an internal failure".to_string(),
            ))
        }
        Err(join_error) => {
            tracing::error!(target: "flashbrowse_mcp::server", %join_error, "MCP server task was cancelled");
            Err(McpError::Internal(
                "MCP server task was cancelled".to_string(),
            ))
        }
    }
}
