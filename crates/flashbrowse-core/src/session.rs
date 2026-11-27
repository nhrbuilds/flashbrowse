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
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

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

/// Tracks in-flight network requests so the settle can wait for the page to
/// actually finish loading (an SPA can be quiet in the DOM while its JS
/// bundle is still downloading).
#[derive(Debug, Clone, Default)]
struct NetActivity {
    inflight: Arc<AtomicUsize>,
    last_activity_ms: Arc<AtomicU64>,
    /// Timestamp (ms) when the current batch of requests started - tells a
    /// slow API call (must wait) from a hung socket (must not stall).
    started_at_ms: Arc<AtomicU64>,
}

/// A request in flight for longer than this is considered hung (e.g. a
/// cancelled favicon request that never reports `loadingFinished`).
const HUNG_REQUEST_MS: u64 = 1000;

impl NetActivity {
    fn new() -> Self {
        Self {
            inflight: Arc::new(AtomicUsize::new(0)),
            last_activity_ms: Arc::new(AtomicU64::new(0)),
            started_at_ms: Arc::new(AtomicU64::new(0)),
        }
    }

    fn touch(&self) {
        self.last_activity_ms.store(now_ms(), Ordering::Relaxed);
    }

    fn reset(&self) {
        self.inflight.store(0, Ordering::Relaxed);
        self.touch();
    }

    fn start(&self) {
        if self.inflight.fetch_add(1, Ordering::Relaxed) == 0 {
            self.started_at_ms.store(now_ms(), Ordering::Relaxed);
        }
        self.touch();
    }

    fn finish(&self) {
        let _ = self
            .inflight
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(1))
            });
        self.touch();
    }

    /// True when the network is effectively idle: nothing in flight (or only
    /// hung requests) and nothing happened for `quiet_ms`.
    fn idle(&self, quiet_ms: u64) -> bool {
        if now_ms().saturating_sub(self.last_activity_ms.load(Ordering::Relaxed)) < quiet_ms {
            return false;
        }
        let inflight = self.inflight.load(Ordering::Relaxed);
        if inflight == 0 {
            return true;
        }
        now_ms().saturating_sub(self.started_at_ms.load(Ordering::Relaxed)) >= HUNG_REQUEST_MS
    }

    /// Subscribe to the network events that drive the tracker (scoped to `session_id`).
    fn attach(
        &self,
        client: &Arc<CdpClient>,
        session_id: &str,
        stop: tokio::sync::watch::Receiver<()>,
    ) {
        let events = client.events();
        let this = self.clone();
        let sid = session_id.to_string();
        let mut rx = events.subscribe_envelopes("Network.requestWillBeSent");
        let mut stop_start = stop.clone();
        spawn_supervised(client, async move {
            while let Some(env) = recv_until_stop(&mut rx, &mut stop_start).await {
                if env.session_id.as_deref() != Some(sid.as_str()) {
                    continue;
                }
                let url = env
                    .params
                    .get("request")
                    .and_then(|r| r.get("url"))
                    .and_then(|u| u.as_str())
                    .unwrap_or("");
                // A favicon request never completes in headless Chrome and
                // would block the settle forever - it cannot change the AX
                // tree, so it is ignored.
                if url.contains("/favicon.ico") {
                    tracing::debug!(url, "ignoring favicon request for network-idle tracking");
                    continue;
                }
                this.start();
            }
        });
        let this = self.clone();
        let sid = session_id.to_string();
        let mut rx = events.subscribe_envelopes("Network.loadingFinished");
        let mut stop_done = stop.clone();
        spawn_supervised(client, async move {
            while let Some(env) = recv_until_stop(&mut rx, &mut stop_done).await {
                if env.session_id.as_deref() == Some(sid.as_str()) {
                    this.finish();
                }
            }
        });
        let this = self.clone();
        let sid = session_id.to_string();
        let mut rx = events.subscribe_envelopes("Network.loadingFailed");
        let mut stop_fail = stop;
        spawn_supervised(client, async move {
            while let Some(env) = recv_until_stop(&mut rx, &mut stop_fail).await {
                if env.session_id.as_deref() == Some(sid.as_str()) {
                    this.finish();
                }
            }
        });
    }
}

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

/// Legacy settle result kept while the candidate is measured in shadow mode.
struct SettleSnapshot {
    nodes: Vec<crate::tree::SemanticNode>,
    map: ActionMap,
    action_sessions: HashMap<u32, FrameSession>,
    raw_nodes: Vec<flashbrowse_cdp::protocol::accessibility::AXNode>,
}

impl BrowserSession {
    /// Launch Chromium, attach, apply stealth + network filtering.
    pub async fn launch(config: SessionConfig) -> Result<Self, CoreError> {
        flashbrowse_cdp::runtime::run(Self::launch_inner(config)).await?
    }

    async fn launch_inner(config: SessionConfig) -> Result<Self, CoreError> {
        config.validate_engine()?;
        if let Some(endpoint) = config.cdp_url.as_deref() {
            let ws_url =
                flashbrowse_cdp::protocol::browser::resolve_websocket_url(endpoint).await?;
            return Self::attach(ws_url, None, &config).await;
        }
        let (chrome, client) = flashbrowse_cdp::launcher::launch_connected_chrome(LaunchOptions {
            chrome_path: config.chrome_path.clone(),
            extra_args: config.extra_args.clone(),
            no_sandbox: config.no_sandbox,
            user_data_dir: config.user_data_dir.clone(),
            window_size: (1280, 800),
            persistent: config.persistent,
        })
        .await?;
        let client = Arc::new(client);
        target::set_discover_targets(&client, true).await?;
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(());
        let popups = install_popup_tracker(&client, stop_rx.clone());
        let t = target::create_target(&client, "about:blank").await?;
        let target_context_id = target::get_targets(&client)
            .await?
            .target_infos
            .into_iter()
            .find(|target| target.target_id == t.target_id)
            .ok_or_else(|| {
                CoreError::NavigationFailed(format!(
                    "new browser tab is no longer available: {}",
                    t.target_id
                ))
            })?
            .browser_context_id;
        let s = target::attach_to_target(&client, &t.target_id).await?;
        let session_id = s.session_id.clone();
        let dialog_state =
            install_dialog_handler(&client, &session_id, config.dialog_policy, stop_rx.clone());

        // Domains must be enabled before navigate so events + AXTree work.
        // Parallelized (was 3 sequential round-trips).
        let (page_r, net_r, rt_r, acc_r) = tokio::join!(
            page::enable(&client, &session_id),
            network::enable(&client, &session_id),
            runtime::enable(&client, &session_id),
            client.send(
                Some(&session_id),
                "Accessibility.enable",
                serde_json::json!({})
            )
        );
        page_r?;
        net_r?;
        rt_r?;
        let download_dir = download_directory(&config);
        std::fs::create_dir_all(&download_dir).map_err(|error| {
            CoreError::NavigationFailed(format!("cannot create download directory: {error}"))
        })?;
        let downloads = install_download_tracker(&client, stop_rx.clone());
        flashbrowse_cdp::protocol::browser::set_download_behavior(
            &client,
            &download_dir.to_string_lossy(),
        )
        .await?;
        page::set_download_behavior(&client, &session_id, &download_dir.to_string_lossy()).await?;
        let _: Value = acc_r?;

        let stealth_js = stealth_script(config.stealth_profile);
        let (s_r, b_r, u_r) = tokio::join!(
            page::add_script_on_new_document(&client, &session_id, &stealth_js),
            network::set_blocked_urls(
                &client,
                &session_id,
                blocked_patterns(config.resource_policy),
            ),
            apply_stealth_profile(&client, &session_id, config.stealth_profile),
        );
        s_r?;
        b_r?;
        u_r?;
        if settle_shadow_enabled() {
            page::add_script_on_new_document(&client, &session_id, SETTLE_DIAGNOSTICS_SCRIPT)
                .await?;
        }

        let browser_ws_url = chrome.browser_ws_url().to_string();
        let net = NetActivity::new();
        net.attach(&client, &session_id, stop_rx.clone());
        let console = ConsoleTracker::install(&client, &session_id, stop_rx.clone());
        let network_log = NetworkLogTracker::install(&client, &session_id, stop_rx.clone());
        let routes = RouteInterceptor::install(&client, &session_id, stop_rx);
        let driver: Box<dyn BrowserDriver> =
            Box::new(CdpDriver::new(client.clone(), session_id.clone()));
        Ok(Self {
            chrome: Some(chrome),
            browser_ws_url,
            client,
            session_id,
            target_id: t.target_id,
            resource_policy: config.resource_policy,
            driver,
            net,
            action_map: ActionMap::new(),
            action_sessions: HashMap::new(),
            title: String::new(),
            url: String::new(),
            last_tree: String::new(),
            last_settle: None,
            dialog_state,
            downloads,
            download_dir,
            popups,
            console,
            network_log,
            routes,
            extra_headers: HashMap::new(),
            target_context_id,
            browser_context_id: None,
            _stop_tx: stop_tx,
        })
    }

    /// Attach to an already-running Chromium (session-file persistence).
    ///
    /// If `target_id` is `Some`, re-attaches to that live target in flatten
    /// mode. If `None`, creates a fresh target.
    ///
    /// NOTE: `Page.addScriptToEvaluateOnNewDocument` and
    /// `Network.setBlockedURLs` are scoped to the CDP *session*, so they are
    /// re-applied on every attach (a new `sessionId` is minted each time).
    pub async fn attach(
        ws_url: String,
        target_id: Option<String>,
        config: &SessionConfig,
    ) -> Result<Self, CoreError> {
        flashbrowse_cdp::runtime::run(Self::attach_inner(ws_url, target_id, config)).await?
    }

    async fn attach_inner(
        ws_url: String,
        target_id: Option<String>,
        config: &SessionConfig,
    ) -> Result<Self, CoreError> {
        config.validate_engine()?;
        let client = Arc::new(CdpClient::connect(&ws_url).await?);
        Self::attach_with_client(client, ws_url, target_id, config).await
    }

    async fn attach_with_client(
        client: Arc<CdpClient>,
        ws_url: String,
        target_id: Option<String>,
        config: &SessionConfig,
    ) -> Result<Self, CoreError> {
        config.validate_engine()?;
        target::set_discover_targets(&client, true).await?;
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(());
        let popups = install_popup_tracker(&client, stop_rx.clone());
        let (tid, target_context_id) = match target_id {
            Some(tid) => {
                let targets = target::get_targets(&client).await?;
                let context_id = targets
                    .target_infos
                    .iter()
                    .find(|target| target.target_id == tid)
                    .ok_or_else(|| {
                        CoreError::NavigationFailed(format!(
                            "page target is no longer available: {tid}"
                        ))
                    })?
                    .browser_context_id
                    .clone();
                (tid, context_id)
            }
            None => {
                let target_id = target::create_target(&client, "about:blank")
                    .await?
                    .target_id;
                let context_id = target::get_targets(&client)
                    .await?
                    .target_infos
                    .into_iter()
                    .find(|target| target.target_id == target_id)
                    .ok_or_else(|| {
                        CoreError::NavigationFailed(format!(
                            "new browser tab is no longer available: {target_id}"
                        ))
                    })?
                    .browser_context_id;
                (target_id, context_id)
            }
        };
        let s = target::attach_to_target(&client, &tid).await?;
        let session_id = s.session_id.clone();
        let dialog_state =
            install_dialog_handler(&client, &session_id, config.dialog_policy, stop_rx.clone());

        // Domains (parallel - 1 round-trip instead of 3).
        let (p_r, n_r, rt_r, a_r) = tokio::join!(
            page::enable(&client, &session_id),
            network::enable(&client, &session_id),
            runtime::enable(&client, &session_id),
            client.send(
                Some(&session_id),
                "Accessibility.enable",
                serde_json::json!({})
            )
        );
        p_r?;
        n_r?;
        rt_r?;
        let download_dir = download_directory(config);
        std::fs::create_dir_all(&download_dir).map_err(|error| {
            CoreError::NavigationFailed(format!("cannot create download directory: {error}"))
        })?;
        let downloads = install_download_tracker(&client, stop_rx.clone());
        flashbrowse_cdp::protocol::browser::set_download_behavior(
            &client,
            &download_dir.to_string_lossy(),
        )
        .await?;
        page::set_download_behavior(&client, &session_id, &download_dir.to_string_lossy()).await?;
        let _: Value = a_r?;

        // Per-session settings: always re-apply (parallel).
        let stealth_js = stealth_script(config.stealth_profile);
        let (s_r, b_r, u_r) = tokio::join!(
            page::add_script_on_new_document(&client, &session_id, &stealth_js),
            network::set_blocked_urls(
                &client,
                &session_id,
                blocked_patterns(config.resource_policy),
            ),
            apply_stealth_profile(&client, &session_id, config.stealth_profile),
        );
        s_r?;
        b_r?;
        u_r?;
        if settle_shadow_enabled() {
            page::add_script_on_new_document(&client, &session_id, SETTLE_DIAGNOSTICS_SCRIPT)
                .await?;
        }

        let net = NetActivity::new();
        net.attach(&client, &session_id, stop_rx.clone());
        let console = ConsoleTracker::install(&client, &session_id, stop_rx.clone());
        let network_log = NetworkLogTracker::install(&client, &session_id, stop_rx.clone());
        let routes = RouteInterceptor::install(&client, &session_id, stop_rx);
        let driver: Box<dyn BrowserDriver> =
            Box::new(CdpDriver::new(client.clone(), session_id.clone()));
        let mut sess = Self {
            chrome: None,
            browser_ws_url: ws_url,
            client,
            session_id,
            target_id: tid,
            resource_policy: config.resource_policy,
            driver,
            net,
            action_map: ActionMap::new(),
            action_sessions: HashMap::new(),
            title: String::new(),
            url: String::new(),
            last_tree: String::new(),
            last_settle: None,
            dialog_state,
            downloads,
            download_dir,
            popups,
            console,
            network_log,
            routes,
            extra_headers: HashMap::new(),
            target_context_id,
            browser_context_id: None,
            _stop_tx: stop_tx,
        };
        // Wait for the popup document to finish loading and remain quiet long
        // enough for post-load hydration before creating its first action map.
        sess.wait_for_popup_settle().await?;
        // Seed title/url + action_map so `act` never uses a blank map.
        // Propagate failures (H6): a blank map must not masquerade as
        // "ActionNotFound" later.
        sess.refresh_title_url().await;
        sess.rebuild_tree().await?;
        Ok(sess)
    }

    async fn wait_for_popup_settle(&self) -> Result<(), CoreError> {
        flashbrowse_cdp::runtime::run(self.wait_for_popup_settle_inner()).await?;
        Ok(())
    }

    async fn wait_for_popup_settle_inner(&self) {
        let deadline = {
            let now = std::time::Instant::now();
            now.checked_add(Duration::from_secs(5)).unwrap_or(now)
        };
        let mut previous = None;
        let mut stable_since = std::time::Instant::now();
        loop {
            let state = self
                .driver
                .evaluate(SETTLE_DOM_STATS_JS)
                .await
                .ok()
                .and_then(|v| v.as_str().map(str::to_string));
            let now = std::time::Instant::now();
            if let Some(state) = state {
                let mut parts = state.split('|');
                let complete = parts.next() == Some("complete");
                let dom_signature = (
                    parts.next().and_then(|v| v.parse::<usize>().ok()),
                    parts.next().and_then(|v| v.parse::<usize>().ok()),
                );
                if !complete {
                    previous = None;
                    stable_since = now;
                } else if previous != Some(dom_signature) {
                    previous = Some(dom_signature);
                    stable_since = now;
                } else if now.duration_since(stable_since) >= Duration::from_millis(1200) {
                    break;
                }
            } else {
                previous = None;
                stable_since = now;
            }
            if now >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Browser-level WebSocket URL (for session files).
    pub fn browser_ws_url(&self) -> &str {
        &self.browser_ws_url
    }

    /// Owned process id (ephemeral sessions only; `None` after attach).
    pub fn pid(&self) -> Option<u32> {
        self.chrome.as_ref().and_then(|c| c.pid())
    }

    /// Owned profile dir (ephemeral sessions only).
    pub fn user_data_dir(&self) -> Option<&PathBuf> {
        self.chrome.as_ref().map(|c| c.user_data_dir())
    }

    /// Detach the browser process so it survives CLI exit and return
    /// metadata for the session file. The CdpClient socket is closed
    /// (pending calls drained); the target page persists in Chromium.
    pub fn detach(mut self) -> DetachedSession {
        let metadata = self.detached_metadata();
        if let Some(proc) = self.chrome.take() {
            if let Some((child, _dir)) = proc.detach() {
                // Forget Child so kill_on_drop does not kill the daemon browser.
                std::mem::forget(child);
            }
        }
        self.client.shutdown();
        metadata
    }

    /// Return the information needed to persist a session while retaining ownership.
    pub fn detached_metadata(&self) -> DetachedSession {
        DetachedSession {
            browser_ws_url: self.browser_ws_url.clone(),
            target_id: self.target_id.clone(),
            pid: self.pid().unwrap_or(0),
            user_data_dir: self.user_data_dir().cloned().unwrap_or_default(),
            resource_policy: self.resource_policy,
        }
    }

    /// Navigate and return the compressed, numbered action tree.
    ///
    /// Return a tree only after the current navigation has a stable,
    /// non-empty accessibility snapshot. The settle loop checks document
    /// readiness, DOM quiet time, and a semantic AX fingerprint. If it cannot
    /// verify stability within 15 seconds, it returns an error and invalidates
    /// the action map instead of exposing the latest partial snapshot.
    pub async fn open(&mut self, url: &str) -> Result<String, CoreError> {
        self.open_with_raw_snapshot(url)
            .await
            .map(|(rendered, _)| rendered)
    }

    /// Open a URL and return the raw AX snapshot used for the authoritative
    /// rendered tree. This is intended for fidelity diagnostics that must
    /// compare both representations from the same browser observation.
    pub async fn open_with_raw_snapshot(
        &mut self,
        url: &str,
    ) -> Result<
        (
            String,
            Vec<flashbrowse_cdp::protocol::accessibility::AXNode>,
        ),
        CoreError,
    > {
        flashbrowse_cdp::runtime::run(self.open_with_raw_snapshot_inner(url)).await?
    }

    async fn open_with_raw_snapshot_inner(
        &mut self,
        url: &str,
    ) -> Result<
        (
            String,
            Vec<flashbrowse_cdp::protocol::accessibility::AXNode>,
        ),
        CoreError,
    > {
        let t_open = std::time::Instant::now();
        let shadow_enabled = settle_shadow_enabled();
        // Subscribe before navigating: a broadcast channel only delivers
        // events emitted after `subscribe`, so this cannot miss this
        // navigation's AX completion. `Accessibility.enable` is already on
        // (see `launch`/`attach`), so the event stream exists at zero cost.
        let mut ax_complete_rx = self
            .client
            .events()
            .subscribe_envelopes("Accessibility.loadComplete");
        // `Page.navigate` returns before the new document commits. Until the
        // commit event for THIS navigation arrives, the DOM, the readyState
        // and the AX tree all still belong to the previous page, so observing
        // them would look like a page that changed the instant it loaded.
        let mut lifecycle_rx = self.client.events().subscribe("Page.lifecycleEvent");
        let t_nav = std::time::Instant::now();
        let nav = page::navigate(&self.client, &self.session_id, url).await?;
        let nav_ms = t_nav.elapsed().as_millis() as u64;
        let nav_loader = nav.loader_id.clone();
        // NOTE: we deliberately do NOT wait for the `load` event - it waits
        // for every subresource (8+ s on heavy pages) while the AX tree is
        // usually final much earlier. Readiness is decided by the settle loop
        // below (document parsed + AX tree complete vs DOM + tree quiet).

        self.url = url.to_string();
        let client_c = self.client.clone();
        let sid = self.session_id.clone();
        let t_titles = std::time::Instant::now();
        let (title_r, url_r) = tokio::join!(
            runtime::evaluate(&client_c, &sid, "document.title"),
            runtime::evaluate(&client_c, &sid, "location.href")
        );
        if let Ok(r) = title_r {
            if let Some(Value::String(s)) = r.result.value {
                if !s.is_empty() {
                    self.title = s;
                }
            }
        }
        if let Ok(r) = url_r {
            if let Some(Value::String(s)) = r.result.value {
                if !s.is_empty() {
                    self.url = s;
                }
            }
        }
        let mut title_url_ms = t_titles.elapsed().as_millis() as u64;
        // Chrome error pages (network lost, DNS failure, ...) look like a
        // tiny page. Report them instead of returning a truncated tree.
        if self.url.starts_with("chrome-error://") {
            self.invalidate_tree();
            return Err(CoreError::NavigationFailed(self.url.clone()));
        }
        // The AX tree is fetched inside the settle loop (the tree right
        // after `navigate` belongs to the previous page - fetching it twice
        // wasted ~200 KB of JSON per open).
        let mut raw_ax_nodes = 0usize;
        let mut nodes: Vec<crate::tree::SemanticNode> = Vec::new();
        let mut map = ActionMap::new();
        let mut action_sessions: HashMap<u32, FrameSession> = HashMap::new();
        let mut fingerprint = 0u64;
        // Separate stage timers are reported through
        // `SettleBreakdown` so a speedup claim can be checked against evidence.
        let mut poll_ms = 0u64;
        let mut sleep_ms = 0u64;
        let mut ax_fetch_ms = 0u64;
        let mut compress_ms = 0u64;
        let mut iterations = 0u64;
        let mut ax_fetches = 0u64;
        let mut exit_reason = "deadline";
        let mut last_raw_nodes = Vec::new();
        let mut mutation_count = None;
        let mut mutation_last_ms = None;
        let mut mutation_quiet_ms = None;
        let mut mutation_would_delay_candidate = None;
        let mut candidate_accept_ms = None;
        let mut legacy_snapshot: Option<SettleSnapshot> = None;
        let mut candidate_nodes: Option<Vec<crate::tree::SemanticNode>> = None;

        // Settle loop.
        //
        // Two-level strategy (fast AND complete):
        //  1. Cheap poll every 100 ms - one `Runtime.evaluate` returning
        //     `readyState|domCount|textLength`. If the DOM is still moving,
        //     the AX tree cannot be final.
        //  2. Once the cheap metrics have been quiet for `QUIET_MS`, fetch
        //     the AX tree once more and compare the content fingerprint.
        //     Only an identical fingerprint ends the wait.
        //
        // Why not just poll the AX tree? Each `getFullAXTree` moves ~200 KB
        // of JSON; three polls cost more than the whole cheap-poll budget.
        //
        // Why the fingerprint and not the node count? Chromium upgrades table
        // semantics after layout (LayoutTableCell -> Link) without changing
        // the node count.
        // Adaptive quiet window: a page that never changed (server-rendered)
        // only needs a short window; a page that is still moving gets a long
        // one. The AX fingerprint check is the final gate in both cases.
        //
        // The small-tree window must cover common hydration delays: a page
        // that fills in content from a `setTimeout(..., 1000)` fires *after*
        // `readyState == "complete"`, and exiting too early truncates that
        // content. 1200 ms of stillness is the compromise between catching
        // late hydration and not doubling the cost of every tiny page.
        const QUIET_MS_STATIC: u64 = 250;
        const QUIET_MS_DYNAMIC: u64 = 600;
        const QUIET_MS_SMALL_TREE: u64 = 1200;
        // The deadline covers the whole settle (page load + hydration + AX
        // population). A slow network can need several seconds just to
        // deliver the HTML, so the budget must not be shorter than the CDP
        // call timeout we already allow elsewhere (15s).
        let deadline = {
            let now = tokio::time::Instant::now();
            now.checked_add(Duration::from_millis(15000)).unwrap_or(now)
        };
        // Poll more often without shortening the evidence window. Every
        // acceptance still requires a stable AX fingerprint and full quiet
        // interval.
        let tick_ms: u64 = 100;
        let prefetch_lead_ms: u64 = 150;
        let mut last_change = tokio::time::Instant::now();
        let mut prev_dom = (0u64, 0u64);
        let mut legacy_saw_change = false;
        let mut candidate_saw_change = false;
        let mut ax_complete = false;
        // Same-document navigations carry no `loaderId`; the current document
        // is the right one from the start.
        let mut nav_committed = nav_loader.is_none();
        // Defensive fallback: a page whose commit event never arrives must not
        // hang the settle. After this, the loop trusts its own readyState and
        // DOM checks as before.
        let commit_fallback = {
            let now = tokio::time::Instant::now();
            now.checked_add(Duration::from_millis(3000)).unwrap_or(now)
        };
        loop {
            iterations = iterations.saturating_add(1);
            // Deadline check FIRST: every `continue` path below must be
            // able to exit the loop (a page that keeps changing would
            // otherwise spin forever).
            if tokio::time::Instant::now() >= deadline {
                tracing::debug!(
                    raw_ax_nodes,
                    semantic = nodes.len(),
                    "settle deadline reached before the page stabilized"
                );
                break;
            }
            // Drain AX load-complete notifications for this navigation.
            // Chrome emits `Accessibility.loadComplete` once the tree for the
            // loaded document is fully populated, which is what separates a
            // legitimately sparse tree from one that is merely early.
            while let Ok(env) = ax_complete_rx.try_recv() {
                if env.session_id.as_deref() == Some(self.session_id.as_str()) {
                    ax_complete = true;
                }
            }
            // Wait for this navigation's document to commit before observing
            // anything. Any event with the matching loaderId means the new
            // document is now the one being polled.
            while let Ok(ev) = lifecycle_rx.try_recv() {
                let matches = match nav_loader.as_deref() {
                    Some(loader) => ev.get("loaderId").and_then(|v| v.as_str()) == Some(loader),
                    None => true,
                };
                if matches && !nav_committed {
                    nav_committed = true;
                    // The quiet window starts at commit: time spent before the
                    // new document existed must not count as stillness.
                    last_change = tokio::time::Instant::now();
                }
            }
            if !nav_committed {
                if tokio::time::Instant::now() >= commit_fallback {
                    tracing::debug!("no commit lifecycle event; proceeding on own checks");
                    nav_committed = true;
                    last_change = tokio::time::Instant::now();
                } else {
                    if iterations > 1 {
                        let t_sleep = std::time::Instant::now();
                        tokio::time::sleep(Duration::from_millis(tick_ms)).await;
                        sleep_ms = sleep_ms.saturating_add(t_sleep.elapsed().as_millis() as u64);
                    }
                    continue;
                }
            }
            // Give the new document (and Chromium's accessibility tree) a
            // tick before the first observation. Fetching immediately after
            // commit samples a pre-population tree, which then reads as a
            // change at the end of the quiet window and costs a second full
            // window. The tick is what keeps the settle at one window.
            let t_sleep = std::time::Instant::now();
            tokio::time::sleep(Duration::from_millis(tick_ms)).await;
            sleep_ms = sleep_ms.saturating_add(t_sleep.elapsed().as_millis() as u64);

            // (1) cheap change detection: is the document still moving?
            let mut ready_state = String::from("loading");
            let mut dom_count = 0u64;
            let mut text_len = 0u64;
            let mut provably_static = false;
            let t_poll = std::time::Instant::now();
            let needs_static_classification = raw_ax_nodes < 100 && !legacy_saw_change;
            let cheap_expression = if shadow_enabled {
                format!(
                    "{SETTLE_DOM_STATS_JS} + '|' + (document.scripts.length === 0 && document.querySelectorAll('iframe').length === 0 && !document.querySelector('meta[http-equiv=refresh]')) + '|' + (window.__flashbrowseMutationState?.count ?? 0) + '|' + (window.__flashbrowseMutationState?.last_ms ?? 0) + '|' + (window.__flashbrowseMutationState ? performance.now() - window.__flashbrowseMutationState.last_ms : 0)"
                )
            } else if needs_static_classification {
                format!(
                    "{SETTLE_DOM_STATS_JS} + '|' + (document.scripts.length === 0 && document.querySelectorAll('iframe').length === 0 && !document.querySelector('meta[http-equiv=refresh]'))"
                )
            } else {
                format!("{SETTLE_DOM_STATS_JS} + '|false'")
            };
            if let Ok(cheap) =
                runtime::evaluate(&self.client, &self.session_id, &cheap_expression).await
            {
                if let Some(Value::String(s)) = cheap.result.value.as_ref() {
                    let mut it = s.split('|');
                    ready_state = it.next().unwrap_or("loading").to_string();
                    dom_count = it.next().unwrap_or("0").parse().unwrap_or(0);
                    text_len = it.next().unwrap_or("0").parse().unwrap_or(0);
                    provably_static = it.next().unwrap_or("false") == "true";
                    if shadow_enabled {
                        mutation_count = it.next().and_then(|v| v.parse().ok());
                        mutation_last_ms = it
                            .next()
                            .and_then(|v| v.parse::<f64>().ok())
                            .map(|ms| ms.max(0.0) as u64);
                        mutation_quiet_ms = it
                            .next()
                            .and_then(|v| v.parse::<f64>().ok())
                            .map(|ms| ms.max(0.0) as u64);
                    }
                }
            }
            poll_ms = poll_ms.saturating_add(t_poll.elapsed().as_millis() as u64);
            // "loading" = the document is still streaming; "interactive" = the
            // DOM is parsed but subresources (JS bundles!) are still coming;
            // "complete" = the browser considers the page fully loaded.
            let page_complete = ready_state.eq_ignore_ascii_case("complete");
            let parsed = !ready_state.eq_ignore_ascii_case("loading");
            if !parsed {
                // Still parsing - the tree cannot be final.
                last_change = tokio::time::Instant::now();
                continue;
            }
            let cheap_now = (dom_count, text_len);
            if prev_dom == (0, 0) {
                prev_dom = cheap_now; // baseline, not a change
            } else if cheap_now != prev_dom {
                prev_dom = cheap_now;
                last_change = tokio::time::Instant::now();
                legacy_saw_change = true;
                candidate_saw_change = true;
                continue;
            }
            // Quiet window:
            //   - the DOM was moving          -> wait longer (600 ms)
            //   - the tree is still small      -> wait longer (800 ms): a
            //     small tree is either a tiny page or a page that has not
            //     hydrated yet (nhrdev.com truncated to 140 tokens with a
            //     250 ms window);
            //   - a big, static tree           -> settle fast (250 ms).
            let legacy_quiet_ms = if legacy_saw_change {
                QUIET_MS_DYNAMIC
            } else if raw_ax_nodes < 100 && !provably_static {
                QUIET_MS_SMALL_TREE
            } else {
                QUIET_MS_STATIC
            };
            let candidate_quiet_ms = if shadow_enabled && candidate_saw_change {
                QUIET_MS_DYNAMIC
            } else if raw_ax_nodes < 100 && !provably_static {
                QUIET_MS_SMALL_TREE
            } else {
                QUIET_MS_STATIC
            };
            let legacy_quiet_enough =
                last_change.elapsed() >= Duration::from_millis(legacy_quiet_ms);
            let candidate_quiet_enough =
                last_change.elapsed() >= Duration::from_millis(candidate_quiet_ms);

            // (2) the AX tree must not be obviously empty while the DOM
            // already has content.
            //
            // Chromium populates the accessibility tree asynchronously
            // (~400 ms after the DOM is final, measured on Hacker News), so a
            // DOM-stability window alone truncates the page (140 tokens
            // instead of 1770). A pre-hydration tree is tiny (2-21 nodes)
            // while the DOM already holds hundreds of elements.
            // A tree with a handful of nodes cannot be a real page (unless
            // the page itself is tiny) - treat it as "not populated yet".
            // `raw_ax_nodes == 0` means "not fetched yet" -> fetch now.
            let tree_short = raw_ax_nodes == 0 || (raw_ax_nodes < 20 && dom_count >= 20);
            // A short tree is final when Chrome itself reports the document's
            // accessibility tree complete. Without this escape a page whose
            // DOM is large but whose tree is legitimately tiny (deep wrapper
            // chains that compress to a single text node) can never satisfy
            // `tree_unready` and burns the whole 15 s deadline on every open.
            // The stability gate is unchanged: the samples must still match
            // across the quiet window.
            let sparse_final = tree_short && ax_complete && page_complete;
            // Chromium's completed accessibility tree can legitimately be
            // sparse, so only keep waiting when the candidate lacks both
            // completion signals.
            let candidate_unready = tree_short && !sparse_final;
            let legacy_unready =
                raw_ax_nodes == 0 || (raw_ax_nodes < 20 && dom_count >= 20 && !sparse_final);

            // Sample early enough that the confirming fetch lands as soon as
            // the quiet window elapses. The two samples still bracket the
            // window (they are ~PREFETCH_LEAD_MS apart), so the stability
            // evidence is at least as strong as before, but acceptance no
            // longer pays an extra poll interval plus a sleep.
            let elapsed_ms = last_change.elapsed().as_millis() as u64;
            let prefetch_due = prefetch_lead_ms > 0
                && !candidate_quiet_enough
                && elapsed_ms.saturating_add(prefetch_lead_ms) >= candidate_quiet_ms;

            // Fetch when the tree looks unready (it must be refreshed while
            // we wait for it), when the DOM settled, or when the confirming
            // sample is due.
            if (legacy_snapshot.is_some() || (!legacy_unready && !legacy_quiet_enough))
                && (candidate_accept_ms.is_some()
                    || (!candidate_unready && !candidate_quiet_enough && !prefetch_due))
            {
                continue;
            }

            // (3) verify the AX tree itself
            let t_fetch = std::time::Instant::now();
            let (fresh, node_sessions) = self.driver.get_full_ax_tree_with_sessions().await?;
            ax_fetch_ms = ax_fetch_ms.saturating_add(t_fetch.elapsed().as_millis() as u64);
            ax_fetches = ax_fetches.saturating_add(1);
            let t_compress = std::time::Instant::now();
            let raw_ax_nodes_before = raw_ax_nodes;
            raw_ax_nodes = fresh.len();
            let (n2, m2) = compress(&fresh);
            let fp2 = tree_fingerprint(&n2, &m2);
            let ratio_ok = dom_count < 100 || raw_ax_nodes as u64 >= dom_count.div_ceil(4);
            let same = raw_ax_nodes_before == raw_ax_nodes && fp2 == fingerprint;
            action_sessions = build_action_sessions(&n2, &node_sessions);
            nodes = n2;
            map = m2;
            fingerprint = fp2;
            compress_ms = compress_ms.saturating_add(t_compress.elapsed().as_millis() as u64);

            let net_idle = self.net.idle(250);
            tracing::debug!(
                dom_count,
                text_len,
                raw_ax_nodes,
                semantic = nodes.len(),
                same,
                legacy_quiet_enough,
                candidate_quiet_enough,
                ax_complete,
                prefetch_due,
                sparse_final,
                net_idle,
                "settle poll"
            );

            // Accept when the tree is stable and the page is done.
            //
            // `readyState == "complete"` is the browser's own verdict that
            // everything (including JS bundles) has loaded - the
            // authoritative, network-speed-independent signal. The network
            // idle check is only a fallback for pages that keep a socket
            // open forever (never "complete").
            //
            // The tree must contain *something* (`nodes.len() >= 1`), not
            // three nodes: a minimal page ("<h1>Hi</h1>") legitimately
            // compresses to one or two nodes, and requiring three made
            // every such page wait out the whole settle deadline.
            let first_fetch = raw_ax_nodes_before == 0;
            let stable = same && ratio_ok && !nodes.is_empty() && (page_complete || net_idle);
            let legacy_stable = stable
                && (!first_fetch || !legacy_saw_change)
                && legacy_quiet_enough
                && !legacy_unready;
            let candidate_stable = stable
                && shadow_enabled
                && (!first_fetch || !candidate_saw_change)
                && candidate_quiet_enough
                && !candidate_unready;
            if legacy_stable && !shadow_enabled {
                last_raw_nodes = fresh;
                exit_reason = "settled";
                break;
            }
            if legacy_stable && legacy_snapshot.is_none() {
                legacy_snapshot = Some(SettleSnapshot {
                    nodes: nodes.clone(),
                    map: map.clone(),
                    action_sessions: action_sessions.clone(),
                    raw_nodes: fresh.clone(),
                });
            }
            if candidate_stable && candidate_accept_ms.is_none() {
                candidate_nodes = Some(nodes.clone());
                candidate_accept_ms = Some(t_open.elapsed().as_millis() as u64);
                mutation_would_delay_candidate =
                    mutation_quiet_ms.map(|quiet| quiet < candidate_quiet_ms);
            }
            if !same {
                // The tree still moves - restart the quiet window.
                last_change = tokio::time::Instant::now();
                // AX changes are evidence of dynamic page content just like
                // DOM text changes. The next confirmation uses the existing
                // dynamic quiet interval instead of treating the page as an
                // untouched small tree and waiting another full 1200 ms.
                if shadow_enabled && raw_ax_nodes_before > 0 {
                    candidate_saw_change = true;
                }
            }
            last_raw_nodes = fresh;
            if shadow_enabled && legacy_snapshot.is_some() && candidate_accept_ms.is_some() {
                exit_reason = "settled";
                break;
            }
        }
        if legacy_snapshot.is_none() && exit_reason == "deadline" {
            self.last_settle = Some(crate::observability::SettleBreakdown {
                nav_ms,
                poll_ms,
                sleep_ms,
                ax_fetch_ms,
                compress_ms,
                title_url_ms,
                render_ms: 0,
                iterations,
                ax_fetches,
                exit_reason: exit_reason.to_string(),
                candidate_accept_ms,
                legacy_tree_sha256: None,
                candidate_tree_sha256: None,
                shadow_diverged: None,
                raw_ax_nodes: raw_ax_nodes as u64,
                semantic_nodes: nodes.len() as u64,
                mutation_count,
                mutation_last_ms,
                mutation_quiet_ms,
                mutation_would_delay_candidate,
            });
            self.invalidate_tree();
            return Err(CoreError::NavigationFailed(format!(
                "page did not reach a stable accessibility tree within 15 seconds: {}",
                self.url
            )));
        }
        let accepted_raw_nodes;
        if let Some(snapshot) = legacy_snapshot {
            nodes = snapshot.nodes;
            map = snapshot.map;
            action_sessions = snapshot.action_sessions;
            accepted_raw_nodes = snapshot.raw_nodes;
        } else {
            accepted_raw_nodes = last_raw_nodes;
        }
        if nodes.is_empty() {
            // The page never rendered (network hiccup, bot wall, JS-only
            // shell). Report it instead of returning an empty tree.
            self.last_settle = Some(crate::observability::SettleBreakdown {
                nav_ms,
                poll_ms,
                sleep_ms,
                ax_fetch_ms,
                compress_ms,
                title_url_ms,
                render_ms: 0,
                iterations,
                ax_fetches,
                exit_reason: "empty".to_string(),
                candidate_accept_ms,
                legacy_tree_sha256: None,
                candidate_tree_sha256: None,
                shadow_diverged: None,
                raw_ax_nodes: raw_ax_nodes as u64,
                semantic_nodes: nodes.len() as u64,
                mutation_count,
                mutation_last_ms,
                mutation_quiet_ms,
                mutation_would_delay_candidate,
            });
            self.invalidate_tree();
            return Err(CoreError::NavigationFailed(format!(
                "page produced no accessibility content: {}",
                self.url
            )));
        }
        // Re-read the title and URL now that the page has settled. The
        // first read happens right after `navigate`, when the new document
        // often has not been parsed yet - `document.title` is then empty
        // and the header line was blank about one page in five.
        let t_titles_final = std::time::Instant::now();
        self.refresh_title_url().await;
        title_url_ms = title_url_ms.saturating_add(t_titles_final.elapsed().as_millis() as u64);
        let t_render = std::time::Instant::now();
        let rendered = render_nested(&self.title, &self.url, &nodes);
        let render_ms = t_render.elapsed().as_millis() as u64;
        let (legacy_tree_sha256, candidate_tree_sha256, shadow_diverged) = if shadow_enabled {
            let legacy_hash = sha256_hex(rendered.as_bytes());
            let candidate_hash = candidate_nodes.as_ref().map(|candidate| {
                sha256_hex(render_nested(&self.title, &self.url, candidate).as_bytes())
            });
            let diverged = candidate_hash
                .as_ref()
                .map(|candidate| candidate != &legacy_hash);
            (Some(legacy_hash), candidate_hash, diverged)
        } else {
            (None, None, None)
        };
        self.last_settle = Some(crate::observability::SettleBreakdown {
            nav_ms,
            poll_ms,
            sleep_ms,
            ax_fetch_ms,
            compress_ms,
            title_url_ms,
            render_ms,
            iterations,
            ax_fetches,
            exit_reason: exit_reason.to_string(),
            candidate_accept_ms,
            legacy_tree_sha256,
            candidate_tree_sha256,
            shadow_diverged,
            raw_ax_nodes: raw_ax_nodes as u64,
            semantic_nodes: nodes.len() as u64,
            mutation_count,
            mutation_last_ms,
            mutation_quiet_ms,
            mutation_would_delay_candidate,
        });
        self.action_map = map;
        self.action_sessions = action_sessions;
        self.last_tree = rendered.clone();
        tracing::debug!(
            ms = t_open.elapsed().as_millis(),
            nodes = nodes.len(),
            settle = ?self.last_settle,
            "open complete"
        );
        Ok((rendered, accepted_raw_nodes))
    }

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

    /// Export Playwright-compatible cookies and per-origin localStorage.
    pub async fn export_storage_state(&self) -> Result<Value, CoreError> {
        let cookies: Value = self
            .client
            .send(
                Some(&self.session_id),
                "Network.getAllCookies",
                serde_json::json!({}),
            )
            .await?;
        let page_state = runtime::evaluate(
            &self.client,
            &self.session_id,
            "JSON.stringify({origin: location.origin, localStorage: Object.entries(localStorage).map(([name, value]) => ({name, value}))})",
        )
        .await?;
        let origin_state = page_state
            .result
            .value
            .as_ref()
            .and_then(Value::as_str)
            .and_then(|encoded| serde_json::from_str::<Value>(encoded).ok())
            .filter(|value| {
                value
                    .get("origin")
                    .and_then(Value::as_str)
                    .is_some_and(|o| o != "null")
            });
        Ok(serde_json::json!({
            "cookies": cookies.get("cookies").cloned().unwrap_or_else(|| Value::Array(Vec::new())),
            "origins": origin_state.into_iter().collect::<Vec<_>>(),
        }))
    }

    /// Apply cookies and arrange localStorage restoration before page scripts,
    /// then reload the current document so the imported state takes effect.
    pub async fn import_storage_state(&mut self, state: &Value) -> Result<String, CoreError> {
        flashbrowse_cdp::transport::validate_json_depth(state)?;
        let cookies = state
            .get("cookies")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                CoreError::NavigationFailed("storage state must contain a cookies array".into())
            })?;
        let origins = state
            .get("origins")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                CoreError::NavigationFailed("storage state must contain an origins array".into())
            })?;
        let _: Value = self
            .client
            .send(
                Some(&self.session_id),
                "Network.setCookies",
                serde_json::json!({"cookies": cookies}),
            )
            .await?;
        let encoded_origins = serde_json::to_string(origins)?;
        let script = format!(
            "(() => {{ const states = {encoded_origins}; const state = states.find(item => item.origin === location.origin); if (state && Array.isArray(state.localStorage)) {{ for (const item of state.localStorage) localStorage.setItem(item.name, item.value); }} }})()"
        );
        page::add_script_on_new_document(&self.client, &self.session_id, &script).await?;
        let current_url = if self.url.is_empty() {
            "about:blank".to_string()
        } else {
            self.url.clone()
        };
        self.open(&current_url).await
    }

    /// Populate an `<input type=file>` with existing files and rebuild its tree.
    pub async fn upload_files(
        &mut self,
        action_id: u32,
        files: &[PathBuf],
    ) -> Result<String, CoreError> {
        if files.is_empty() {
            return Err(CoreError::NavigationFailed(
                "upload requires at least one file".into(),
            ));
        }
        let backend = *self
            .action_map
            .get(&action_id)
            .ok_or(CoreError::ActionNotFound(action_id))?;
        let (dom_sid, _) = self.frame_for_action(action_id);
        let object = dom::resolve_node(&self.client, &dom_sid, backend).await?;
        let object_id = object
            .object
            .object_id
            .ok_or(CoreError::NoBackendId(action_id))?;
        let input_check = runtime::call_function_on(
            &self.client,
            &dom_sid,
            &object_id,
            "function() { return this instanceof HTMLInputElement && this.type === 'file'; }",
            Vec::new(),
        )
        .await?;
        if !input_check
            .result
            .value
            .as_ref()
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Err(CoreError::NavigationFailed(format!(
                "action [{action_id}] is not a file input"
            )));
        }
        let mut canonical_files = Vec::with_capacity(files.len());
        for file in files {
            let canonical = std::fs::canonicalize(file).map_err(|error| {
                CoreError::NavigationFailed(format!(
                    "cannot access upload file {}: {error}",
                    file.display()
                ))
            })?;
            if !canonical.is_file() {
                return Err(CoreError::NavigationFailed(format!(
                    "upload path is not a file: {}",
                    canonical.display()
                )));
            }
            canonical_files.push(canonical.to_string_lossy().into_owned());
        }
        dom::set_file_input_files(&self.client, &dom_sid, backend, canonical_files).await?;
        self.rebuild_tree().await
    }

    pub fn downloads(&self) -> Vec<DownloadInfo> {
        let mut result = self
            .downloads
            .lock()
            .map(|downloads| downloads.values().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        result.sort_by(|left, right| left.guid.cmp(&right.guid));
        result
    }

    pub fn download_dir(&self) -> &std::path::Path {
        &self.download_dir
    }

    /// Create a new isolated session inside an existing `browserContextId` on the same Chrome.
    pub async fn new_in_context(
        client: Arc<CdpClient>,
        browser_ws_url: String,
        browser_context_id: String,
        config: &SessionConfig,
    ) -> Result<Self, CoreError> {
        let t = target::create_target_in_context(&client, "about:blank", Some(&browser_context_id))
            .await?;
        let mut sess =
            Self::attach_with_client(client, browser_ws_url, Some(t.target_id), config).await?;
        sess.browser_context_id = Some(browser_context_id);
        Ok(sess)
    }

    /// Spawn an isolated incognito-like `BrowserSession` sharing this Chrome process.
    pub async fn create_isolated_context(&self, config: &SessionConfig) -> Result<Self, CoreError> {
        let cid = target::create_browser_context(&self.client).await?;
        let mut cleanup = BrowserContextCleanup::new(self.client.clone(), cid.clone());
        match Self::new_in_context(
            self.client.clone(),
            self.browser_ws_url.clone(),
            cid.clone(),
            config,
        )
        .await
        {
            Ok(sess) => {
                cleanup.browser_context_id = None;
                Ok(sess)
            }
            Err(err) => {
                if let Err(cleanup_error) = cleanup.dispose().await {
                    tracing::warn!(%cleanup_error, "isolated browser context cleanup failed after creation error");
                }
                Err(err)
            }
        }
    }

    pub fn current_url(&self) -> &str {
        &self.url
    }

    pub fn browser_context_id(&self) -> Option<&str> {
        self.browser_context_id.as_deref()
    }

    pub async fn list_tabs(&self) -> Result<Vec<target::TargetInfo>, CoreError> {
        let targets = target::get_targets(&self.client).await?;
        Ok(targets
            .target_infos
            .into_iter()
            .filter(|t| t.kind == "page")
            .filter(|t| t.browser_context_id == self.target_context_id)
            .collect())
    }

    /// Reattach to a live page in this session's browser context if the active
    /// page was closed outside flashbrowse. If its context has no pages left,
    /// create a blank page in that same context before reattaching.
    pub async fn recover_closed_active_tab(
        &mut self,
        config: &SessionConfig,
    ) -> Result<Vec<target::TargetInfo>, CoreError> {
        let mut tabs = self.list_tabs().await?;
        if tabs.iter().any(|tab| tab.target_id == self.target_id) {
            return Ok(tabs);
        }

        let target_id = tabs
            .iter()
            .find(|tab| !tab.url.starts_with("chrome://"))
            .or_else(|| tabs.first())
            .map(|tab| tab.target_id.clone());
        let target_id = match target_id {
            Some(target_id) => target_id,
            None => {
                target::create_target_in_context(
                    &self.client,
                    "about:blank",
                    self.target_context_id.as_deref(),
                )
                .await?
                .target_id
            }
        };

        self.select_tab(&target_id, config).await?;
        tabs = self.list_tabs().await?;
        if !tabs.iter().any(|tab| tab.target_id == self.target_id) {
            return Err(CoreError::NavigationFailed(
                "could not restore an active page in the browser context".to_string(),
            ));
        }
        Ok(tabs)
    }

    /// Consume the newest `page` target opened by `opener_id` from the
    /// `Target.targetCreated` / `Target.attachedToTarget` event tracker.
    pub fn take_popup_for(
        &self,
        opener_id: &str,
        existing: &std::collections::HashSet<String>,
    ) -> Option<target::TargetInfo> {
        take_popup_from_tracker(&self.popups, opener_id, existing)
    }

    pub async fn select_tab(
        &mut self,
        target_id: &str,
        config: &SessionConfig,
    ) -> Result<String, CoreError> {
        if target_id == self.target_id {
            return Ok(self.last_tree.clone());
        }
        let tabs = self.list_tabs().await?;
        if !tabs.iter().any(|t| t.target_id == target_id) {
            return Err(CoreError::NavigationFailed(format!(
                "unknown page target: {target_id}"
            )));
        }
        let ws_url = self.browser_ws_url.clone();
        // Attach the replacement and re-apply per-session state before
        // detaching the old session or moving the Chrome handle. If any of
        // this fails, `self` stays attached and still owns the browser.
        let mut replacement = Self::attach_with_client(
            self.client.clone(),
            ws_url,
            Some(target_id.to_string()),
            config,
        )
        .await?;
        replacement.browser_context_id = self.browser_context_id.clone();
        let restore = async {
            if !self.extra_headers.is_empty() {
                replacement
                    .set_extra_headers(self.extra_headers.clone())
                    .await?;
            }
            for rule in self.routes.list_rules() {
                replacement.add_route(rule).await?;
            }
            Ok::<(), CoreError>(())
        }
        .await;
        if let Err(error) = restore {
            let _ = target::detach_from_target(&self.client, &replacement.session_id).await;
            return Err(error);
        }
        let _ = target::detach_from_target(&self.client, &self.session_id).await;
        replacement.chrome = self.chrome.take();
        *self = replacement;
        Ok(self.last_tree.clone())
    }

    pub async fn close_tab(
        &mut self,
        target_id: &str,
        config: &SessionConfig,
    ) -> Result<Option<String>, CoreError> {
        let tabs = self.list_tabs().await?;
        if tabs.len() <= 1 {
            return Err(CoreError::NavigationFailed(
                "cannot close the last browser tab".into(),
            ));
        }
        if !tabs.iter().any(|t| t.target_id == target_id) {
            return Err(CoreError::NavigationFailed(format!(
                "unknown page target: {target_id}"
            )));
        }
        if target_id == self.target_id {
            let opener_id = tabs
                .iter()
                .find(|t| t.target_id == target_id)
                .and_then(|t| t.opener_id.as_deref());
            let next = opener_id
                .and_then(|oid| tabs.iter().find(|t| t.target_id == oid))
                .or_else(|| {
                    tabs.iter()
                        .find(|t| t.target_id != target_id && !t.url.starts_with("chrome://"))
                })
                .or_else(|| tabs.iter().find(|t| t.target_id != target_id));
            let Some(next) = next else {
                return Err(CoreError::NavigationFailed(
                    "Chrome reported multiple tabs but no remaining tab was available".into(),
                ));
            };
            let tree = self.select_tab(&next.target_id, config).await?;
            if !target::close_target(&self.client, target_id).await? {
                return Err(CoreError::NavigationFailed(format!(
                    "Chrome refused to close page target: {target_id}"
                )));
            }
            return Ok(Some(tree));
        }
        if !target::close_target(&self.client, target_id).await? {
            return Err(CoreError::NavigationFailed(format!(
                "Chrome refused to close page target: {target_id}"
            )));
        }
        Ok(None)
    }

    pub fn resource_policy(&self) -> ResourcePolicy {
        self.resource_policy
    }

    /// Update the active resource policy via `Network.setBlockedURLs` without
    /// restarting the browser. Only updates session state after CDP succeeds.
    pub async fn set_resource_policy(&mut self, policy: ResourcePolicy) -> Result<(), CoreError> {
        if self.resource_policy == policy {
            return Ok(());
        }
        self.driver.set_resource_policy(policy).await?;
        self.resource_policy = policy;
        Ok(())
    }

    /// Whether text-only AV blocking is active.
    pub fn text_only(&self) -> bool {
        self.resource_policy == ResourcePolicy::TextOnly
    }

    /// Raw `CdpClient` handle (escape hatch for advanced callers).
    pub fn client(&self) -> Arc<CdpClient> {
        self.client.clone()
    }

    /// Raw (uncompressed) AX tree nodes, for debugging the compressor.
    pub async fn raw_ax_nodes(
        &self,
    ) -> Result<Vec<flashbrowse_cdp::protocol::accessibility::AXNode>, CoreError> {
        self.driver.get_full_ax_tree().await
    }

    /// Evaluate an expression in the page (engine-neutral).
    ///
    /// Used by the screenshot prepare (scroll bookkeeping) and exposed for
    /// callers that need to inspect page state.
    pub async fn evaluate(&self, expression: &str) -> Result<Value, CoreError> {
        self.driver.evaluate(expression).await
    }

    /// Current vertical scroll offset of the page.
    pub async fn evaluate_scroll_y(&self) -> f64 {
        match self.evaluate("window.scrollY").await {
            Ok(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
            _ => 0.0,
        }
    }

    /// Re-fetch AXTree, recompress, re-render. Single point of rebuild so
    /// every action funnels through identical state.
    pub async fn rebuild_tree(&mut self) -> Result<String, CoreError> {
        let (raw, node_sessions) = self.driver.get_full_ax_tree_with_sessions().await?;
        let (nodes, map) = compress(&raw);
        let rendered = render_nested(&self.title, &self.url, &nodes);
        self.action_sessions = build_action_sessions(&nodes, &node_sessions);
        self.action_map = map;
        self.last_tree = rendered.clone();
        Ok(rendered)
    }

    fn frame_for_action(&self, action_id: u32) -> (String, Option<i64>) {
        match self.action_sessions.get(&action_id) {
            Some(fs) => (fs.session_id.clone(), fs.owner_backend),
            None => (self.session_id.clone(), None),
        }
    }

    async fn frame_offset(&self, owner_backend: Option<i64>) -> (f64, f64) {
        let Some(owner) = owner_backend else {
            return (0.0, 0.0);
        };
        if let Ok(resp) = dom::get_box_model(&self.client, &self.session_id, owner).await {
            if let [x, y, ..] = resp.model.content.as_slice() {
                return (*x, *y);
            }
        }
        (0.0, 0.0)
    }

    /// `click(action_id)`: scroll → box → center → mouse events → rebuild.
    pub async fn click(&mut self, action_id: u32) -> Result<String, CoreError> {
        let backend = *self
            .action_map
            .get(&action_id)
            .ok_or(CoreError::ActionNotFound(action_id))?;
        let (dom_sid, owner) = self.frame_for_action(action_id);
        self.click_at_backend(&dom_sid, owner, backend).await?;
        self.post_action_settle(Duration::from_millis(400)).await?;
        self.refresh_title_url().await;
        self.rebuild_tree().await
    }

    /// `hover(action_id)`: move mouse to element center without clicking → rebuild.
    pub async fn hover(&mut self, action_id: u32) -> Result<String, CoreError> {
        let backend = *self
            .action_map
            .get(&action_id)
            .ok_or(CoreError::ActionNotFound(action_id))?;
        let (dom_sid, owner) = self.frame_for_action(action_id);
        if let Err(e) = dom::scroll_into_view_if_needed(&self.client, &dom_sid, backend).await {
            tracing::warn!(backend, "scrollIntoViewIfNeeded failed (proceeding): {e}");
        }
        let resp = dom::get_box_model(&self.client, &dom_sid, backend).await?;
        let (cx, cy) = box_center(&resp.model).ok_or(CoreError::NoBoxModel(action_id))?;
        let (ox, oy) = self.frame_offset(owner).await;
        let (x, y) = (cx + ox, cy + oy);
        input::dispatch_mouse_event(
            &self.client,
            &self.session_id,
            &input::DispatchMouseEventParams {
                type_: "mouseMoved".to_string(),
                x,
                y,
                button: None,
                buttons: None,
                click_count: None,
                delta_x: None,
                delta_y: None,
                modifiers: None,
            },
        )
        .await?;
        self.post_action_settle(Duration::from_millis(200)).await?;
        self.rebuild_tree().await
    }

    async fn fill_backend(
        &self,
        dom_sid: &str,
        owner: Option<i64>,
        backend: i64,
        text: &str,
    ) -> Result<(), CoreError> {
        self.click_at_backend(dom_sid, owner, backend).await?;
        for ev in [select_all_event(), backspace_event()] {
            let down = ev.clone();
            input::dispatch_key_event(&self.client, &self.session_id, &down).await?;
            let mut up = down;
            up.type_ = "keyUp".to_string();
            input::dispatch_key_event(&self.client, &self.session_id, &up).await?;
        }
        input::insert_text(&self.client, &self.session_id, text).await?;
        Ok(())
    }

    /// `fill(action_id, text)`: focus → Ctrl+A → Backspace → insert → rebuild.
    pub async fn fill(&mut self, action_id: u32, text: &str) -> Result<String, CoreError> {
        let backend = *self
            .action_map
            .get(&action_id)
            .ok_or(CoreError::ActionNotFound(action_id))?;
        let (dom_sid, owner) = self.frame_for_action(action_id);
        self.fill_backend(&dom_sid, owner, backend, text).await?;
        self.post_action_settle(Duration::from_millis(200)).await?;
        self.rebuild_tree().await
    }

    /// Fill multiple form fields sequentially from the current action map,
    /// stopping on the first unavailable/failing field and always returning
    /// the refreshed final tree.
    pub async fn fill_form(
        &mut self,
        fields: &[(u32, String)],
    ) -> Result<FillFormOutcome, CoreError> {
        let mut completed = Vec::with_capacity(fields.len());
        let mut failed = None;
        for (action_id, value) in fields {
            let Some(&backend) = self.action_map.get(action_id) else {
                failed = Some((
                    *action_id,
                    CoreError::ActionNotFound(*action_id).to_string(),
                ));
                break;
            };
            let (dom_sid, owner) = self.frame_for_action(*action_id);
            if let Err(err) = self.fill_backend(&dom_sid, owner, backend, value).await {
                failed = Some((*action_id, err.to_string()));
                break;
            }
            completed.push(*action_id);
        }
        self.post_action_settle(Duration::from_millis(250)).await?;
        let tree = self.rebuild_tree().await?;
        Ok(FillFormOutcome {
            completed,
            failed,
            tree,
        })
    }

    async fn resolve_action_center(&self, action_id: u32) -> Result<(f64, f64), CoreError> {
        let backend = *self
            .action_map
            .get(&action_id)
            .ok_or(CoreError::ActionNotFound(action_id))?;
        let (dom_sid, owner) = self.frame_for_action(action_id);
        let _ = dom::scroll_into_view_if_needed(&self.client, &dom_sid, backend).await;
        let resp = dom::get_box_model(&self.client, &dom_sid, backend).await?;
        let (cx, cy) = box_center(&resp.model).ok_or(CoreError::NoBoxModel(action_id))?;
        let (ox, oy) = self.frame_offset(owner).await;
        Ok((cx + ox, cy + oy))
    }

    /// `double_click(action_id)`: two synthetic clicks (`clickCount: 1` then `2`) -> rebuild.
    pub async fn double_click(&mut self, action_id: u32) -> Result<String, CoreError> {
        let (x, y) = self.resolve_action_center(action_id).await?;
        let mv = input::DispatchMouseEventParams {
            type_: "mouseMoved".to_string(),
            x,
            y,
            button: None,
            buttons: None,
            click_count: None,
            delta_x: None,
            delta_y: None,
            modifiers: None,
        };
        input::dispatch_mouse_event(&self.client, &self.session_id, &mv).await?;
        for count in [1, 2] {
            let down = input::DispatchMouseEventParams {
                type_: "mousePressed".to_string(),
                x,
                y,
                button: Some("left".to_string()),
                buttons: Some(1),
                click_count: Some(count),
                delta_x: None,
                delta_y: None,
                modifiers: None,
            };
            input::dispatch_mouse_event(&self.client, &self.session_id, &down).await?;
            let up = input::DispatchMouseEventParams {
                type_: "mouseReleased".to_string(),
                buttons: Some(0),
                ..down
            };
            input::dispatch_mouse_event(&self.client, &self.session_id, &up).await?;
        }
        self.post_action_settle(Duration::from_millis(300)).await?;
        self.refresh_title_url().await;
        self.rebuild_tree().await
    }

    /// `right_click(action_id)`: synthetic secondary mouse button click -> rebuild.
    pub async fn right_click(&mut self, action_id: u32) -> Result<String, CoreError> {
        let (x, y) = self.resolve_action_center(action_id).await?;
        let mv = input::DispatchMouseEventParams {
            type_: "mouseMoved".to_string(),
            x,
            y,
            button: None,
            buttons: None,
            click_count: None,
            delta_x: None,
            delta_y: None,
            modifiers: None,
        };
        input::dispatch_mouse_event(&self.client, &self.session_id, &mv).await?;
        let down = input::DispatchMouseEventParams {
            type_: "mousePressed".to_string(),
            x,
            y,
            button: Some("right".to_string()),
            buttons: Some(2),
            click_count: Some(1),
            delta_x: None,
            delta_y: None,
            modifiers: None,
        };
        input::dispatch_mouse_event(&self.client, &self.session_id, &down).await?;
        let up = input::DispatchMouseEventParams {
            type_: "mouseReleased".to_string(),
            buttons: Some(0),
            ..down
        };
        input::dispatch_mouse_event(&self.client, &self.session_id, &up).await?;
        self.post_action_settle(Duration::from_millis(300)).await?;
        self.refresh_title_url().await;
        self.rebuild_tree().await
    }

    /// `drag_and_drop(source_id, target_id)`: smooth pointer drag from source to target -> rebuild.
    pub async fn drag_and_drop(
        &mut self,
        source_id: u32,
        target_id: u32,
    ) -> Result<String, CoreError> {
        if !self.action_map.contains_key(&target_id) {
            return Err(CoreError::ActionNotFound(target_id));
        }
        let (sx, sy) = self.resolve_action_center(source_id).await?;
        input::dispatch_mouse_event(
            &self.client,
            &self.session_id,
            &input::DispatchMouseEventParams {
                type_: "mouseMoved".to_string(),
                x: sx,
                y: sy,
                button: None,
                buttons: None,
                click_count: None,
                delta_x: None,
                delta_y: None,
                modifiers: None,
            },
        )
        .await?;
        input::dispatch_mouse_event(
            &self.client,
            &self.session_id,
            &input::DispatchMouseEventParams {
                type_: "mousePressed".to_string(),
                x: sx,
                y: sy,
                button: Some("left".to_string()),
                buttons: Some(1),
                click_count: Some(1),
                delta_x: None,
                delta_y: None,
                modifiers: None,
            },
        )
        .await?;
        // Resolve target after mousePressed so scrolling target into view cannot shift source before grab.
        let (tx, ty) = self.resolve_action_center(target_id).await?;
        const STEPS: usize = 8;
        for step in 1..=STEPS {
            let t = step as f64 / STEPS as f64;
            let x = sx + (tx - sx) * t;
            let y = sy + (ty - sy) * t;
            input::dispatch_mouse_event(
                &self.client,
                &self.session_id,
                &input::DispatchMouseEventParams {
                    type_: "mouseMoved".to_string(),
                    x,
                    y,
                    button: Some("left".to_string()),
                    buttons: Some(1),
                    click_count: None,
                    delta_x: None,
                    delta_y: None,
                    modifiers: None,
                },
            )
            .await?;
        }
        input::dispatch_mouse_event(
            &self.client,
            &self.session_id,
            &input::DispatchMouseEventParams {
                type_: "mouseReleased".to_string(),
                x: tx,
                y: ty,
                button: Some("left".to_string()),
                buttons: Some(0),
                click_count: Some(1),
                delta_x: None,
                delta_y: None,
                modifiers: None,
            },
        )
        .await?;
        self.post_action_settle(Duration::from_millis(300)).await?;
        self.refresh_title_url().await;
        self.rebuild_tree().await
    }

    /// `scroll(direction, amount_px)`: synthetic `mouseWheel` -> rebuild.
    pub async fn scroll(
        &mut self,
        direction: ScrollDirection,
        amount: f64,
    ) -> Result<String, CoreError> {
        self.scroll_at(None, direction, amount).await
    }

    /// Scroll at a specific element's center (for inner scrollable containers) or viewport center.
    pub async fn scroll_at(
        &mut self,
        action_id: Option<u32>,
        direction: ScrollDirection,
        amount: f64,
    ) -> Result<String, CoreError> {
        let (x, y) = match action_id {
            Some(id) if id > 0 => self.resolve_action_center(id).await?,
            _ => (640.0, 400.0),
        };
        let (dx, dy) = direction.delta(amount);
        input::dispatch_mouse_event(
            &self.client,
            &self.session_id,
            &input::DispatchMouseEventParams {
                type_: "mouseWheel".to_string(),
                x,
                y,
                button: None,
                buttons: None,
                click_count: None,
                delta_x: Some(dx),
                delta_y: Some(dy),
                modifiers: None,
            },
        )
        .await?;
        self.post_action_settle(Duration::from_millis(300)).await?;
        self.rebuild_tree().await
    }

    /// `select(action_id, text)`: pick a `<select>` option by visible text
    /// or value, dispatch `change` + `input`, then rebuild.
    ///
    /// Implemented via `DOM.resolveNode` + `Runtime.callFunctionOn` so the
    /// native dropdown popup is never opened (opening it would swallow
    /// subsequent input events and leave the tree in `expanded` state).
    pub async fn select(&mut self, action_id: u32, text: &str) -> Result<String, CoreError> {
        let backend = *self
            .action_map
            .get(&action_id)
            .ok_or(CoreError::ActionNotFound(action_id))?;
        let (dom_sid, _) = self.frame_for_action(action_id);
        let resolved = dom::resolve_node(&self.client, &dom_sid, backend).await?;
        let object_id = resolved
            .object
            .object_id
            .ok_or(CoreError::NoBackendId(action_id))?;
        let needle = serde_json::to_string(text)?;
        let resp = runtime::call_function_on(
            &self.client,
            &dom_sid,
            &object_id,
            "function(want) { if (!this || this.tagName !== 'SELECT') return 'not-a-select'; for (const o of this.options) { if (o.text.trim() === want.trim() || o.value === want) { this.value = o.value; this.dispatchEvent(new Event('change', { bubbles: true })); this.dispatchEvent(new Event('input', { bubbles: true })); return 'ok:' + o.value; } } return 'no-match'; }",
            vec![serde_json::json!(needle)],
        )
        .await?;
        let outcome = resp
            .result
            .value
            .as_ref()
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        if !outcome.starts_with("ok:") {
            tracing::warn!(action_id, text, outcome, "select failed");
        }
        self.post_action_settle(Duration::from_millis(200)).await?;
        self.rebuild_tree().await
    }

    /// `press_key(key)`: single key or modifier chord (e.g. `"Control+K"`, `"Shift+Tab"`) -> rebuild.
    pub async fn press_key(&mut self, key: &str) -> Result<String, CoreError> {
        let chord = crate::actions::keys::resolve_chord(key)
            .ok_or_else(|| CoreError::InvalidScrollDirection(format!("unsupported key: {key}")))?;
        for m in &chord.modifier_specs {
            input::dispatch_key_event(
                &self.client,
                &self.session_id,
                &input::DispatchKeyEventParams {
                    type_: "rawKeyDown".to_string(),
                    key: m.key.clone(),
                    code: Some(m.code.clone()),
                    modifiers: Some(chord.modifiers),
                    windows_virtual_key_code: m.vk,
                    native_virtual_key_code: None,
                },
            )
            .await?;
        }
        let params = input::DispatchKeyEventParams {
            type_: "keyDown".to_string(),
            key: chord.key.key.clone(),
            code: Some(chord.key.code.clone()),
            modifiers: (chord.modifiers != 0).then_some(chord.modifiers),
            windows_virtual_key_code: chord.key.vk,
            native_virtual_key_code: None,
        };
        input::dispatch_key_event(&self.client, &self.session_id, &params).await?;
        let mut up = params;
        up.type_ = "keyUp".to_string();
        input::dispatch_key_event(&self.client, &self.session_id, &up).await?;
        for m in chord.modifier_specs.iter().rev() {
            input::dispatch_key_event(
                &self.client,
                &self.session_id,
                &input::DispatchKeyEventParams {
                    type_: "keyUp".to_string(),
                    key: m.key.clone(),
                    code: Some(m.code.clone()),
                    modifiers: Some(0),
                    windows_virtual_key_code: m.vk,
                    native_virtual_key_code: None,
                },
            )
            .await?;
        }
        self.post_action_settle(Duration::from_millis(250)).await?;
        self.refresh_title_url().await;
        self.rebuild_tree().await
    }

    /// Navigate back one entry in session history and return the refreshed tree.
    pub async fn back(&mut self) -> Result<String, CoreError> {
        let hist = page::get_navigation_history(&self.client, &self.session_id).await?;
        let target = adjacent_history_entry(&hist, -1)
            .ok_or_else(|| CoreError::NavigationFailed("no previous history entry".to_string()))?;
        self.net.reset();
        page::navigate_to_history_entry(&self.client, &self.session_id, target.id).await?;
        self.settle_after_history_nav().await
    }

    /// Navigate forward one entry in session history and return the refreshed tree.
    pub async fn forward(&mut self) -> Result<String, CoreError> {
        let hist = page::get_navigation_history(&self.client, &self.session_id).await?;
        let target = adjacent_history_entry(&hist, 1)
            .ok_or_else(|| CoreError::NavigationFailed("no next history entry".to_string()))?;
        self.net.reset();
        page::navigate_to_history_entry(&self.client, &self.session_id, target.id).await?;
        self.settle_after_history_nav().await
    }

    /// Reload the current page and return the refreshed tree.
    pub async fn reload(&mut self, ignore_cache: bool) -> Result<String, CoreError> {
        self.net.reset();
        page::reload(&self.client, &self.session_id, ignore_cache).await?;
        self.settle_after_history_nav().await
    }

    async fn settle_after_history_nav(&mut self) -> Result<String, CoreError> {
        flashbrowse_cdp::runtime::run(self.settle_after_history_nav_inner()).await?
    }

    async fn settle_after_history_nav_inner(&mut self) -> Result<String, CoreError> {
        // Brief grace period so Chrome begins transitioning out of the old document.
        tokio::time::sleep(Duration::from_millis(60)).await;
        self.wait_for_popup_settle().await?;
        self.refresh_title_url().await;
        if self.url.starts_with("chrome-error://") {
            self.invalidate_tree();
            return Err(CoreError::NavigationFailed(self.url.clone()));
        }
        self.rebuild_tree().await
    }

    /// Wait until `condition` holds or `timeout_ms` elapses (capped 100..=30_000 ms),
    /// then return the refreshed semantic tree.
    pub async fn wait_for(
        &mut self,
        condition: WaitCondition,
        timeout_ms: u64,
    ) -> Result<String, CoreError> {
        flashbrowse_cdp::runtime::run(self.wait_for_inner(condition, timeout_ms)).await?
    }

    async fn wait_for_inner(
        &mut self,
        condition: WaitCondition,
        timeout_ms: u64,
    ) -> Result<String, CoreError> {
        let clamped = timeout_ms.clamp(100, 30_000);
        let deadline = {
            let now = tokio::time::Instant::now();
            now.checked_add(Duration::from_millis(clamped))
                .unwrap_or(now)
        };
        loop {
            let matched = match &condition {
                WaitCondition::Text(needle) => {
                    let needle_js = serde_json::to_string(needle)?;
                    let expr = format!(
                        r#"(() => {{
  const want = {needle_js};
  const pending = [document];
  while (pending.length) {{
    const root = pending.pop();
    if (root === document && document.body && (document.body.innerText || '').includes(want)) return true;
    if (root instanceof ShadowRoot && (root.textContent || '').includes(want)) return true;
    for (const el of root.querySelectorAll('*')) {{
      if (el.shadowRoot) pending.push(el.shadowRoot);
    }}
  }}
  return false;
}})()"#
                    );
                    self.driver
                        .evaluate(&expr)
                        .await
                        .ok()
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false)
                }
                WaitCondition::Selector(selector) => {
                    let sel_js = serde_json::to_string(selector)?;
                    let expr = format!(
                        r#"(() => {{
  const sel = {sel_js};
  const pending = [document];
  while (pending.length) {{
    const root = pending.pop();
    try {{ if (root.querySelector(sel)) return true; }} catch (_) {{ return false; }}
    for (const el of root.querySelectorAll('*')) {{
      if (el.shadowRoot) pending.push(el.shadowRoot);
    }}
  }}
  return false;
}})()"#
                    );
                    self.driver
                        .evaluate(&expr)
                        .await
                        .ok()
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false)
                }
                WaitCondition::UrlContains(part) => self
                    .driver
                    .evaluate("location.href")
                    .await
                    .ok()
                    .and_then(|v| v.as_str().map(|s| s.contains(part.as_str())))
                    .unwrap_or(false),
                WaitCondition::NetworkIdle { quiet_ms } => self.net.idle(*quiet_ms),
            };
            if matched {
                self.refresh_title_url().await;
                return self.rebuild_tree().await;
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(CoreError::NavigationFailed(format!(
                    "wait_for timed out after {clamped}ms"
                )));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Captured console messages and uncaught page exceptions (bounded to 200 entries).
    pub fn console_entries(&self, level: Option<&str>) -> Vec<ConsoleEntry> {
        self.console.list(level)
    }

    /// Clear captured console entries.
    pub fn clear_console(&self) {
        self.console.clear();
    }

    /// Captured network request/response entries (bounded to 200 entries).
    pub fn network_entries(&self, failed_only: bool) -> Vec<NetworkEntry> {
        self.network_log.list(failed_only)
    }

    /// Fetch the response body for a captured `request_id` (capped at 64 KiB text).
    pub async fn network_response_body(&self, request_id: &str) -> Result<String, CoreError> {
        let resp = network::get_response_body(&self.client, &self.session_id, request_id).await?;
        let text = if resp.base64_encoded {
            match flashbrowse_cdp::protocol::fetch::decode_base64(&resp.body) {
                Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                Err(_) => resp.body,
            }
        } else {
            resp.body
        };
        const MAX_BODY_BYTES: usize = 64 * 1024;
        if text.len() > MAX_BODY_BYTES {
            let end = text.floor_char_boundary(MAX_BODY_BYTES);
            Ok(format!(
                "{}...(truncated)",
                text.get(..end).unwrap_or_default()
            ))
        } else {
            Ok(text)
        }
    }

    /// Clear captured network log entries.
    pub fn clear_network(&self) {
        self.network_log.clear();
    }

    /// Set extra HTTP headers sent on every subsequent request in this session.
    pub async fn set_extra_headers(
        &mut self,
        headers: HashMap<String, String>,
    ) -> Result<(), CoreError> {
        network::set_extra_http_headers(&self.client, &self.session_id, &headers).await?;
        self.extra_headers = headers;
        Ok(())
    }

    /// Add a lazy `Fetch` route mock or abort rule.
    pub async fn add_route(&self, rule: RouteRule) -> Result<(), CoreError> {
        self.routes
            .add_rule(&self.client, &self.session_id, rule)
            .await?;
        Ok(())
    }

    /// List active `Fetch` route rules.
    pub fn list_routes(&self) -> Vec<RouteRule> {
        self.routes.list_rules()
    }

    /// Clear all active `Fetch` route rules and disable `Fetch` interception.
    pub async fn clear_routes(&self) -> Result<(), CoreError> {
        self.routes
            .clear_rules(&self.client, &self.session_id)
            .await?;
        Ok(())
    }

    /// Extract clean Markdown (boss-level `browser_extract_markdown`).
    pub async fn extract_markdown(&self) -> Result<String, CoreError> {
        let resp = runtime::evaluate(&self.client, &self.session_id, EXTRACT_SCRIPT).await?;
        if let Some(details) = resp.exception_details {
            return Err(CoreError::NavigationFailed(format!(
                "page extraction failed: {details}"
            )));
        }
        if let Some(v) = resp.result.value {
            match v {
                Value::String(s) => return Ok(s),
                Value::Object(obj) => {
                    if let Some(Value::String(md)) = obj.get("markdown") {
                        return Ok(md.clone());
                    }
                    return Ok(serde_json::to_string(&Value::Object(obj))?);
                }
                other => return Ok(other.to_string()),
            }
        }
        Ok(String::new())
    }

    /// Capture PNG screenshot (base64). See [`ScreenshotOptions`].
    ///
    /// In agent-default mode, font and image blocks are lifted for the prepare
    /// window, then restored. [`ResourcePolicy::FullRender`] skips that dance.
    ///
    /// Lifting the blocklist alone is **not** enough: `Network.setBlockedURLs`
    /// only affects *future* requests, and Chrome never retries a request that
    /// was already blocked. A blocked `<img>` therefore stays broken forever.
    /// So the prepare window also re-requests the images (see
    /// [`Self::reload_blocked_images`]), which is what actually makes pixels
    /// appear.
    pub async fn screenshot(&self, opts: ScreenshotOptions) -> Result<String, CoreError> {
        flashbrowse_cdp::runtime::run(self.screenshot_inner(opts)).await?
    }

    async fn screenshot_inner(&self, opts: ScreenshotOptions) -> Result<String, CoreError> {
        // Validate action_id before lifting resource blocks or mutating page state.
        if let Some(action_id) = opts.action_id {
            if !self.action_map.contains_key(&action_id) {
                return Err(CoreError::ActionNotFound(action_id));
            }
        }
        let mut guard = ScreenshotGuard {
            client: &self.client,
            armed: true,
        };
        let needs_lift = self.resource_policy != ResourcePolicy::FullRender;
        if needs_lift {
            self.driver
                .set_blocklist(blocked_patterns_for_screenshot(self.resource_policy))
                .await?;
        }
        // Re-request images that were blocked during the page load. Without
        // this the capture shows broken-image glyphs, no matter how long we
        // wait for them.
        if needs_lift {
            self.reload_blocked_images().await?;
        }
        if opts.prepare_lazy_content && opts.action_id.is_none() {
            self.prepare_lazy_content_for_screenshot(opts.full_page)
                .await?;
        }
        self.wait_for_visual_ready(opts.timeout_ms).await?;
        let capture_res = async {
            let clip = if let Some(action_id) = opts.action_id {
                let backend = *self
                    .action_map
                    .get(&action_id)
                    .ok_or(CoreError::ActionNotFound(action_id))?;
                let (dom_sid, owner) = self.frame_for_action(action_id);
                let _ = dom::scroll_into_view_if_needed(&self.client, &dom_sid, backend).await;
                let resp = dom::get_box_model(&self.client, &dom_sid, backend).await?;
                let quad = if resp.model.border.len() >= 8 {
                    &resp.model.border
                } else {
                    &resp.model.content
                };
                let [x1, y1, x2, y2, x3, y3, x4, y4, ..] = quad.as_slice() else {
                    return Err(CoreError::NoBoxModel(action_id));
                };
                let (ox, oy) = self.frame_offset(owner).await;
                let xs = [*x1, *x2, *x3, *x4];
                let ys = [*y1, *y2, *y3, *y4];
                let min_x = xs.into_iter().fold(f64::INFINITY, f64::min) + ox;
                let max_x = xs.into_iter().fold(f64::NEG_INFINITY, f64::max) + ox;
                let min_y = ys.into_iter().fold(f64::INFINITY, f64::min) + oy;
                let max_y = ys.into_iter().fold(f64::NEG_INFINITY, f64::max) + oy;
                let width = (max_x - min_x).max(1.0);
                let height = (max_y - min_y).max(1.0);
                Some(page::Viewport {
                    x: min_x.max(0.0),
                    y: min_y.max(0.0),
                    width,
                    height,
                    scale: 1.0,
                })
            } else {
                None
            };
            self.driver
                .capture_screenshot_clipped(opts.full_page && clip.is_none(), clip)
                .await
        }
        .await;
        if needs_lift {
            self.driver
                .set_resource_policy(self.resource_policy)
                .await?;
        }
        guard.armed = false;
        capture_res
    }

    /// Apply viewport, timezone, locale, and/or geolocation emulation overrides.
    pub async fn emulate(&self, opts: EmulationOptions) -> Result<(), CoreError> {
        if let Some(vp) = opts.viewport {
            emulation::set_device_metrics_override(
                &self.client,
                &self.session_id,
                &emulation::DeviceMetricsParams {
                    width: vp.width,
                    height: vp.height,
                    device_scale_factor: vp.device_scale_factor,
                    mobile: vp.mobile,
                },
            )
            .await?;
        }
        if let Some(tz) = opts.timezone {
            emulation::set_timezone_override(&self.client, &self.session_id, &tz).await?;
        }
        if let Some(loc) = opts.locale {
            emulation::set_locale_override(&self.client, &self.session_id, &loc).await?;
        }
        if let Some(geo) = opts.geolocation {
            emulation::set_geolocation_override(
                &self.client,
                &self.session_id,
                geo.latitude,
                geo.longitude,
                geo.accuracy,
            )
            .await?;
        }
        Ok(())
    }

    /// Clear device metrics, timezone, locale, geolocation, and permission overrides.
    pub async fn reset_emulation(&self) -> Result<(), CoreError> {
        let mut first_err: Option<CoreError> = None;
        let mut remember = |result: Result<(), CdpError>| {
            if let Err(err) = result {
                first_err.get_or_insert(err.into());
            }
        };
        remember(emulation::clear_device_metrics_override(&self.client, &self.session_id).await);
        remember(emulation::set_timezone_override(&self.client, &self.session_id, "").await);
        remember(emulation::set_locale_override(&self.client, &self.session_id, "").await);
        remember(emulation::clear_geolocation_override(&self.client, &self.session_id).await);
        remember(
            browser::reset_permissions(&self.client, self.browser_context_id.as_deref()).await,
        );
        match first_err {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    /// Grant browser permissions (e.g. `geolocation`, `notifications`, `clipboardReadWrite`).
    pub async fn grant_permissions(
        &self,
        origin: Option<&str>,
        permissions: &[String],
    ) -> Result<(), CoreError> {
        browser::grant_permissions(
            &self.client,
            origin,
            permissions,
            self.browser_context_id.as_deref(),
        )
        .await?;
        Ok(())
    }

    /// Render the current page to PDF bytes via `Page.printToPDF`.
    pub async fn print_to_pdf(&self, opts: PdfOptions) -> Result<Vec<u8>, CoreError> {
        let safe_scale = if opts.scale.is_finite() {
            opts.scale.clamp(0.1, 2.0)
        } else {
            1.0
        };
        let resp = page::print_to_pdf(
            &self.client,
            &self.session_id,
            &page::PrintToPdfParams {
                landscape: Some(opts.landscape),
                print_background: Some(opts.print_background),
                scale: Some(safe_scale),
                paper_width: None,
                paper_height: None,
            },
        )
        .await?;
        fetch::decode_base64(&resp.data).map_err(CoreError::NavigationFailed)
    }

    /// Convenience wrapper (viewport, default prepare/timeout).
    pub async fn screenshot_viewport(&self) -> Result<String, CoreError> {
        self.screenshot(ScreenshotOptions::default()).await
    }

    /// Re-request images that the network layer blocked during page load.
    ///
    /// `Network.setBlockedURLs` prevents the *request*; it does not prevent
    /// the `<img>` element from existing, and Chrome does not retry a blocked
    /// request on its own. Re-assigning the source issues a fresh request,
    /// which now succeeds because the blocklist has been lifted.
    ///
    /// Images that already loaded are left alone (`complete && naturalWidth`),
    /// so this never re-fetches a working image. Inline `data:` URIs are
    /// never blocked, so they are skipped too.
    async fn reload_blocked_images(&self) -> Result<(), CoreError> {
        // Bounded: a pathological page with thousands of images must not
        // turn one screenshot into thousands of requests.
        const MAX_RETRY: usize = 200;
        let expr = format!(
            r#"(function(){{
  const imgs = Array.from(document.images || []);
  let n = 0;
  for (const img of imgs) {{
    if (n >= {MAX_RETRY}) break;
    if (img.complete && img.naturalWidth > 0) continue; // already loaded
    const src = img.currentSrc || img.src;
    if (!src || src.indexOf('data:') === 0) continue;   // inline data URI
    if (img.srcset) {{
      // Re-trigger the srcset selection rather than pinning one candidate.
      const s = img.srcset;
      img.srcset = '';
      img.srcset = s;
    }} else {{
      img.src = '';
      img.src = src;
    }}
    n++;
  }}
  return n;
}})()"#
        );
        if let Value::Number(n) = self.driver.evaluate(&expr).await? {
            let count = n.as_u64().unwrap_or(0);
            if count > 0 {
                tracing::debug!(count, "re-requested blocked images");
            }
        }
        Ok(())
    }

    /// Surface lazy-loaded content before a capture.
    ///
    /// The page is scrolled through so that `loading="lazy"` images start
    /// fetching, then the agent's original scroll position is restored. The
    /// restore matters: the agent asked for a picture of what it is currently
    /// looking at, so snapping the viewport back to the top would destroy
    /// that context.
    async fn prepare_lazy_content_for_screenshot(&self, full_page: bool) -> Result<(), CoreError> {
        // Remember where the agent was. The value is held here (not on
        // `window`) so nothing is left behind in the page's global scope.
        let saved_y = self
            .driver
            .evaluate("window.scrollY")
            .await?
            .as_f64()
            .ok_or_else(|| {
                CoreError::NavigationFailed("could not read the page scroll position".to_string())
            })?;
        let target = if full_page { 0.0 } else { saved_y };
        let script = format!(
            r#"(function(){{
  try {{
  const h = Math.max(document.body.scrollHeight, document.documentElement.scrollHeight);
  const step = Math.max(400, window.innerHeight);
  for (let y = 0; y < h; y += step) {{ window.scrollTo(0, y); }}
  return true;
  }} finally {{
    const key = Symbol.for('flashbrowse.screenshot.scroll.restore');
    window[key] = setTimeout(() => {{ delete window[key]; window.scrollTo(0, {target}); }}, 150);
  }}
}})()"#
        );
        self.driver.evaluate(&script).await?;
        // Let the page restore its viewport before preparing the capture.
        tokio::time::sleep(Duration::from_millis(150)).await;
        self.driver
            .evaluate(&format!("(() => {{ const key = Symbol.for('flashbrowse.screenshot.scroll.restore'); clearTimeout(window[key]); delete window[key]; window.scrollTo(0, {target}); return true; }})()"))
            .await?;
        Ok(())
    }

    async fn wait_for_visual_ready(&self, timeout_ms: u64) -> Result<(), CoreError> {
        let budget = timeout_ms.clamp(500, SCREENSHOT_PREPARE_MS_MAX);
        let deadline = {
            let now = tokio::time::Instant::now();
            now.checked_add(Duration::from_millis(budget))
                .unwrap_or(now)
        };
        let quiet_ms = 350u64;
        loop {
            if tokio::time::Instant::now() >= deadline {
                tracing::warn!("screenshot prepare deadline reached - capturing best effort");
                break;
            }
            let net_idle = self.net.idle(quiet_ms);
            let images_ok = self.images_ready().await?;
            if net_idle && images_ok {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Ok(())
    }

    /// True when no image is still pending.
    ///
    /// `img.complete` is the readiness signal: it is true once the image has
    /// loaded *or* failed. Requiring `naturalWidth > 0` as well would mean a
    /// permanently broken image (blocked, 404, unsupported format) keeps the
    /// wait loop spinning until the deadline - every screenshot of a page with
    /// one bad image would cost the full timeout.
    async fn images_ready(&self) -> Result<bool, CoreError> {
        let expr = r#"(function(){
  const imgs = Array.from(document.images || []);
  if (imgs.length === 0) return true;
  return imgs.every(function(img){
    if (!img.src) return true;
    return img.complete;
  });
})()"#;
        self.driver.evaluate(expr).await?.as_bool().ok_or_else(|| {
            CoreError::NavigationFailed("could not check image readiness".to_string())
        })
    }

    /// `Title`/`URL` header fields via in-page evaluation (best-effort).
    async fn refresh_title_url(&mut self) {
        if let Ok(r) = runtime::evaluate(&self.client, &self.session_id, "document.title").await {
            if let Some(Value::String(s)) = r.result.value {
                if !s.is_empty() {
                    self.title = s;
                }
            }
        }
        if let Ok(r) = runtime::evaluate(&self.client, &self.session_id, "location.href").await {
            if let Some(Value::String(s)) = r.result.value {
                if !s.is_empty() {
                    self.url = s;
                }
            }
        }
    }

    /// Wait for post-action page activity (in-flight network + DOM stability)
    /// instead of a hardcoded sleep.
    async fn post_action_settle(&self, max_wait: Duration) -> Result<(), CoreError> {
        flashbrowse_cdp::runtime::run(self.post_action_settle_inner(max_wait)).await?;
        Ok(())
    }

    async fn post_action_settle_inner(&self, max_wait: Duration) {
        let deadline = {
            let now = tokio::time::Instant::now();
            now.checked_add(max_wait).unwrap_or(now)
        };
        let mut prev_state: Option<String> = None;
        loop {
            let state = self
                .driver
                .evaluate(SETTLE_DOM_STATS_JS)
                .await
                .ok()
                .and_then(|v| v.as_str().map(str::to_string));
            let net_quiet = self.net.idle(30);
            let ready = state.as_deref().is_some_and(|s| s.starts_with("complete|"));
            if net_quiet && ready && prev_state.is_some() && prev_state == state {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            prev_state = state;
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Wait until `backend` is visible + stable (actionability check -
    /// Playwright-parity, beats fixed-sleep clicking). Polls box center;
    /// stable = same center twice in a row, visible = center inside viewport.
    async fn wait_for_actionable(
        &self,
        dom_session_id: &str,
        backend: i64,
    ) -> Result<(f64, f64), CoreError> {
        flashbrowse_cdp::runtime::run(self.wait_for_actionable_inner(dom_session_id, backend))
            .await?
    }

    async fn wait_for_actionable_inner(
        &self,
        dom_session_id: &str,
        backend: i64,
    ) -> Result<(f64, f64), CoreError> {
        let mut prev: Option<(f64, f64)> = None;
        let mut stable_count = 0u8;
        let deadline = {
            let now = tokio::time::Instant::now();
            now.checked_add(Duration::from_secs(5)).unwrap_or(now)
        };
        loop {
            if let Err(e) =
                dom::scroll_into_view_if_needed(&self.client, dom_session_id, backend).await
            {
                tracing::warn!(backend, "scrollIntoViewIfNeeded failed (proceeding): {e}");
            }
            let resp = match dom::get_box_model(&self.client, dom_session_id, backend).await {
                Ok(r) => r,
                Err(e) => {
                    // Transient: node not in the layout tree yet.
                    tracing::warn!(backend, "getBoxModel failed (retrying): {e}");
                    if tokio::time::Instant::now() >= deadline {
                        return Err(e.into());
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                    continue;
                }
            };
            let center = box_center(&resp.model).ok_or(CoreError::NoBoxModel(0))?;
            let (x, y) = center;
            let visible = x > 0.0
                && y > 0.0
                && x < 1280.0
                && y < 800.0
                && resp.model.width > 0.0
                && resp.model.height > 0.0;
            if visible {
                if prev == Some(center) {
                    stable_count = stable_count.saturating_add(1);
                    if stable_count >= 1 {
                        return Ok(center);
                    }
                } else {
                    stable_count = 0;
                }
            } else {
                stable_count = 0;
            }
            prev = Some(center);
            if tokio::time::Instant::now() >= deadline {
                // Fall back to last observed center (best-effort, never hang).
                return Ok(center);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Drive a single mouse click at the backend node's box center.
    async fn click_at_backend(
        &self,
        dom_session_id: &str,
        owner_backend: Option<i64>,
        backend: i64,
    ) -> Result<(), CoreError> {
        let (cx, cy) = self.wait_for_actionable(dom_session_id, backend).await?;
        let (ox, oy) = self.frame_offset(owner_backend).await;
        let (x, y) = (cx + ox, cy + oy);
        for ty in ["mouseMoved", "mousePressed", "mouseReleased"] {
            input::dispatch_mouse_event(
                &self.client,
                &self.session_id,
                &input::DispatchMouseEventParams {
                    type_: ty.to_string(),
                    x,
                    y,
                    button: Some("left".to_string()),
                    buttons: None,
                    click_count: Some(1),
                    delta_x: None,
                    delta_y: None,
                    modifiers: None,
                },
            )
            .await?;
        }
        Ok(())
    }

    /// Drain pending RPCs, kill Chromium (if owned), drop profile dir.
    /// Isolated context sessions dispose their browserContextId without shutting
    /// down the shared `CdpClient`. Attached sessions only disconnect.
    pub async fn shutdown(mut self) {
        if self.chrome.is_none() {
            if let Some(cid) = self.browser_context_id.take() {
                let mut cleanup = BrowserContextCleanup::new(self.client.clone(), cid);
                if let Err(error) = cleanup.dispose().await {
                    tracing::warn!(%error, "isolated browser context disposal failed; scheduling a retry");
                }
                return;
            }
        }
        self.client.shutdown();
        if let Some(mut proc) = self.chrome.take() {
            proc.shutdown().await;
        }
    }

    /// Close an isolated context and report a CDP disposal failure to its caller.
    /// If disposal fails, the cleanup guard schedules a best-effort retry.
    pub async fn close_isolated_context(mut self) -> Result<(), CoreError> {
        let Some(browser_context_id) = self.browser_context_id.take() else {
            self.shutdown().await;
            return Ok(());
        };
        let mut cleanup = BrowserContextCleanup::new(self.client.clone(), browser_context_id);
        cleanup.dispose().await?;
        Ok(())
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
mod settle_shadow_tests {
    use std::time::{Duration, Instant};

    use super::{target, BrowserContextCleanup, BrowserSession, SessionConfig};

    #[cfg(unix)]
    #[tokio::test]
    async fn screenshot_guard_disconnects_the_client_during_unwind() {
        use super::ScreenshotGuard;
        use tokio::io::duplex;

        let (client_rx, _peer_tx) = duplex(16);
        let (_peer_rx, client_tx) = duplex(16);
        let client = flashbrowse_cdp::CdpClient::connect_pipe(client_rx, client_tx)
            .await
            .expect("create an in-memory CDP client");
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = ScreenshotGuard {
                client: &client,
                armed: true,
            };
            panic!("simulate a screenshot preparation panic");
        }));

        assert!(panic.is_err());
        assert!(
            !client.is_connected(),
            "a partially prepared screenshot must invalidate its CDP session"
        );
    }

    #[test]
    fn malformed_history_index_cannot_overflow() {
        let history = super::page::GetNavigationHistoryResponse {
            current_index: usize::MAX,
            entries: Vec::new(),
        };
        assert!(super::adjacent_history_entry(&history, 1).is_none());
        assert!(super::adjacent_history_entry(&history, -1).is_none());
    }

    #[test]
    fn sha256_matches_standard_vector() {
        assert_eq!(
            super::sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[tokio::test]
    async fn dropped_browser_context_guard_schedules_context_disposal() {
        if flashbrowse_cdp::find_chrome(None).is_err() {
            return;
        }
        let session = match BrowserSession::launch(SessionConfig {
            no_sandbox: Some(true),
            ..SessionConfig::default()
        })
        .await
        {
            Ok(session) => session,
            Err(error) => {
                eprintln!("SKIP: Chrome could not launch for context cleanup test: {error}");
                return;
            }
        };
        let client = session.client();
        let browser_context_id = super::target::create_browser_context(&client)
            .await
            .expect("create isolated browser context");
        let target_id = super::target::create_target_in_context(
            &client,
            "about:blank",
            Some(&browser_context_id),
        )
        .await
        .expect("create page in isolated context")
        .target_id;

        drop(BrowserContextCleanup::new(
            client.clone(),
            browser_context_id,
        ));

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut disposed = false;
        while Instant::now() < deadline {
            let targets = super::target::get_targets(&client)
                .await
                .expect("list Chrome targets");
            if !targets
                .target_infos
                .iter()
                .any(|target| target.target_id == target_id)
            {
                disposed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        session.shutdown().await;
        assert!(disposed, "dropping the guard should dispose its context");
    }

    #[tokio::test]
    async fn externally_closed_active_page_recovers_inside_its_browser_context() {
        if flashbrowse_cdp::find_chrome(None).is_err() {
            return;
        }
        let config = SessionConfig {
            no_sandbox: Some(true),
            ..SessionConfig::default()
        };
        let default_session = match BrowserSession::launch(config.clone()).await {
            Ok(session) => session,
            Err(error) => {
                eprintln!("SKIP: Chrome could not launch for tab recovery test: {error}");
                return;
            }
        };
        let mut isolated = default_session
            .create_isolated_context(&config)
            .await
            .expect("create isolated context");
        let isolated_context = isolated
            .browser_context_id()
            .expect("isolated context id")
            .to_string();
        let closed_target = isolated.target_id().to_string();
        let isolated_client = isolated.client();

        assert!(target::close_target(&isolated_client, &closed_target)
            .await
            .expect("close active target externally"));
        let close_deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < close_deadline {
            let tabs = isolated.list_tabs().await.expect("list isolated tabs");
            if !tabs.iter().any(|tab| tab.target_id == closed_target) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        let tabs = isolated
            .recover_closed_active_tab(&config)
            .await
            .expect("recover active tab");
        assert_ne!(isolated.target_id(), closed_target);
        assert!(tabs.iter().any(|tab| tab.target_id == isolated.target_id()));
        assert!(tabs
            .iter()
            .all(|tab| tab.browser_context_id.as_deref() == Some(&isolated_context)));
        assert_eq!(
            isolated.browser_context_id(),
            Some(isolated_context.as_str())
        );

        let tree = isolated
            .open("data:text/html,%3Ctitle%3ERecovered%3C/title%3E%3Ch1%3EIsolated%20Recovery%3C/h1%3E")
            .await
            .expect("open after external tab closure");
        assert!(tree.contains("Isolated Recovery"));
        let default_tabs = default_session
            .list_tabs()
            .await
            .expect("list default context tabs");
        let all_targets = target::get_targets(&default_session.client())
            .await
            .expect("list all browser targets");
        assert!(
            !default_tabs.is_empty(),
            "default context tabs: {default_tabs:?}; all targets: {:?}",
            all_targets.target_infos
        );
        assert!(default_tabs.iter().all(|tab| {
            tab.browser_context_id.as_deref() == default_session.target_context_id.as_deref()
        }));

        isolated
            .close_isolated_context()
            .await
            .expect("close context");
        default_session.shutdown().await;
    }
}

#[cfg(test)]
mod popup_tracker_tests {
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};

    use flashbrowse_cdp::protocol::target::TargetInfo;

    use super::{record_popup_target, take_popup_from_tracker, PopupTracker};

    fn target(id: &str, kind: &str, opener: Option<&str>) -> TargetInfo {
        TargetInfo {
            target_id: id.to_string(),
            kind: kind.to_string(),
            url: format!("https://example.com/{id}"),
            title: id.to_string(),
            opener_id: opener.map(str::to_string),
            browser_context_id: None,
        }
    }

    #[test]
    fn records_newest_page_for_opener_and_ignores_non_page_and_existing() {
        let tracker: PopupTracker = Arc::new(Mutex::new(Vec::new()));
        record_popup_target(&tracker, target("sw1", "service_worker", Some("opener1")));
        record_popup_target(&tracker, target("old_tab", "page", Some("opener1")));
        record_popup_target(&tracker, target("other", "page", Some("opener2")));
        record_popup_target(&tracker, target("popup1", "page", Some("opener1")));

        let mut existing = HashSet::new();
        existing.insert("old_tab".to_string());

        let picked = take_popup_from_tracker(&tracker, "opener1", &existing).expect("popup1");
        assert_eq!(picked.target_id, "popup1");
        assert!(take_popup_from_tracker(&tracker, "opener1", &existing).is_none());
        assert!(take_popup_from_tracker(&tracker, "opener2", &existing).is_some());
    }
}
