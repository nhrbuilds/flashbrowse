use super::*;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Tracks in-flight network requests so the settle can wait for the page to
/// actually finish loading (an SPA can be quiet in the DOM while its JS
/// bundle is still downloading).
#[derive(Debug, Clone, Default)]
pub(super) struct NetActivity {
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
    pub(super) fn new() -> Self {
        Self {
            inflight: Arc::new(AtomicUsize::new(0)),
            last_activity_ms: Arc::new(AtomicU64::new(0)),
            started_at_ms: Arc::new(AtomicU64::new(0)),
        }
    }

    fn touch(&self) {
        self.last_activity_ms.store(now_ms(), Ordering::Relaxed);
    }

    pub(super) fn reset(&self) {
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
    pub(super) fn idle(&self, quiet_ms: u64) -> bool {
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
    pub(super) fn attach(
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
                    tracing::debug!(target: "flashbrowse_core::session", url, "ignoring favicon request for network-idle tracking");
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

impl BrowserSession {
    /// `Title`/`URL` header fields via in-page evaluation (best-effort).
    pub(super) async fn refresh_title_url(&mut self) {
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
}
