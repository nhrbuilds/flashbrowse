use flashbrowse_cdp::events::SessionEventObserver;
use flashbrowse_cdp::EventEmitter;

use super::*;

const REQUEST: &str = "Network.requestWillBeSent";
const RESPONSE: &str = "Network.responseReceived";
const FAILED: &str = "Network.loadingFailed";

/// Bounded ring buffer of network events for one CDP session.
#[derive(Debug, Clone)]
pub struct NetworkLogTracker {
    entries: Arc<Mutex<VecDeque<NetworkEntry>>>,
    _observer: Arc<SessionEventObserver>,
}

impl NetworkLogTracker {
    pub fn install(
        client: &Arc<CdpClient>,
        session_id: &str,
        stop: tokio::sync::watch::Receiver<()>,
    ) -> Self {
        Self::observe(&client.events(), session_id, stop)
    }

    fn observe(
        events: &EventEmitter,
        session_id: &str,
        stop: tokio::sync::watch::Receiver<()>,
    ) -> Self {
        let entries = Arc::new(Mutex::new(VecDeque::with_capacity(64)));
        let observed_entries = entries.clone();
        // Record on the CDP reader so request and response events cannot be
        // reordered by task scheduling or dropped by a lagging subscriber.
        let observer = events.observe_session_events(
            session_id,
            &[REQUEST, RESPONSE, FAILED],
            move |method, params| {
                if matches!(stop.has_changed(), Ok(false)) {
                    record_event(&observed_entries, method, params);
                }
            },
        );
        Self {
            entries,
            _observer: observer,
        }
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

fn record_event(entries: &Mutex<VecDeque<NetworkEntry>>, event: &str, params: &Value) {
    let Some(request_id) = params
        .get("requestId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
    else {
        return;
    };

    if event == REQUEST {
        let request = params.get("request");
        push_bounded(
            entries,
            NetworkEntry {
                request_id: request_id.to_string(),
                url: request
                    .and_then(|req| req.get("url"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                method: request
                    .and_then(|req| req.get("method"))
                    .and_then(Value::as_str)
                    .unwrap_or("GET")
                    .to_string(),
                resource_type: params
                    .get("type")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                status: None,
                failed: false,
                error_text: None,
            },
        );
        return;
    }

    let Ok(mut entries) = entries.lock() else {
        return;
    };
    // Redirects reuse request IDs. An update belongs to the latest hop.
    let Some(entry) = entries
        .iter_mut()
        .rev()
        .find(|e| e.request_id == request_id)
    else {
        return;
    };
    match event {
        RESPONSE => {
            if let Some(status) = params
                .get("response")
                .and_then(|response| response.get("status"))
                .and_then(Value::as_u64)
                .and_then(|n| u16::try_from(n).ok())
            {
                entry.status = Some(status);
                entry.failed |= status >= 400;
            }
        }
        FAILED => {
            entry.failed = true;
            entry.error_text = params
                .get("errorText")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn network_log_records_a_burst_in_wire_order_without_a_worker() {
        let events = EventEmitter::new();
        let (stop, stop_rx) = tokio::sync::watch::channel(());
        let tracker = NetworkLogTracker::observe(&events, "target-a", stop_rx);
        let emit = |method, session: &str, params| {
            events.emit_with_session(method, Some(session.to_string()), params);
        };

        // More events than both the log capacity and the CDP broadcast capacity.
        for index in 0..300 {
            let id = index.to_string();
            emit(REQUEST, "target-a", json!({"requestId": id}));
            emit(
                RESPONSE,
                "target-a",
                json!({"requestId": id, "response": {"status": 200}}),
            );
        }
        emit(
            REQUEST,
            "target-a",
            json!({"requestId": "http", "request": {"url": "/redirect"}}),
        );
        emit(
            REQUEST,
            "target-a",
            json!({"requestId": "http", "request": {"url": "/api/fail"}}),
        );
        emit(
            RESPONSE,
            "target-a",
            json!({"requestId": "http", "response": {"status": 500}}),
        );
        emit(
            FAILED,
            "target-b",
            json!({"requestId": "http", "errorText": "wrong session"}),
        );
        emit(REQUEST, "target-a", json!({"requestId": "transport"}));
        emit(
            FAILED,
            "target-a",
            json!({"requestId": "transport", "errorText": "net::ERR_FAILED"}),
        );

        let all = tracker.list(false);
        assert_eq!(all.len(), MAX_LOG_ENTRIES);
        assert_eq!(all[0].request_id, "103");
        assert!(all[..197].iter().all(|entry| entry.status == Some(200)));
        let failed = tracker.list(true);
        assert_eq!(failed.len(), 2);
        assert_eq!(failed[0].url, "/api/fail");
        assert_eq!(failed[0].status, Some(500));
        assert_eq!(failed[0].error_text, None);
        assert_eq!(failed[1].status, None);
        assert_eq!(failed[1].error_text.as_deref(), Some("net::ERR_FAILED"));

        tracker.clear();
        emit(
            RESPONSE,
            "target-a",
            json!({"requestId": "http", "response": {"status": 500}}),
        );
        assert!(tracker.list(false).is_empty());
        drop(stop);
        emit(REQUEST, "target-a", json!({"requestId": "stopped"}));
        assert!(tracker.list(false).is_empty());
    }
}
