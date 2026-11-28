use super::*;

impl BrowserSession {
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

    pub(super) fn frame_for_action(&self, action_id: u32) -> (String, Option<i64>) {
        match self.action_sessions.get(&action_id) {
            Some(fs) => (fs.session_id.clone(), fs.owner_backend),
            None => (self.session_id.clone(), None),
        }
    }

    pub(super) async fn frame_offset(&self, owner_backend: Option<i64>) -> (f64, f64) {
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
            tracing::warn!(target: "flashbrowse_core::session", backend, "scrollIntoViewIfNeeded failed (proceeding): {e}");
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
            tracing::warn!(target: "flashbrowse_core::session", action_id, text, outcome, "select failed");
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
                tracing::warn!(target: "flashbrowse_core::session", backend, "scrollIntoViewIfNeeded failed (proceeding): {e}");
            }
            let resp = match dom::get_box_model(&self.client, dom_session_id, backend).await {
                Ok(r) => r,
                Err(e) => {
                    // Transient: node not in the layout tree yet.
                    tracing::warn!(target: "flashbrowse_core::session", backend, "getBoxModel failed (retrying): {e}");
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
}
