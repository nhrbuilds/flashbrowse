use super::*;

impl BrowserSession {
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
}
