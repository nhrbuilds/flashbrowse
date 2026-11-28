use super::*;

impl BrowserSession {
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
                    tracing::warn!(target: "flashbrowse_core::session", %cleanup_error, "isolated browser context cleanup failed after creation error");
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
}
