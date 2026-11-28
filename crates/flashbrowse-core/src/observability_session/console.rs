use super::*;

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
                    tracing::debug!(target: "flashbrowse_core::observability_session", count, "broadcast event receiver lagged");
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
            }
        }
    }
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
