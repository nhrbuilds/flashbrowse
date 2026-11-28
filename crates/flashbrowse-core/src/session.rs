//! High-level browser session controller.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use flashbrowse_cdp::protocol::{
    browser, dom, emulation, fetch, input, network, page, runtime, target,
};
use flashbrowse_cdp::{CdpClient, CdpError, ChromeProcess, LaunchOptions};
use serde_json::Value;

use crate::actions::fill::{backspace_event, select_all_event};
use crate::actions::{box_center, ScrollDirection};
use crate::driver::{BrowserDriver, CdpDriver, FrameSession};
use crate::engine::BrowserEngine;
use crate::interceptor::{blocked_patterns, blocked_patterns_for_screenshot};
use crate::markdown::EXTRACT_SCRIPT;
use crate::observability_session::{
    recv_until_stop, spawn_supervised, ConsoleEntry, ConsoleTracker, NetworkEntry,
    NetworkLogTracker, RouteInterceptor, RouteRule,
};
use crate::resource_policy::ResourcePolicy;
use crate::stealth::{
    build_ua_override, stealth_script, StealthProfile, SETTLE_DIAGNOSTICS_SCRIPT,
};
use crate::tree::{compress, render_nested, ActionMap};

mod actions;
mod capture;
mod lifecycle;
mod navigation;
mod observability;
mod settling;
mod storage;
mod tabs;

use settling::NetActivity;

/// Best-effort cleanup for an isolated Chrome context while unwinding or when
/// an operation is cancelled. The explicit async cleanup path disarms this
/// guard only after Chrome confirms disposal.
struct BrowserContextCleanup {
    client: Arc<CdpClient>,
    browser_context_id: Option<String>,
}

impl BrowserContextCleanup {
    fn new(client: Arc<CdpClient>, browser_context_id: String) -> Self {
        Self {
            client,
            browser_context_id: Some(browser_context_id),
        }
    }

    async fn dispose(&mut self) -> Result<(), CdpError> {
        let Some(browser_context_id) = self.browser_context_id.as_deref() else {
            return Ok(());
        };
        target::dispose_browser_context(&self.client, browser_context_id).await?;
        self.browser_context_id = None;
        Ok(())
    }
}

impl Drop for BrowserContextCleanup {
    fn drop(&mut self) {
        let Some(browser_context_id) = self.browser_context_id.take() else {
            return;
        };
        let client = Arc::clone(&self.client);
        let schedule = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            match flashbrowse_cdp::runtime::spawn(async move {
                match flashbrowse_cdp::runtime::run(target::dispose_browser_context(
                    &client,
                    &browser_context_id,
                ))
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) | Err(error) => {
                        tracing::warn!(%error, "deferred isolated browser context cleanup failed");
                    }
                }
            }) {
                Ok(_task) => {}
                Err(error) => {
                    tracing::warn!(%error, "could not schedule isolated browser context cleanup");
                }
            }
        }));
        if let Err(payload) = schedule {
            flashbrowse_cdp::runtime::discard_panic_payload(payload);
        }
    }
}

/// Close the CDP client if screenshot preparation exits before restoring the
/// active resource policy. This also runs while an internal panic unwinds, so
/// callers cannot keep using a session with screenshot-only policy applied.
struct ScreenshotGuard<'a> {
    client: &'a CdpClient,
    armed: bool,
}

impl Drop for ScreenshotGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            if let Err(payload) =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.client.shutdown()))
            {
                flashbrowse_cdp::runtime::discard_panic_payload(payload);
            }
        }
    }
}

const SETTLE_DOM_STATS_JS: &str = r#"(() => {
  let count = 0;
  let textLen = document.body ? document.body.innerText.length : 0;
  const pending = [document];
  while (pending.length) {
    const root = pending.pop();
    const els = root.querySelectorAll('*');
    count += els.length;
    for (const el of els) {
      if (el.shadowRoot) {
        textLen += (el.shadowRoot.textContent || '').length;
        pending.push(el.shadowRoot);
      }
    }
  }
  return `${document.readyState}|${count}|${textLen}`;
})()"#;

/// Errors surfaced by the core engine (maps to MCP `tools/call` error).
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("CDP error: {0}")]
    Cdp(#[from] CdpError),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("action id [{0}] not in current action map")]
    ActionNotFound(u32),
    #[error("action id [{0}] has no backend node handle")]
    NoBackendId(u32),
    #[error("action id [{0}] has no box model")]
    NoBoxModel(u32),
    #[error("invalid scroll direction: {0}")]
    InvalidScrollDirection(String),
    #[error("navigation failed (browser error page): {0}")]
    NavigationFailed(String),
    #[error("unsupported browser engine: {0}")]
    UnsupportedEngine(String),
}

/// Session launch configuration.
#[derive(Debug, Clone, Default)]
pub struct SessionConfig {
    /// Explicit Chrome path (`--chrome-path` / `CHROME_PATH`).
    pub chrome_path: Option<PathBuf>,
    /// Network resource load policy (AX tree format is unchanged).
    pub resource_policy: ResourcePolicy,
    /// Browser engine (`firefox` requires `flashbrowse-bidi`, not yet wired in CLI).
    pub engine: BrowserEngine,
    /// Pass `--no-sandbox` (containers/CI). `None` → `FLASHBROWSE_NO_SANDBOX` env.
    pub no_sandbox: Option<bool>,
    /// Extra Chromium CLI args.
    pub extra_args: Vec<String>,
    /// Keep Chromium alive after parent CLI exit (`open --session-id`).
    pub persistent: bool,
    /// Default disposition for native JavaScript dialogs.
    pub dialog_policy: DialogPolicy,
    /// Use an existing Chrome profile when launching a managed browser.
    pub user_data_dir: Option<PathBuf>,
    /// Attach to an existing Chrome debugging endpoint instead of launching.
    pub cdp_url: Option<String>,
    /// Root directory allowed for MCP storage-state files.
    pub storage_state_root: Option<PathBuf>,
    /// Root for MCP upload inputs and browser downloads.
    pub mcp_file_root: Option<PathBuf>,
    /// Stealth User-Agent/platform/Client-Hints profile (`host-native` default).
    pub stealth_profile: StealthProfile,
}

/// Native JavaScript dialog handling policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DialogPolicy {
    /// Accept alerts; dismiss confirms and prompts to avoid blocking the page.
    #[default]
    SafeDefault,
    /// Accept all dialog types (prompts receive no text).
    AcceptAll,
    /// Dismiss all dialog types.
    DismissAll,
}

#[derive(Debug, Clone)]
pub struct DialogInfo {
    pub dialog_type: String,
    pub message: String,
    pub accepted: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DownloadInfo {
    pub guid: String,
    pub url: String,
    pub suggested_filename: String,
    pub received_bytes: f64,
    pub total_bytes: f64,
    pub state: String,
}

#[derive(Debug, Default)]
struct DialogState {
    policy: DialogPolicy,
    override_decision: Option<(bool, Option<String>)>,
    last_dialog: Option<DialogInfo>,
}

type DownloadTracker = Arc<std::sync::Mutex<HashMap<String, DownloadInfo>>>;

fn download_directory(config: &SessionConfig) -> PathBuf {
    config
        .mcp_file_root
        .clone()
        .unwrap_or_else(|| crate::paths::cache_dir().join("files"))
        .join("downloads")
}

fn install_download_tracker(
    client: &Arc<CdpClient>,
    stop: tokio::sync::watch::Receiver<()>,
) -> DownloadTracker {
    let downloads: DownloadTracker = Arc::default();
    let mut begun = client.events().subscribe("Browser.downloadWillBegin");
    let mut progress = client.events().subscribe("Browser.downloadProgress");
    let begin_state = downloads.clone();
    let progress_state = downloads.clone();
    let mut stop_begin = stop.clone();
    spawn_supervised(client, async move {
        while let Some(event) = recv_until_stop(&mut begun, &mut stop_begin).await {
            let Some(guid) = event.get("guid").and_then(Value::as_str) else {
                continue;
            };
            if let Ok(mut downloads) = begin_state.lock() {
                downloads.insert(
                    guid.to_string(),
                    DownloadInfo {
                        guid: guid.to_string(),
                        url: event
                            .get("url")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        suggested_filename: event
                            .get("suggestedFilename")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        received_bytes: 0.0,
                        total_bytes: 0.0,
                        state: "inProgress".to_string(),
                    },
                );
            }
        }
    });
    let mut stop_progress = stop;
    spawn_supervised(client, async move {
        while let Some(event) = recv_until_stop(&mut progress, &mut stop_progress).await {
            let Some(guid) = event.get("guid").and_then(Value::as_str) else {
                continue;
            };
            if let Ok(mut downloads) = progress_state.lock() {
                if let Some(info) = downloads.get_mut(guid) {
                    info.received_bytes = event
                        .get("receivedBytes")
                        .and_then(Value::as_f64)
                        .unwrap_or(info.received_bytes);
                    info.total_bytes = event
                        .get("totalBytes")
                        .and_then(Value::as_f64)
                        .unwrap_or(info.total_bytes);
                    info.state = event
                        .get("state")
                        .and_then(Value::as_str)
                        .unwrap_or(&info.state)
                        .to_string();
                }
            }
        }
    });
    downloads
}

impl SessionConfig {
    /// Launch is Chrome-only until BiDi Firefox lands.
    pub fn validate_engine(&self) -> Result<(), CoreError> {
        if self.engine != BrowserEngine::Chrome {
            return Err(CoreError::UnsupportedEngine(format!(
                "{} (chrome only today)",
                self.engine.as_str()
            )));
        }
        if self.cdp_url.is_some() && self.user_data_dir.is_some() {
            return Err(CoreError::NavigationFailed(
                "--cdp-url and --user-data-dir cannot be used together".into(),
            ));
        }
        Ok(())
    }
}

type PopupTracker = Arc<std::sync::Mutex<Vec<target::TargetInfo>>>;

fn record_popup_target(tracker: &PopupTracker, info: target::TargetInfo) {
    if info.kind != "page" || info.opener_id.is_none() {
        return;
    }
    if let Ok(mut list) = tracker.lock() {
        if let Some(pos) = list.iter().position(|t| t.target_id == info.target_id) {
            if let Some(existing) = list.get_mut(pos) {
                *existing = info;
            }
        } else {
            list.push(info);
        }
    }
}

fn take_popup_from_tracker(
    tracker: &PopupTracker,
    opener_id: &str,
    existing: &std::collections::HashSet<String>,
) -> Option<target::TargetInfo> {
    let Ok(mut list) = tracker.lock() else {
        return None;
    };
    let pos = list.iter().rposition(|t| {
        t.kind == "page"
            && t.opener_id.as_deref() == Some(opener_id)
            && !existing.contains(&t.target_id)
    })?;
    let picked = list.remove(pos);
    list.retain(|t| t.opener_id.as_deref() != Some(opener_id) || existing.contains(&t.target_id));
    Some(picked)
}

fn install_popup_tracker(
    client: &Arc<CdpClient>,
    stop: tokio::sync::watch::Receiver<()>,
) -> PopupTracker {
    let tracker: PopupTracker = Arc::new(std::sync::Mutex::new(Vec::new()));

    let mut created_rx = client.events().subscribe("Target.targetCreated");
    let t_created = tracker.clone();
    let mut stop_created = stop.clone();
    spawn_supervised(client, async move {
        while let Some(v) = recv_until_stop(&mut created_rx, &mut stop_created).await {
            if let Ok(ev) = serde_json::from_value::<target::TargetCreatedEvent>(v) {
                record_popup_target(&t_created, ev.target_info);
            }
        }
    });

    let mut attached_rx = client.events().subscribe("Target.attachedToTarget");
    let t_attached = tracker.clone();
    let mut stop_attached = stop;
    spawn_supervised(client, async move {
        while let Some(v) = recv_until_stop(&mut attached_rx, &mut stop_attached).await {
            if let Ok(ev) = serde_json::from_value::<target::AttachedToTargetEvent>(v) {
                record_popup_target(&t_attached, ev.target_info);
            }
        }
    });

    tracker
}

fn settle_shadow_enabled() -> bool {
    std::env::var("FLASHBROWSE_SETTLE_SHADOW").is_ok_and(|value| value == "1")
}

async fn apply_stealth_profile(
    client: &Arc<CdpClient>,
    session_id: &str,
    profile: StealthProfile,
) -> Result<(), CoreError> {
    let version = browser::get_version(client).await?;
    let params = build_ua_override(version.user_agent.as_deref().unwrap_or(""), profile);
    let _: Value = client
        .send(Some(session_id), "Emulation.setUserAgentOverride", params)
        .await?;
    Ok(())
}

fn install_dialog_handler(
    client: &Arc<CdpClient>,
    session_id: &str,
    policy: DialogPolicy,
    mut stop: tokio::sync::watch::Receiver<()>,
) -> Arc<std::sync::Mutex<DialogState>> {
    let state = Arc::new(std::sync::Mutex::new(DialogState {
        policy,
        ..DialogState::default()
    }));
    let mut events = client
        .events()
        .subscribe_envelopes("Page.javascriptDialogOpening");
    let state_for_task = Arc::downgrade(&state);
    let dialog_client = client.clone();
    let session_id = session_id.to_string();
    spawn_supervised(client, async move {
        while let Some(env) = recv_until_stop(&mut events, &mut stop).await {
            if env.session_id.as_deref() != Some(session_id.as_str()) {
                continue;
            }
            let event = env.params;
            let Some(state_arc) = state_for_task.upgrade() else {
                break;
            };
            let dialog_type = event
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("alert")
                .to_string();
            let message = event
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let (accepted, prompt_text) = {
                let Ok(mut state) = state_arc.lock() else {
                    continue;
                };
                let (accepted, prompt_text) = match state.override_decision.take() {
                    Some(decision) => decision,
                    None => match state.policy {
                        DialogPolicy::SafeDefault => (dialog_type == "alert", None),
                        DialogPolicy::AcceptAll => (true, None),
                        DialogPolicy::DismissAll => (false, None),
                    },
                };
                (accepted, prompt_text)
            };
            let mut params = serde_json::Map::from_iter([("accept".into(), Value::Bool(accepted))]);
            if let Some(prompt_text) = prompt_text {
                params.insert("promptText".into(), Value::String(prompt_text));
            }
            let response: Result<Value, CdpError> = dialog_client
                .send(
                    Some(&session_id),
                    "Page.handleJavaScriptDialog",
                    Value::Object(params),
                )
                .await;
            match response {
                Ok(_) => {
                    if let Ok(mut state) = state_arc.lock() {
                        state.last_dialog = Some(DialogInfo {
                            dialog_type,
                            message,
                            accepted,
                        });
                    }
                }
                Err(error) => tracing::warn!(%error, "failed to resolve JavaScript dialog"),
            }
        }
    });
    state
}

/// Result of [`BrowserSession::fill_form`].
#[derive(Debug, Clone)]
pub struct FillFormOutcome {
    /// Action IDs that were filled before completion or failure.
    pub completed: Vec<u32>,
    /// `(action_id, error_message)` if the batch stopped early on a field.
    pub failed: Option<(u32, String)>,
    /// Authoritative refreshed semantic tree after the batch.
    pub tree: String,
}

/// Condition for [`BrowserSession::wait_for`].
#[derive(Debug, Clone)]
pub enum WaitCondition {
    /// Wait until visible text appears in the document or an open shadow root.
    Text(String),
    /// Wait until a CSS selector matches in the document or an open shadow root.
    Selector(String),
    /// Wait until `location.href` contains the given substring.
    UrlContains(String),
    /// Wait until no in-flight network requests exist for at least `quiet_ms`.
    NetworkIdle { quiet_ms: u64 },
}

/// Options for [`BrowserSession::screenshot`].
#[derive(Debug, Clone)]
pub struct ScreenshotOptions {
    pub full_page: bool,
    /// Scroll the document to surface lazy-loaded images before capture.
    pub prepare_lazy_content: bool,
    /// Max time to wait for network/images before best-effort capture.
    pub timeout_ms: u64,
    /// Optional element ID to clip the screenshot to its bounding box.
    pub action_id: Option<u32>,
}

impl Default for ScreenshotOptions {
    fn default() -> Self {
        Self {
            full_page: false,
            prepare_lazy_content: true,
            timeout_ms: 8_000,
            action_id: None,
        }
    }
}

/// Viewport dimensions for [`BrowserSession::emulate`].
#[derive(Debug, Clone)]
pub struct ViewportOptions {
    pub width: i32,
    pub height: i32,
    pub device_scale_factor: f64,
    pub mobile: bool,
}

/// Geolocation override for [`BrowserSession::emulate`].
#[derive(Debug, Clone)]
pub struct GeolocationOptions {
    pub latitude: f64,
    pub longitude: f64,
    pub accuracy: f64,
}

/// Device, locale, timezone, and geolocation emulation options.
#[derive(Debug, Clone, Default)]
pub struct EmulationOptions {
    pub viewport: Option<ViewportOptions>,
    pub timezone: Option<String>,
    pub locale: Option<String>,
    pub geolocation: Option<GeolocationOptions>,
}

/// Options for [`BrowserSession::print_to_pdf`].
#[derive(Debug, Clone)]
pub struct PdfOptions {
    pub landscape: bool,
    pub print_background: bool,
    pub scale: f64,
}

impl Default for PdfOptions {
    fn default() -> Self {
        Self {
            landscape: false,
            print_background: true,
            scale: 1.0,
        }
    }
}

/// Default cap on screenshot prepare time (milliseconds).
pub const SCREENSHOT_PREPARE_MS_DEFAULT: u64 = 8_000;

/// Hard ceiling on screenshot prepare time (milliseconds).
///
/// An agent could otherwise ask for a 24-hour wait and pin the (serial) MCP
/// server for the whole time. 60 s is far beyond any real page and still
/// bounded.
pub const SCREENSHOT_PREPARE_MS_MAX: u64 = 60_000;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// One headless browser + one flattened page target.
///
/// Action IDs are ephemeral: every `open`/`click`/`fill`/`scroll` rebuilds
/// the tree and replaces `action_map`. Stale IDs return `ActionNotFound`.
pub struct BrowserSession {
    chrome: Option<ChromeProcess>,
    browser_ws_url: String,
    client: Arc<CdpClient>,
    session_id: String,
    target_id: String,
    resource_policy: ResourcePolicy,
    /// Engine-neutral primitives. Held behind the trait object so a BiDi
    /// driver can replace `CdpDriver` without touching the call sites.
    driver: Box<dyn BrowserDriver>,
    net: NetActivity,
    action_map: ActionMap,
    action_sessions: HashMap<u32, FrameSession>,
    title: String,
    url: String,
    last_tree: String,
    /// Timing breakdown of the most recent `open` operation.
    last_settle: Option<crate::observability::SettleBreakdown>,
    dialog_state: Arc<std::sync::Mutex<DialogState>>,
    downloads: DownloadTracker,
    download_dir: PathBuf,
    popups: PopupTracker,
    console: ConsoleTracker,
    network_log: NetworkLogTracker,
    routes: RouteInterceptor,
    extra_headers: HashMap<String, String>,
    /// Context reported for the current target, including Chrome's default
    /// context. This scopes tab listing and recovery.
    target_context_id: Option<String>,
    /// Isolated context created and owned by this session, if any.
    browser_context_id: Option<String>,
    /// Dropping this wakes and stops all background CDP event tasks for this session.
    _stop_tx: tokio::sync::watch::Sender<()>,
}

impl BrowserSession {
    /// Current action map (`ID → BackendNodeId`).
    pub fn action_map(&self) -> &ActionMap {
        &self.action_map
    }

    /// Drop the action map and the rendered tree.
    ///
    /// Called when a navigation fails: the old map points at backend node
    /// ids from the *previous* page, which no longer exist. Acting on them
    /// makes CDP calls hang until they time out. Clearing them turns a
    /// stale action into an immediate, explicit "action not found".
    #[doc(hidden)]
    pub fn invalidate_tree(&mut self) {
        self.action_map.clear();
        self.action_sessions.clear();
        self.last_tree.clear();
    }

    /// Last rendered tree (from the most recent rebuild).
    pub fn last_tree(&self) -> &str {
        &self.last_tree
    }

    /// Timing breakdown of the most recent `open`, when one has completed.
    pub fn last_settle(&self) -> Option<&crate::observability::SettleBreakdown> {
        self.last_settle.as_ref()
    }

    /// Document title from the last navigation.
    pub fn page_title(&self) -> &str {
        &self.title
    }

    /// Resolved URL of the loaded page.
    pub fn page_url(&self) -> &str {
        &self.url
    }

    /// Flattened CDP `sessionId`.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// CDP `Target` id backing this page.
    pub fn target_id(&self) -> &str {
        &self.target_id
    }

    /// Set a one-action override. It is consumed by the next dialog event.
    pub fn set_dialog_override(&self, accept: bool, prompt_text: Option<String>) {
        if let Ok(mut state) = self.dialog_state.lock() {
            state.override_decision = Some((accept, prompt_text));
            state.last_dialog = None;
        }
    }

    pub fn clear_dialog_override(&self) {
        if let Ok(mut state) = self.dialog_state.lock() {
            state.override_decision = None;
        }
    }

    /// Return and clear the most recently handled dialog, if any.
    pub fn take_dialog_info(&self) -> Option<DialogInfo> {
        self.dialog_state
            .lock()
            .ok()
            .and_then(|mut state| state.last_dialog.take())
    }
}

fn adjacent_history_entry(
    history: &page::GetNavigationHistoryResponse,
    offset: isize,
) -> Option<&page::NavigationEntry> {
    if history.current_index >= history.entries.len() {
        return None;
    }
    let index = history.current_index.checked_add_signed(offset)?;
    history.entries.get(index)
}

/// Metadata emitted by [`BrowserSession::detach`] for `close`/`act` files.
#[derive(Debug, Clone)]
pub struct DetachedSession {
    /// Browser WebSocket URL (`ws://127.0.0.1:...`).
    pub browser_ws_url: String,
    /// Live page `targetId`.
    pub target_id: String,
    /// Chromium OS pid.
    pub pid: u32,
    /// Profile dir (owned by the session file; removed by `close`).
    pub user_data_dir: PathBuf,
    /// Resource policy at launch time.
    pub resource_policy: ResourcePolicy,
}

/// Cheap content fingerprint of a compressed tree: roles, names and the
/// action-id -> backend-id mapping. Detects semantic changes (role
/// upgrades, text changes) that a node count would miss.
fn tree_fingerprint(nodes: &[crate::tree::SemanticNode], map: &crate::tree::ActionMap) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for n in nodes {
        n.role.hash(&mut h);
        n.name.hash(&mut h);
        n.action_id.hash(&mut h);
    }
    // Action map order-independent hash.
    let mut ids: Vec<u32> = map.keys().copied().collect();
    ids.sort_unstable();
    for id in ids {
        id.hash(&mut h);
        map.get(&id).hash(&mut h);
    }
    h.finish()
}

fn build_action_sessions(
    nodes: &[crate::tree::SemanticNode],
    node_sessions: &HashMap<String, FrameSession>,
) -> HashMap<u32, FrameSession> {
    let mut out = HashMap::new();
    if node_sessions.is_empty() {
        return out;
    }
    for n in nodes {
        if let Some(id) = n.action_id {
            if let Some(fs) = node_sessions.get(&n.node_id) {
                out.insert(id, fs.clone());
            }
        }
    }
    out
}

// The SHA-256 schedule uses a fixed 64-word block. Initialization writes
// at most 16 words, each expansion index is in 16..64, and both round arrays
// have exactly 64 entries.
#[expect(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    reason = "fixed SHA-256 schedule bounds"
)]
fn sha256_hex(input: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut data = input.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    data.push(0x80);
    while data.len() % 64 != 56 {
        data.push(0);
    }
    data.extend_from_slice(&bit_len.to_be_bytes());
    let mut h = [
        0x6a09e667u32,
        0xbb67ae85,
        0x3c6ef372,
        0xa54ff53a,
        0x510e527f,
        0x9b05688c,
        0x1f83d9ab,
        0x5be0cd19,
    ];
    for chunk in data.chunks(64) {
        let mut w = [0u32; 64];
        let (words, _) = chunk.as_chunks::<4>();
        for (i, bytes) in words.iter().take(16).enumerate() {
            w[i] = u32::from_be_bytes(*bytes);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, value) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *slot = slot.wrapping_add(value);
        }
    }
    h.iter().map(|word| format!("{word:08x}")).collect()
}

#[cfg(test)]
#[path = "session/tests.rs"]
mod tests;
