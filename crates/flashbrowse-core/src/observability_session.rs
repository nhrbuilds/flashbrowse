//! Per-session console log, network request/response log, and lazy Fetch route mocking.

use std::collections::VecDeque;
use std::future::Future;
use std::sync::{Arc, Mutex};

use flashbrowse_cdp::protocol::fetch;
use flashbrowse_cdp::CdpClient;
use futures_util::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

mod console;
mod network;
mod routing;

pub(crate) use console::recv_until_stop;

const MAX_LOG_ENTRIES: usize = 200;

/// One captured `console.*` call or uncaught JS exception.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsoleEntry {
    pub level: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<i64>,
}

/// Bounded ring-buffer collector for `Runtime.consoleAPICalled` and `Runtime.exceptionThrown`.
#[derive(Debug, Clone)]
pub struct ConsoleTracker {
    entries: Arc<Mutex<VecDeque<ConsoleEntry>>>,
}

/// One captured network request/response entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkEntry {
    pub request_id: String,
    pub url: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    pub failed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_text: Option<String>,
}

/// Bounded ring-buffer collector for `Network.*` events scoped to one `session_id`.
#[derive(Debug, Clone)]
pub struct NetworkLogTracker {
    entries: Arc<Mutex<VecDeque<NetworkEntry>>>,
}

/// One route mock or abort rule for `Fetch.requestPaused`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteRule {
    pub url_pattern: String,
    pub status: u16,
    pub content_type: String,
    pub body: String,
    pub abort: bool,
}

/// Lazy `Fetch` domain interceptor scoped to one `session_id`.
#[derive(Debug, Clone)]
pub struct RouteInterceptor {
    state: Arc<Mutex<RouteState>>,
}

#[derive(Debug, Default)]
struct RouteState {
    rules: Vec<RouteRule>,
    failed: bool,
}

pub(crate) fn spawn_supervised(
    client: &Arc<CdpClient>,
    task: impl Future<Output = ()> + Send + 'static,
) {
    let worker_client = client.clone();
    let result = flashbrowse_cdp::runtime::spawn(async move {
        if let Err(payload) = std::panic::AssertUnwindSafe(task).catch_unwind().await {
            flashbrowse_cdp::runtime::discard_panic_payload(payload);
            tracing::error!("browser event task failed; disconnecting browser session");
            worker_client.shutdown();
        }
    });
    if let Err(error) = result {
        tracing::error!(%error, "could not start browser event task");
        client.shutdown();
    }
}

fn push_bounded<T>(buf: &Mutex<VecDeque<T>>, item: T) {
    if let Ok(mut guard) = buf.lock() {
        if guard.len() >= MAX_LOG_ENTRIES {
            guard.pop_front();
        }
        guard.push_back(item);
    }
}

/// Wildcard (`*`) or substring URL pattern matching.
pub fn url_matches(pattern: &str, url: &str) -> bool {
    if pattern.is_empty() {
        return false;
    }
    if !pattern.contains('*') {
        return url.contains(pattern);
    }
    let pat: Vec<char> = pattern.chars().collect();
    let txt: Vec<char> = url.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star_pi, mut star_ti) = (None::<usize>, 0usize);
    while ti < txt.len() {
        if pat.get(pi) == Some(&'*') {
            star_pi = Some(pi);
            star_ti = ti;
            pi = pi.saturating_add(1);
        } else if pat.get(pi) == txt.get(ti) {
            pi = pi.saturating_add(1);
            ti = ti.saturating_add(1);
        } else if let Some(sp) = star_pi {
            pi = sp.saturating_add(1);
            star_ti = star_ti.saturating_add(1);
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pat.get(pi) == Some(&'*') {
        pi = pi.saturating_add(1);
    }
    pi == pat.len()
}

#[cfg(test)]
#[path = "observability_session/tests.rs"]
mod tests;
