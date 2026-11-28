use super::*;

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
                            tracing::error!(target: "flashbrowse_core::observability_session", count,
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
                                tracing::error!(target: "flashbrowse_core::observability_session", %error,
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
                            tracing::warn!(target: "flashbrowse_core::observability_session", %err,
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
                            tracing::warn!(target: "flashbrowse_core::observability_session", %err,
                                req_id,
                                "Fetch.fulfillRequest failed; falling back to continueRequest"
                            );
                            let _ = fetch::continue_request(&client_task, &sid, &req_id).await;
                        }
                    }
                    None => {
                        if let Err(err) = fetch::continue_request(&client_task, &sid, &req_id).await
                        {
                            tracing::debug!(target: "flashbrowse_core::observability_session", %err, req_id, "Fetch.continueRequest failed");
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
