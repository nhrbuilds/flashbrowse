//! Per-session console log, network request/response log, and lazy Fetch route mocking.

use std::collections::VecDeque;
use std::future::Future;
use std::sync::{Arc, Mutex};

use flashbrowse_cdp::protocol::fetch;
use flashbrowse_cdp::CdpClient;
use futures_util::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const MAX_LOG_ENTRIES: usize = 200;

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

/// Receive the next broadcast item, ignoring transient lag until `stop` closes.
pub(crate) async fn recv_until_stop<T: Clone>(
    rx: &mut tokio::sync::broadcast::Receiver<T>,
    stop: &mut tokio::sync::watch::Receiver<()>,
) -> Option<T> {
    loop {
        tokio::select! {
            biased;
            _ = stop.changed() => return None,
            res = rx.recv() => match res {
                Ok(v) => return Some(v),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
                    tracing::debug!(count, "broadcast event receiver lagged");
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
            }
        }
    }
}

/// Bounded ring-buffer collector for `Runtime.consoleAPICalled` and `Runtime.exceptionThrown`.
#[derive(Debug, Clone)]
pub struct ConsoleTracker {
    entries: Arc<Mutex<VecDeque<ConsoleEntry>>>,
}

impl ConsoleTracker {
    pub fn install(
        client: &Arc<CdpClient>,
        session_id: &str,
        stop: tokio::sync::watch::Receiver<()>,
    ) -> Self {
        let entries = Arc::new(Mutex::new(VecDeque::with_capacity(64)));
        let weak_console = Arc::downgrade(&entries);
        let sid_console = session_id.to_string();
        let mut rx_console = client
            .events()
            .subscribe_envelopes("Runtime.consoleAPICalled");
        let mut stop_console = stop.clone();
        spawn_supervised(client, async move {
            while let Some(env) = recv_until_stop(&mut rx_console, &mut stop_console).await {
                if env.session_id.as_deref() != Some(sid_console.as_str()) {
                    continue;
                }
                let Some(buf) = weak_console.upgrade() else {
                    break;
                };
                let level = env
                    .params
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("log")
                    .to_string();
                let text = env
                    .params
                    .get("args")
                    .and_then(Value::as_array)
                    .map(|args| {
                        args.iter()
                            .map(|a| {
                                if let Some(s) = a.get("value").and_then(Value::as_str) {
                                    s.to_string()
                                } else if let Some(v) = a.get("value") {
                                    v.to_string()
                                } else if let Some(d) = a.get("description").and_then(Value::as_str)
                                {
                                    d.to_string()
                                } else {
                                    a.get("type")
                                        .and_then(Value::as_str)
                                        .unwrap_or("")
                                        .to_string()
                                }
                            })
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .unwrap_or_default();
                let frame = env
                    .params
                    .get("stackTrace")
                    .and_then(|st| st.get("callFrames"))
                    .and_then(Value::as_array)
                    .and_then(|arr| arr.first());
                let url = frame
                    .and_then(|f| f.get("url"))
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
                let line = frame
                    .and_then(|f| f.get("lineNumber"))
                    .and_then(Value::as_i64);
                push_bounded(
                    &buf,
                    ConsoleEntry {
                        level,
                        text,
                        url,
                        line,
                    },
                );
            }
        });

        let weak_exc = Arc::downgrade(&entries);
        let sid_exc = session_id.to_string();
        let mut rx_exc = client
            .events()
            .subscribe_envelopes("Runtime.exceptionThrown");
        let mut stop_exc = stop;
        spawn_supervised(client, async move {
            while let Some(env) = recv_until_stop(&mut rx_exc, &mut stop_exc).await {
                if env.session_id.as_deref() != Some(sid_exc.as_str()) {
                    continue;
                }
                let Some(buf) = weak_exc.upgrade() else {
                    break;
                };
                let details = env.params.get("exceptionDetails");
                let text = details
                    .and_then(|d| {
                        d.get("exception")
                            .and_then(|e| e.get("description"))
                            .and_then(Value::as_str)
                            .or_else(|| d.get("text").and_then(Value::as_str))
                    })
                    .unwrap_or("Uncaught exception")
                    .to_string();
                let url = details
                    .and_then(|d| d.get("url"))
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
                let line = details
                    .and_then(|d| d.get("lineNumber"))
                    .and_then(Value::as_i64);
                push_bounded(
                    &buf,
                    ConsoleEntry {
                        level: "exception".to_string(),
                        text,
                        url,
                        line,
                    },
                );
            }
        });

        Self { entries }
    }

    pub fn list(&self, level: Option<&str>) -> Vec<ConsoleEntry> {
        let Ok(guard) = self.entries.lock() else {
            return Vec::new();
        };
        guard
            .iter()
            .filter(|e| level.is_none_or(|want| e.level.eq_ignore_ascii_case(want)))
            .cloned()
            .collect()
    }

    pub fn clear(&self) {
        if let Ok(mut guard) = self.entries.lock() {
            guard.clear();
        }
    }
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

impl NetworkLogTracker {
    pub fn install(
        client: &Arc<CdpClient>,
        session_id: &str,
        stop: tokio::sync::watch::Receiver<()>,
    ) -> Self {
        let entries = Arc::new(Mutex::new(VecDeque::with_capacity(64)));

        let weak_req = Arc::downgrade(&entries);
        let sid_req = session_id.to_string();
        let mut rx_req = client
            .events()
            .subscribe_envelopes("Network.requestWillBeSent");
        let mut stop_req = stop.clone();
        spawn_supervised(client, async move {
            while let Some(env) = recv_until_stop(&mut rx_req, &mut stop_req).await {
                if env.session_id.as_deref() != Some(sid_req.as_str()) {
                    continue;
                }
                let Some(buf) = weak_req.upgrade() else {
                    break;
                };
                let request_id = env
                    .params
                    .get("requestId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let url = env
                    .params
                    .get("request")
                    .and_then(|r| r.get("url"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let method = env
                    .params
                    .get("request")
                    .and_then(|r| r.get("method"))
                    .and_then(Value::as_str)
                    .unwrap_or("GET")
                    .to_string();
                let resource_type = env
                    .params
                    .get("type")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                if request_id.is_empty() {
                    continue;
                }
                push_bounded(
                    &buf,
                    NetworkEntry {
                        request_id,
                        url,
                        method,
                        resource_type,
                        status: None,
                        failed: false,
                        error_text: None,
                    },
                );
            }
        });

        let weak_res = Arc::downgrade(&entries);
        let sid_res = session_id.to_string();
        let mut rx_res = client
            .events()
            .subscribe_envelopes("Network.responseReceived");
        let mut stop_res = stop.clone();
        spawn_supervised(client, async move {
            while let Some(env) = recv_until_stop(&mut rx_res, &mut stop_res).await {
                if env.session_id.as_deref() != Some(sid_res.as_str()) {
                    continue;
                }
                let Some(buf) = weak_res.upgrade() else {
                    break;
                };
                let Some(request_id) = env.params.get("requestId").and_then(Value::as_str) else {
                    continue;
                };
                let status = env
                    .params
                    .get("response")
                    .and_then(|r| r.get("status"))
                    .and_then(Value::as_u64)
                    .and_then(|n| u16::try_from(n).ok());
                if let Ok(mut guard) = buf.lock() {
                    if let Some(entry) = guard.iter_mut().rev().find(|e| e.request_id == request_id)
                    {
                        entry.status = status;
                        if status.is_some_and(|s| s >= 400) {
                            entry.failed = true;
                        }
                    }
                };
            }
        });

        let weak_fail = Arc::downgrade(&entries);
        let sid_fail = session_id.to_string();
        let mut rx_fail = client.events().subscribe_envelopes("Network.loadingFailed");
        let mut stop_fail = stop;
        spawn_supervised(client, async move {
            while let Some(env) = recv_until_stop(&mut rx_fail, &mut stop_fail).await {
                if env.session_id.as_deref() != Some(sid_fail.as_str()) {
                    continue;
                }
                let Some(buf) = weak_fail.upgrade() else {
                    break;
                };
                let Some(request_id) = env.params.get("requestId").and_then(Value::as_str) else {
                    continue;
                };
                let error_text = env
                    .params
                    .get("errorText")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                if let Ok(mut guard) = buf.lock() {
                    if let Some(entry) = guard.iter_mut().rev().find(|e| e.request_id == request_id)
                    {
                        entry.failed = true;
                        entry.error_text = error_text;
                    }
                };
            }
        });

        Self { entries }
    }

    pub fn list(&self, failed_only: bool) -> Vec<NetworkEntry> {
        let Ok(guard) = self.entries.lock() else {
            return Vec::new();
        };
        guard
            .iter()
            .filter(|e| !failed_only || e.failed)
            .cloned()
            .collect()
    }

    pub fn clear(&self) {
        if let Ok(mut guard) = self.entries.lock() {
            guard.clear();
        }
    }
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

impl RouteInterceptor {
    pub fn install(
        client: &Arc<CdpClient>,
        session_id: &str,
        mut stop: tokio::sync::watch::Receiver<()>,
    ) -> Self {
        let state = Arc::new(Mutex::new(RouteState::default()));
        let weak = Arc::downgrade(&state);
        let client_task = client.clone();
        let sid = session_id.to_string();
        let mut rx = client.events().subscribe_envelopes("Fetch.requestPaused");
        spawn_supervised(client, async move {
            loop {
                let env = tokio::select! {
                    biased;
                    _ = stop.changed() => break,
                    result = rx.recv() => match result {
                        Ok(env) => env,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
                            tracing::error!(
                                count,
                                session_id = %sid,
                                "Fetch event queue overflowed; disabling interception for this target"
                            );
                            if let Some(state) = weak.upgrade() {
                                if let Ok(mut state) = state.lock() {
                                    state.failed = true;
                                    state.rules.clear();
                                }
                            }
                            // Fetch.disable destroys Chromium's interceptor for this target.
                            // Chromium detaches unresolved interception jobs by continuing their
                            // requests, including jobs whose requestPaused event was lost.
                            if let Err(error) = fetch::disable(&client_task, &sid).await {
                                tracing::error!(
                                    %error,
                                    session_id = %sid,
                                    "could not disable Fetch after event loss; the browser session may need recovery"
                                );
                            }
                            continue;
                        }
                    }
                };
                if env.session_id.as_deref() != Some(sid.as_str()) {
                    continue;
                }
                let Some(state_arc) = weak.upgrade() else {
                    break;
                };
                if state_arc.lock().map(|state| state.failed).unwrap_or(true) {
                    continue;
                }
                let Some(req_id) = env
                    .params
                    .get("requestId")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                else {
                    continue;
                };
                let url = env
                    .params
                    .get("request")
                    .and_then(|r| r.get("url"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let matched = state_arc.lock().ok().and_then(|state| {
                    state
                        .rules
                        .iter()
                        .find(|rule| url_matches(&rule.url_pattern, url))
                        .cloned()
                });
                match matched {
                    Some(rule) if rule.abort => {
                        if let Err(err) =
                            fetch::fail_request(&client_task, &sid, &req_id, "BlockedByClient")
                                .await
                        {
                            tracing::warn!(
                                %err,
                                req_id,
                                "Fetch.failRequest failed; falling back to continueRequest"
                            );
                            let _ = fetch::continue_request(&client_task, &sid, &req_id).await;
                        }
                    }
                    Some(rule) => {
                        let headers = vec![("Content-Type".to_string(), rule.content_type)];
                        if let Err(err) = fetch::fulfill_request(
                            &client_task,
                            &sid,
                            &req_id,
                            rule.status,
                            &headers,
                            rule.body.as_bytes(),
                        )
                        .await
                        {
                            tracing::warn!(
                                %err,
                                req_id,
                                "Fetch.fulfillRequest failed; falling back to continueRequest"
                            );
                            let _ = fetch::continue_request(&client_task, &sid, &req_id).await;
                        }
                    }
                    None => {
                        if let Err(err) = fetch::continue_request(&client_task, &sid, &req_id).await
                        {
                            tracing::debug!(%err, req_id, "Fetch.continueRequest failed");
                        }
                    }
                }
            }
        });
        Self { state }
    }

    pub async fn add_rule(
        &self,
        client: &CdpClient,
        session_id: &str,
        rule: RouteRule,
    ) -> Result<(), flashbrowse_cdp::CdpError> {
        if !(100..=599).contains(&rule.status) {
            return Err(flashbrowse_cdp::CdpError::ProtocolError {
                method: "RouteInterceptor.addRule".to_string(),
                code: -32602,
                message: format!("invalid route status {} (expected 100..=599)", rule.status),
            });
        }
        let ct = rule.content_type.trim();
        if ct.is_empty() || !ct.is_ascii() || ct.bytes().any(|b| b.is_ascii_control()) {
            return Err(flashbrowse_cdp::CdpError::ProtocolError {
                method: "RouteInterceptor.addRule".to_string(),
                code: -32602,
                message: "route content_type must be non-empty ASCII without control characters"
                    .to_string(),
            });
        }
        let was_empty = if let Ok(mut state) = self.state.lock() {
            if state.failed {
                return Err(flashbrowse_cdp::CdpError::ProtocolError {
                    method: "RouteInterceptor.addRule".to_string(),
                    code: -1,
                    message: "request interception was disabled after losing Fetch events; recreate the browser session before adding routes".to_string(),
                });
            }
            let empty = state.rules.is_empty();
            state.rules.push(rule);
            empty
        } else {
            return Ok(());
        };
        if was_empty {
            if let Err(err) = fetch::enable(client, session_id).await {
                if let Ok(mut state) = self.state.lock() {
                    state.rules.pop();
                }
                return Err(err);
            }
        }
        Ok(())
    }

    pub fn list_rules(&self) -> Vec<RouteRule> {
        self.state
            .lock()
            .map(|state| state.rules.clone())
            .unwrap_or_default()
    }

    pub async fn clear_rules(
        &self,
        client: &CdpClient,
        session_id: &str,
    ) -> Result<(), flashbrowse_cdp::CdpError> {
        let should_disable = self
            .state
            .lock()
            .map(|mut state| {
                let should_disable = !state.rules.is_empty() || state.failed;
                state.rules.clear();
                should_disable
            })
            .unwrap_or(false);
        if should_disable {
            fetch::disable(client, session_id).await?;
        }
        Ok(())
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
mod tests {
    use super::url_matches;

    #[cfg(unix)]
    #[tokio::test]
    async fn fetch_event_lag_disables_interception_for_only_its_target() {
        use super::{RouteInterceptor, RouteRule};
        use std::sync::Arc;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        async fn read_request(
            reader: &mut BufReader<tokio::io::DuplexStream>,
        ) -> serde_json::Value {
            let mut frame = Vec::new();
            tokio::time::timeout(
                std::time::Duration::from_secs(2),
                reader.read_until(0, &mut frame),
            )
            .await
            .expect("CDP request did not arrive")
            .expect("CDP pipe read failed");
            assert_eq!(frame.pop(), Some(0));
            serde_json::from_slice(&frame).expect("valid CDP request")
        }

        async fn respond(writer: &mut tokio::io::DuplexStream, request: &serde_json::Value) {
            let response = serde_json::json!({
                "id": request["id"],
                "sessionId": request["sessionId"],
                "result": {}
            });
            writer
                .write_all(response.to_string().as_bytes())
                .await
                .expect("write CDP response");
            writer.write_all(b"\0").await.expect("terminate CDP frame");
        }

        let (client_writer, peer_reader) = tokio::io::duplex(64 * 1024);
        let (mut peer_writer, client_reader) = tokio::io::duplex(64 * 1024);
        let client = Arc::new(
            flashbrowse_cdp::CdpClient::connect_pipe(client_reader, client_writer)
                .await
                .expect("connect test CDP pipe"),
        );
        let mut peer_reader = BufReader::new(peer_reader);
        let (_stop_tx, stop_rx) = tokio::sync::watch::channel(());
        let routes = RouteInterceptor::install(&client, "session-a", stop_rx);

        let routes_for_add = routes.clone();
        let client_for_add = client.clone();
        let add_rule = tokio::spawn(async move {
            routes_for_add
                .add_rule(
                    &client_for_add,
                    "session-a",
                    RouteRule {
                        url_pattern: "never-match".to_string(),
                        status: 200,
                        content_type: "text/plain".to_string(),
                        body: "unused".to_string(),
                        abort: false,
                    },
                )
                .await
        });
        let enable = read_request(&mut peer_reader).await;
        assert_eq!(enable["method"], "Fetch.enable");
        assert_eq!(enable["sessionId"], "session-a");
        respond(&mut peer_writer, &enable).await;
        add_rule
            .await
            .expect("add route task")
            .expect("enable route rule");

        let emit_request = |id: usize| {
            client.events().emit_with_session(
                "Fetch.requestPaused",
                Some("session-a".to_string()),
                serde_json::json!({
                    "requestId": format!("request-{id}"),
                    "request": {"url": "https://example.test/resource"}
                }),
            );
        };

        emit_request(0);
        let first_continue = read_request(&mut peer_reader).await;
        assert_eq!(first_continue["method"], "Fetch.continueRequest");
        assert_eq!(first_continue["sessionId"], "session-a");

        // Keep the worker waiting for its first continuation response while
        // overflowing the bounded broadcast queue.
        for id in 1..=257 {
            emit_request(id);
        }
        respond(&mut peer_writer, &first_continue).await;

        let disable = read_request(&mut peer_reader).await;
        assert_eq!(disable["method"], "Fetch.disable");
        assert_eq!(disable["sessionId"], "session-a");
        respond(&mut peer_writer, &disable).await;

        assert!(routes.list_rules().is_empty());
        assert!(routes.state.lock().expect("route state lock").failed);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn event_worker_panic_disconnects_session() {
        use std::sync::Arc;
        let (writer, _peer_reader) = tokio::io::duplex(128);
        let (_peer_writer, reader) = tokio::io::duplex(128);
        let client = Arc::new(
            flashbrowse_cdp::CdpClient::connect_pipe(reader, writer)
                .await
                .unwrap(),
        );
        super::spawn_supervised(&client, async { panic!("test event worker panic") });
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while client.is_connected() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[test]
    fn url_wildcard_and_substring_matching() {
        assert!(!url_matches("", "http://127.0.0.1:8080/api/user"));
        assert!(url_matches(
            "/api/user",
            "http://127.0.0.1:8080/api/user?id=1"
        ));
        assert!(url_matches("*/api/*", "http://127.0.0.1:8080/api/user"));
        assert!(!url_matches(
            "*/api/*.json",
            "http://127.0.0.1:8080/api/user.xml"
        ));
        assert!(url_matches(
            "*/api/*.json",
            "http://127.0.0.1:8080/api/user.json"
        ));
        assert!(url_matches("*a*b", "http://127.0.0.1/a_first_b_second_ab"));
    }
}
