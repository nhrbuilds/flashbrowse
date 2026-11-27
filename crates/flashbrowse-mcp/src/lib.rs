//! `flashbrowse-mcp`: MCP JSON-RPC 2.0 stdio server (2025-06-18, fallback
//! 2025-03-26 → 2024-11-05).
//!
//! Browser operations use flashbrowse's Tokio runtime with I/O and time enabled.
//!
//! ```text
//! serve_stdio() ──▶ stdin lines ──▶ dispatch (initialize / tools/*)
//!                                        └──▶ flashbrowse_core::BrowserSession
//! stdout ◀── one JSON-RPC frame per line, pure JSON (never logs)
//! stderr ◀── tracing (stderr-only subscriber)
//! ```

#![forbid(unsafe_code)]
#![cfg_attr(
    not(test),
    deny(
        clippy::arithmetic_side_effects,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::print_stderr,
        clippy::print_stdout,
        clippy::string_slice,
        clippy::panic,
        clippy::todo,
        clippy::unimplemented,
        clippy::unreachable,
        clippy::unwrap_used
    )
)]

#[cfg(all(not(test), panic = "abort"))]
compile_error!("flashbrowse requires panic unwinding for MCP recovery.");

pub mod protocol;
pub mod server;
pub mod tools;

pub use protocol::{McpError, ToolResult};
pub use server::{serve, serve_stdio};
pub use tools::{call_tool, tool_definitions, SessionHandle};

/// Crate identifier exposed by this library.
pub const CRATE_NAME: &str = "flashbrowse-mcp";
