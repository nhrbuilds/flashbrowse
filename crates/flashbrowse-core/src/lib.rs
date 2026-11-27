//! `flashbrowse-core`: compression, action graph, stealth, navigation logic.
//!
//! ```text
//! BrowserSession ──▶ stealth + interceptor ──▶ Page.navigate
//!       └──▶ Accessibility.getFullAXTree ──▶ tree::compress ──▶ tree::render
//!       └──▶ actions::{click, fill, scroll, press_key} ──▶ rebuild_tree
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
compile_error!("flashbrowse requires panic unwinding for panic recovery.");

pub mod actions;
pub mod driver;
pub mod engine;
pub mod interceptor;
pub mod markdown;
pub mod observability;
pub mod observability_session;
pub mod paths;
pub mod resource_policy;
pub mod session;
pub mod stealth;
pub mod tree;
pub mod url_norm;

pub use engine::BrowserEngine;
#[doc(hidden)]
pub use flashbrowse_cdp::runtime as async_runtime;
pub use observability_session::{ConsoleEntry, NetworkEntry, RouteRule};
pub use resource_policy::ResourcePolicy;
pub use session::{
    BrowserSession, CoreError, DetachedSession, DialogInfo, DialogPolicy, EmulationOptions,
    GeolocationOptions, PdfOptions, ScreenshotOptions, SessionConfig, ViewportOptions,
    WaitCondition, SCREENSHOT_PREPARE_MS_DEFAULT, SCREENSHOT_PREPARE_MS_MAX,
};
pub use stealth::StealthProfile;
pub use tree::{
    compress_with_options, render, render_nested, ActionMap, CompressOptions, SemanticNode,
};
pub use url_norm::{normalize_url, UrlError};
