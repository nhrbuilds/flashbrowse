use super::*;

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
