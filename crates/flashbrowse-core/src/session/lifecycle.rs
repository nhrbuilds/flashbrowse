use super::*;

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

    pub(super) async fn attach_with_client(
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

    pub(super) async fn wait_for_popup_settle(&self) -> Result<(), CoreError> {
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

    /// Drain pending RPCs, kill Chromium (if owned), drop profile dir.
    /// Isolated context sessions dispose their browserContextId without shutting
    /// down the shared `CdpClient`. Attached sessions only disconnect.
    pub async fn shutdown(mut self) {
        if self.chrome.is_none() {
            if let Some(cid) = self.browser_context_id.take() {
                let mut cleanup = BrowserContextCleanup::new(self.client.clone(), cid);
                if let Err(error) = cleanup.dispose().await {
                    tracing::warn!(target: "flashbrowse_core::session", %error, "isolated browser context disposal failed; scheduling a retry");
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
