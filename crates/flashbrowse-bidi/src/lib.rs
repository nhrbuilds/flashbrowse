//! Firefox WebDriver BiDi integration (milestone 3 groundwork).
//!
//! Chromium sessions use `flashbrowse-cdp`. Firefox will attach over BiDi
//! (`browsingContext.captureScreenshot`, navigation, input) once a full
//! accessibility-tree snapshot exists or a DOM adapter feeds the compressor.
//!
//! This crate currently provides binary discovery and launch option types only.

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

mod launcher;

pub use launcher::{find_firefox, firefox_version, FirefoxLaunchOptions, FirefoxNotAvailable};

/// Experimental flag: tree parity on Firefox is not production-ready.
pub const FIREFOX_EXPERIMENTAL: bool = true;
