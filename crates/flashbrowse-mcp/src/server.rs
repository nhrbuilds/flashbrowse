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

mod framing;
mod recovery;
mod request;
mod stdio;

use framing::{read_bounded_line, write_frame_tracked, BoundedLine};
use recovery::cleanup_sessions;
#[cfg(test)]
use recovery::{run_isolated_tool_call, RecoveryScope};
use request::serve_loop;

pub use stdio::serve_stdio;

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

#[cfg(test)]
#[path = "server/tests.rs"]
mod tests;
