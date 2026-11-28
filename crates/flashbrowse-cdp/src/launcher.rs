//! Platform-agnostic Chromium discovery, process spawning, stderr scraping.
//!
//! Priority order for binary resolution (locked decision):
//! 1. explicit `--chrome-path` / `LaunchOptions::chrome_path`
//! 2. `CHROME_PATH` env
//! 3. `CHROME_BINARY` env
//! 4. PATH: `google-chrome-stable`, `google-chrome`, `chromium`, `chromium-browser`
//! 5. macOS: `/Applications/Google Chrome.app/...`, `/Applications/Chromium.app/...`
//! 6. Windows: `Program Files/Google/Chrome/...`, `LocalAppData/Google/Chrome/...`

mod discovery;
mod process;
mod profile;
mod startup;

pub use discovery::find_chrome;
#[cfg(all(test, unix))]
use process::kill_chrome_process_group_without_panicking;
#[cfg(all(test, unix))]
use startup::configure_process_group;
pub use startup::{launch_chrome, launch_chrome_ws, launch_connected_chrome};

use std::collections::hash_map::DefaultHasher;
use std::env;
use std::fs::create_dir_all;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
#[cfg(unix)]
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::oneshot;
use tokio::time::timeout;

use crate::error::CdpError;

/// Seconds to wait for `DevTools listening on ...` on stderr.
const DEVTOOLS_TIMEOUT_SECS: u64 = 20;

/// Async pipes for `--remote-debugging-pipe` on Unix.
#[cfg(unix)]
pub struct PipeHandles {
    pub read_pipe: ChildStdout,
    pub write_pipe: ChildStdin,
}

/// Handle to a running Chromium process.
///
/// On drop, the child is killed and reaped on a best-effort basis so CLI exit
/// never leaks a browser. Callers needing persistence (session files) should
/// detach ownership explicitly (e.g. keep pid + ws_url, then drop the guard
/// after killing a clone - no, use `detach`).
pub struct ChromeProcess {
    child: Option<Child>,
    browser_ws_url: String,
    chrome_path: PathBuf,
    user_data_dir: PathBuf,
    /// `true` only for auto-created temp dirs - caller-supplied
    /// `--user-data-dir` is never deleted on drop (H1 fix).
    owned_dir: bool,
    #[cfg(unix)]
    pipe_handles: Option<PipeHandles>,
}

/// Matches Chromium stderr line, e.g.
/// `DevTools listening on ws://127.0.0.1:9222/devtools/browser/abc-123`
fn parse_devtools_ws_url(line: &str) -> Option<&str> {
    let (_, suffix) = line.split_once("DevTools listening on ")?;
    let token = suffix.split_whitespace().next()?;
    let (host, rest) = token
        .strip_prefix("ws://")?
        .split_once("/devtools/browser/")?;
    if host.is_empty() {
        return None;
    }
    let id_len = rest
        .bytes()
        .take_while(|byte| byte.is_ascii_hexdigit() || *byte == b'-')
        .count();
    if id_len == 0 {
        return None;
    }
    let suffix = rest.get(id_len..)?;
    token.strip_suffix(suffix)
}

/// Options for launching a headless Chromium instance.
#[derive(Debug, Clone)]
pub struct LaunchOptions {
    /// Explicit binary path (highest priority).
    pub chrome_path: Option<PathBuf>,
    /// Extra CLI args appended after the hardened defaults.
    pub extra_args: Vec<String>,
    /// If `Some(true)`, pass `--no-sandbox` (containers / CI).
    /// Defaults from `FLASHBROWSE_NO_SANDBOX` env at launch time.
    pub no_sandbox: Option<bool>,
    /// If `Some(dir)`, use as `--user-data-dir`. Otherwise a fresh temp dir.
    pub user_data_dir: Option<PathBuf>,
    /// Window size `WxH`, default `1280x800`.
    pub window_size: (u32, u32),
    /// Use WebSocket so detached persistent sessions survive CLI exit.
    pub persistent: bool,
}

impl Default for LaunchOptions {
    fn default() -> Self {
        Self {
            chrome_path: None,
            extra_args: Vec::new(),
            no_sandbox: None,
            user_data_dir: None,
            window_size: (1280, 800),
            persistent: false,
        }
    }
}

#[cfg(test)]
#[path = "launcher/tests.rs"]
mod tests;
