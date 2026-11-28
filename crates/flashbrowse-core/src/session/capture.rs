use super::*;

impl BrowserSession {
    /// Extract clean Markdown (boss-level `browser_extract_markdown`).
    pub async fn extract_markdown(&self) -> Result<String, CoreError> {
        let resp = runtime::evaluate(&self.client, &self.session_id, EXTRACT_SCRIPT).await?;
        if let Some(details) = resp.exception_details {
            return Err(CoreError::NavigationFailed(format!(
                "page extraction failed: {details}"
            )));
        }
        if let Some(v) = resp.result.value {
            match v {
                Value::String(s) => return Ok(s),
                Value::Object(obj) => {
                    if let Some(Value::String(md)) = obj.get("markdown") {
                        return Ok(md.clone());
                    }
                    return Ok(serde_json::to_string(&Value::Object(obj))?);
                }
                other => return Ok(other.to_string()),
            }
        }
        Ok(String::new())
    }

    /// Capture PNG screenshot (base64). See [`ScreenshotOptions`].
    ///
    /// In agent-default mode, font and image blocks are lifted for the prepare
    /// window, then restored. [`ResourcePolicy::FullRender`] skips that dance.
    ///
    /// Lifting the blocklist alone is **not** enough: `Network.setBlockedURLs`
    /// only affects *future* requests, and Chrome never retries a request that
    /// was already blocked. A blocked `<img>` therefore stays broken forever.
    /// So the prepare window also re-requests the images (see
    /// [`Self::reload_blocked_images`]), which is what actually makes pixels
    /// appear.
    pub async fn screenshot(&self, opts: ScreenshotOptions) -> Result<String, CoreError> {
        flashbrowse_cdp::runtime::run(self.screenshot_inner(opts)).await?
    }

    async fn screenshot_inner(&self, opts: ScreenshotOptions) -> Result<String, CoreError> {
        // Validate action_id before lifting resource blocks or mutating page state.
        if let Some(action_id) = opts.action_id {
            if !self.action_map.contains_key(&action_id) {
                return Err(CoreError::ActionNotFound(action_id));
            }
        }
        let mut guard = ScreenshotGuard {
            client: &self.client,
            armed: true,
        };
        let needs_lift = self.resource_policy != ResourcePolicy::FullRender;
        if needs_lift {
            self.driver
                .set_blocklist(blocked_patterns_for_screenshot(self.resource_policy))
                .await?;
        }
        // Re-request images that were blocked during the page load. Without
        // this the capture shows broken-image glyphs, no matter how long we
        // wait for them.
        if needs_lift {
            self.reload_blocked_images().await?;
        }
        if opts.prepare_lazy_content && opts.action_id.is_none() {
            self.prepare_lazy_content_for_screenshot(opts.full_page)
                .await?;
        }
        self.wait_for_visual_ready(opts.timeout_ms).await?;
        let capture_res = async {
            let clip = if let Some(action_id) = opts.action_id {
                let backend = *self
                    .action_map
                    .get(&action_id)
                    .ok_or(CoreError::ActionNotFound(action_id))?;
                let (dom_sid, owner) = self.frame_for_action(action_id);
                let _ = dom::scroll_into_view_if_needed(&self.client, &dom_sid, backend).await;
                let resp = dom::get_box_model(&self.client, &dom_sid, backend).await?;
                let quad = if resp.model.border.len() >= 8 {
                    &resp.model.border
                } else {
                    &resp.model.content
                };
                let [x1, y1, x2, y2, x3, y3, x4, y4, ..] = quad.as_slice() else {
                    return Err(CoreError::NoBoxModel(action_id));
                };
                let (ox, oy) = self.frame_offset(owner).await;
                let xs = [*x1, *x2, *x3, *x4];
                let ys = [*y1, *y2, *y3, *y4];
                let min_x = xs.into_iter().fold(f64::INFINITY, f64::min) + ox;
                let max_x = xs.into_iter().fold(f64::NEG_INFINITY, f64::max) + ox;
                let min_y = ys.into_iter().fold(f64::INFINITY, f64::min) + oy;
                let max_y = ys.into_iter().fold(f64::NEG_INFINITY, f64::max) + oy;
                let width = (max_x - min_x).max(1.0);
                let height = (max_y - min_y).max(1.0);
                Some(page::Viewport {
                    x: min_x.max(0.0),
                    y: min_y.max(0.0),
                    width,
                    height,
                    scale: 1.0,
                })
            } else {
                None
            };
            self.driver
                .capture_screenshot_clipped(opts.full_page && clip.is_none(), clip)
                .await
        }
        .await;
        if needs_lift {
            self.driver
                .set_resource_policy(self.resource_policy)
                .await?;
        }
        guard.armed = false;
        capture_res
    }

    /// Apply viewport, timezone, locale, and/or geolocation emulation overrides.
    pub async fn emulate(&self, opts: EmulationOptions) -> Result<(), CoreError> {
        if let Some(vp) = opts.viewport {
            emulation::set_device_metrics_override(
                &self.client,
                &self.session_id,
                &emulation::DeviceMetricsParams {
                    width: vp.width,
                    height: vp.height,
                    device_scale_factor: vp.device_scale_factor,
                    mobile: vp.mobile,
                },
            )
            .await?;
        }
        if let Some(tz) = opts.timezone {
            emulation::set_timezone_override(&self.client, &self.session_id, &tz).await?;
        }
        if let Some(loc) = opts.locale {
            emulation::set_locale_override(&self.client, &self.session_id, &loc).await?;
        }
        if let Some(geo) = opts.geolocation {
            emulation::set_geolocation_override(
                &self.client,
                &self.session_id,
                geo.latitude,
                geo.longitude,
                geo.accuracy,
            )
            .await?;
        }
        Ok(())
    }

    /// Clear device metrics, timezone, locale, geolocation, and permission overrides.
    pub async fn reset_emulation(&self) -> Result<(), CoreError> {
        let mut first_err: Option<CoreError> = None;
        let mut remember = |result: Result<(), CdpError>| {
            if let Err(err) = result {
                first_err.get_or_insert(err.into());
            }
        };
        remember(emulation::clear_device_metrics_override(&self.client, &self.session_id).await);
        remember(emulation::set_timezone_override(&self.client, &self.session_id, "").await);
        remember(emulation::set_locale_override(&self.client, &self.session_id, "").await);
        remember(emulation::clear_geolocation_override(&self.client, &self.session_id).await);
        remember(
            browser::reset_permissions(&self.client, self.browser_context_id.as_deref()).await,
        );
        match first_err {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    /// Grant browser permissions (e.g. `geolocation`, `notifications`, `clipboardReadWrite`).
    pub async fn grant_permissions(
        &self,
        origin: Option<&str>,
        permissions: &[String],
    ) -> Result<(), CoreError> {
        browser::grant_permissions(
            &self.client,
            origin,
            permissions,
            self.browser_context_id.as_deref(),
        )
        .await?;
        Ok(())
    }

    /// Render the current page to PDF bytes via `Page.printToPDF`.
    pub async fn print_to_pdf(&self, opts: PdfOptions) -> Result<Vec<u8>, CoreError> {
        let safe_scale = if opts.scale.is_finite() {
            opts.scale.clamp(0.1, 2.0)
        } else {
            1.0
        };
        let resp = page::print_to_pdf(
            &self.client,
            &self.session_id,
            &page::PrintToPdfParams {
                landscape: Some(opts.landscape),
                print_background: Some(opts.print_background),
                scale: Some(safe_scale),
                paper_width: None,
                paper_height: None,
            },
        )
        .await?;
        fetch::decode_base64(&resp.data).map_err(CoreError::NavigationFailed)
    }

    /// Convenience wrapper (viewport, default prepare/timeout).
    pub async fn screenshot_viewport(&self) -> Result<String, CoreError> {
        self.screenshot(ScreenshotOptions::default()).await
    }

    /// Re-request images that the network layer blocked during page load.
    ///
    /// `Network.setBlockedURLs` prevents the *request*; it does not prevent
    /// the `<img>` element from existing, and Chrome does not retry a blocked
    /// request on its own. Re-assigning the source issues a fresh request,
    /// which now succeeds because the blocklist has been lifted.
    ///
    /// Images that already loaded are left alone (`complete && naturalWidth`),
    /// so this never re-fetches a working image. Inline `data:` URIs are
    /// never blocked, so they are skipped too.
    async fn reload_blocked_images(&self) -> Result<(), CoreError> {
        // Bounded: a pathological page with thousands of images must not
        // turn one screenshot into thousands of requests.
        const MAX_RETRY: usize = 200;
        let expr = format!(
            r#"(function(){{
      const imgs = Array.from(document.images || []);
      let n = 0;
      for (const img of imgs) {{
        if (n >= {MAX_RETRY}) break;
        if (img.complete && img.naturalWidth > 0) continue; // already loaded
        const src = img.currentSrc || img.src;
        if (!src || src.indexOf('data:') === 0) continue;   // inline data URI
        if (img.srcset) {{
          // Re-trigger the srcset selection rather than pinning one candidate.
          const s = img.srcset;
          img.srcset = '';
          img.srcset = s;
        }} else {{
          img.src = '';
          img.src = src;
        }}
        n++;
      }}
      return n;
    }})()"#
        );
        if let Value::Number(n) = self.driver.evaluate(&expr).await? {
            let count = n.as_u64().unwrap_or(0);
            if count > 0 {
                tracing::debug!(target: "flashbrowse_core::session", count, "re-requested blocked images");
            }
        }
        Ok(())
    }

    /// Surface lazy-loaded content before a capture.
    ///
    /// The page is scrolled through so that `loading="lazy"` images start
    /// fetching, then the agent's original scroll position is restored. The
    /// restore matters: the agent asked for a picture of what it is currently
    /// looking at, so snapping the viewport back to the top would destroy
    /// that context.
    async fn prepare_lazy_content_for_screenshot(&self, full_page: bool) -> Result<(), CoreError> {
        // Remember where the agent was. The value is held here (not on
        // `window`) so nothing is left behind in the page's global scope.
        let saved_y = self
            .driver
            .evaluate("window.scrollY")
            .await?
            .as_f64()
            .ok_or_else(|| {
                CoreError::NavigationFailed("could not read the page scroll position".to_string())
            })?;
        let target = if full_page { 0.0 } else { saved_y };
        let script = format!(
            r#"(function(){{
      try {{
      const h = Math.max(document.body.scrollHeight, document.documentElement.scrollHeight);
      const step = Math.max(400, window.innerHeight);
      for (let y = 0; y < h; y += step) {{ window.scrollTo(0, y); }}
      return true;
      }} finally {{
        const key = Symbol.for('flashbrowse.screenshot.scroll.restore');
        window[key] = setTimeout(() => {{ delete window[key]; window.scrollTo(0, {target}); }}, 150);
      }}
    }})()"#
        );
        self.driver.evaluate(&script).await?;
        // Let the page restore its viewport before preparing the capture.
        tokio::time::sleep(Duration::from_millis(150)).await;
        self.driver
                .evaluate(&format!("(() => {{ const key = Symbol.for('flashbrowse.screenshot.scroll.restore'); clearTimeout(window[key]); delete window[key]; window.scrollTo(0, {target}); return true; }})()"))
                .await?;
        Ok(())
    }

    async fn wait_for_visual_ready(&self, timeout_ms: u64) -> Result<(), CoreError> {
        let budget = timeout_ms.clamp(500, SCREENSHOT_PREPARE_MS_MAX);
        let deadline = {
            let now = tokio::time::Instant::now();
            now.checked_add(Duration::from_millis(budget))
                .unwrap_or(now)
        };
        let quiet_ms = 350u64;
        loop {
            if tokio::time::Instant::now() >= deadline {
                tracing::warn!(target: "flashbrowse_core::session", "screenshot prepare deadline reached - capturing best effort");
                break;
            }
            let net_idle = self.net.idle(quiet_ms);
            let images_ok = self.images_ready().await?;
            if net_idle && images_ok {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Ok(())
    }

    /// True when no image is still pending.
    ///
    /// `img.complete` is the readiness signal: it is true once the image has
    /// loaded *or* failed. Requiring `naturalWidth > 0` as well would mean a
    /// permanently broken image (blocked, 404, unsupported format) keeps the
    /// wait loop spinning until the deadline - every screenshot of a page with
    /// one bad image would cost the full timeout.
    async fn images_ready(&self) -> Result<bool, CoreError> {
        let expr = r#"(function(){
      const imgs = Array.from(document.images || []);
      if (imgs.length === 0) return true;
      return imgs.every(function(img){
        if (!img.src) return true;
        return img.complete;
      });
    })()"#;
        self.driver.evaluate(expr).await?.as_bool().ok_or_else(|| {
            CoreError::NavigationFailed("could not check image readiness".to_string())
        })
    }
}
