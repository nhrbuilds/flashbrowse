use super::*;

/// Legacy settle result kept while the candidate is measured in shadow mode.
struct SettleSnapshot {
    nodes: Vec<crate::tree::SemanticNode>,
    map: ActionMap,
    action_sessions: HashMap<u32, FrameSession>,
    raw_nodes: Vec<flashbrowse_cdp::protocol::accessibility::AXNode>,
}

impl BrowserSession {
    /// Navigate and return the compressed, numbered action tree.
    ///
    /// Return a tree only after the current navigation has a stable,
    /// non-empty accessibility snapshot. The settle loop checks document
    /// readiness, DOM quiet time, and a semantic AX fingerprint. If it cannot
    /// verify stability within 15 seconds, it returns an error and invalidates
    /// the action map instead of exposing the latest partial snapshot.
    pub async fn open(&mut self, url: &str) -> Result<String, CoreError> {
        self.open_with_raw_snapshot(url)
            .await
            .map(|(rendered, _)| rendered)
    }

    /// Open a URL and return the raw AX snapshot used for the authoritative
    /// rendered tree. This is intended for fidelity diagnostics that must
    /// compare both representations from the same browser observation.
    pub async fn open_with_raw_snapshot(
        &mut self,
        url: &str,
    ) -> Result<
        (
            String,
            Vec<flashbrowse_cdp::protocol::accessibility::AXNode>,
        ),
        CoreError,
    > {
        flashbrowse_cdp::runtime::run(self.open_with_raw_snapshot_inner(url)).await?
    }

    async fn open_with_raw_snapshot_inner(
        &mut self,
        url: &str,
    ) -> Result<
        (
            String,
            Vec<flashbrowse_cdp::protocol::accessibility::AXNode>,
        ),
        CoreError,
    > {
        let t_open = std::time::Instant::now();
        let shadow_enabled = settle_shadow_enabled();
        // Subscribe before navigating: a broadcast channel only delivers
        // events emitted after `subscribe`, so this cannot miss this
        // navigation's AX completion. `Accessibility.enable` is already on
        // (see `launch`/`attach`), so the event stream exists at zero cost.
        let mut ax_complete_rx = self
            .client
            .events()
            .subscribe_envelopes("Accessibility.loadComplete");
        // `Page.navigate` returns before the new document commits. Until the
        // commit event for THIS navigation arrives, the DOM, the readyState
        // and the AX tree all still belong to the previous page, so observing
        // them would look like a page that changed the instant it loaded.
        let mut lifecycle_rx = self.client.events().subscribe("Page.lifecycleEvent");
        let t_nav = std::time::Instant::now();
        let nav = page::navigate(&self.client, &self.session_id, url).await?;
        let nav_ms = t_nav.elapsed().as_millis() as u64;
        let nav_loader = nav.loader_id.clone();
        // NOTE: we deliberately do NOT wait for the `load` event - it waits
        // for every subresource (8+ s on heavy pages) while the AX tree is
        // usually final much earlier. Readiness is decided by the settle loop
        // below (document parsed + AX tree complete vs DOM + tree quiet).

        self.url = url.to_string();
        let client_c = self.client.clone();
        let sid = self.session_id.clone();
        let t_titles = std::time::Instant::now();
        let (title_r, url_r) = tokio::join!(
            runtime::evaluate(&client_c, &sid, "document.title"),
            runtime::evaluate(&client_c, &sid, "location.href")
        );
        if let Ok(r) = title_r {
            if let Some(Value::String(s)) = r.result.value {
                if !s.is_empty() {
                    self.title = s;
                }
            }
        }
        if let Ok(r) = url_r {
            if let Some(Value::String(s)) = r.result.value {
                if !s.is_empty() {
                    self.url = s;
                }
            }
        }
        let mut title_url_ms = t_titles.elapsed().as_millis() as u64;
        // Chrome error pages (network lost, DNS failure, ...) look like a
        // tiny page. Report them instead of returning a truncated tree.
        if self.url.starts_with("chrome-error://") {
            self.invalidate_tree();
            return Err(CoreError::NavigationFailed(self.url.clone()));
        }
        // The AX tree is fetched inside the settle loop (the tree right
        // after `navigate` belongs to the previous page - fetching it twice
        // wasted ~200 KB of JSON per open).
        let mut raw_ax_nodes = 0usize;
        let mut nodes: Vec<crate::tree::SemanticNode> = Vec::new();
        let mut map = ActionMap::new();
        let mut action_sessions: HashMap<u32, FrameSession> = HashMap::new();
        let mut fingerprint = 0u64;
        // Separate stage timers are reported through
        // `SettleBreakdown` so a speedup claim can be checked against evidence.
        let mut poll_ms = 0u64;
        let mut sleep_ms = 0u64;
        let mut ax_fetch_ms = 0u64;
        let mut compress_ms = 0u64;
        let mut iterations = 0u64;
        let mut ax_fetches = 0u64;
        let mut exit_reason = "deadline";
        let mut last_raw_nodes = Vec::new();
        let mut mutation_count = None;
        let mut mutation_last_ms = None;
        let mut mutation_quiet_ms = None;
        let mut mutation_would_delay_candidate = None;
        let mut candidate_accept_ms = None;
        let mut legacy_snapshot: Option<SettleSnapshot> = None;
        let mut candidate_nodes: Option<Vec<crate::tree::SemanticNode>> = None;

        // Settle loop.
        //
        // Two-level strategy (fast AND complete):
        //  1. Cheap poll every 100 ms - one `Runtime.evaluate` returning
        //     `readyState|domCount|textLength`. If the DOM is still moving,
        //     the AX tree cannot be final.
        //  2. Once the cheap metrics have been quiet for `QUIET_MS`, fetch
        //     the AX tree once more and compare the content fingerprint.
        //     Only an identical fingerprint ends the wait.
        //
        // Why not just poll the AX tree? Each `getFullAXTree` moves ~200 KB
        // of JSON; three polls cost more than the whole cheap-poll budget.
        //
        // Why the fingerprint and not the node count? Chromium upgrades table
        // semantics after layout (LayoutTableCell -> Link) without changing
        // the node count.
        // Adaptive quiet window: a page that never changed (server-rendered)
        // only needs a short window; a page that is still moving gets a long
        // one. The AX fingerprint check is the final gate in both cases.
        //
        // The small-tree window must cover common hydration delays: a page
        // that fills in content from a `setTimeout(..., 1000)` fires *after*
        // `readyState == "complete"`, and exiting too early truncates that
        // content. 1200 ms of stillness is the compromise between catching
        // late hydration and not doubling the cost of every tiny page.
        const QUIET_MS_STATIC: u64 = 250;
        const QUIET_MS_DYNAMIC: u64 = 600;
        const QUIET_MS_SMALL_TREE: u64 = 1200;
        // The deadline covers the whole settle (page load + hydration + AX
        // population). A slow network can need several seconds just to
        // deliver the HTML, so the budget must not be shorter than the CDP
        // call timeout we already allow elsewhere (15s).
        let deadline = {
            let now = tokio::time::Instant::now();
            now.checked_add(Duration::from_millis(15000)).unwrap_or(now)
        };
        // Poll more often without shortening the evidence window. Every
        // acceptance still requires a stable AX fingerprint and full quiet
        // interval.
        let tick_ms: u64 = 100;
        let prefetch_lead_ms: u64 = 150;
        let mut last_change = tokio::time::Instant::now();
        let mut prev_dom = (0u64, 0u64);
        let mut legacy_saw_change = false;
        let mut candidate_saw_change = false;
        let mut ax_complete = false;
        // Same-document navigations carry no `loaderId`; the current document
        // is the right one from the start.
        let mut nav_committed = nav_loader.is_none();
        // Defensive fallback: a page whose commit event never arrives must not
        // hang the settle. After this, the loop trusts its own readyState and
        // DOM checks as before.
        let commit_fallback = {
            let now = tokio::time::Instant::now();
            now.checked_add(Duration::from_millis(3000)).unwrap_or(now)
        };
        loop {
            iterations = iterations.saturating_add(1);
            // Deadline check FIRST: every `continue` path below must be
            // able to exit the loop (a page that keeps changing would
            // otherwise spin forever).
            if tokio::time::Instant::now() >= deadline {
                tracing::debug!(target: "flashbrowse_core::session", raw_ax_nodes,
                    semantic = nodes.len(),
                    "settle deadline reached before the page stabilized"
                );
                break;
            }
            // Drain AX load-complete notifications for this navigation.
            // Chrome emits `Accessibility.loadComplete` once the tree for the
            // loaded document is fully populated, which is what separates a
            // legitimately sparse tree from one that is merely early.
            while let Ok(env) = ax_complete_rx.try_recv() {
                if env.session_id.as_deref() == Some(self.session_id.as_str()) {
                    ax_complete = true;
                }
            }
            // Wait for this navigation's document to commit before observing
            // anything. Any event with the matching loaderId means the new
            // document is now the one being polled.
            while let Ok(ev) = lifecycle_rx.try_recv() {
                let matches = match nav_loader.as_deref() {
                    Some(loader) => ev.get("loaderId").and_then(|v| v.as_str()) == Some(loader),
                    None => true,
                };
                if matches && !nav_committed {
                    nav_committed = true;
                    // The quiet window starts at commit: time spent before the
                    // new document existed must not count as stillness.
                    last_change = tokio::time::Instant::now();
                }
            }
            if !nav_committed {
                if tokio::time::Instant::now() >= commit_fallback {
                    tracing::debug!(target: "flashbrowse_core::session", "no commit lifecycle event; proceeding on own checks");
                    nav_committed = true;
                    last_change = tokio::time::Instant::now();
                } else {
                    if iterations > 1 {
                        let t_sleep = std::time::Instant::now();
                        tokio::time::sleep(Duration::from_millis(tick_ms)).await;
                        sleep_ms = sleep_ms.saturating_add(t_sleep.elapsed().as_millis() as u64);
                    }
                    continue;
                }
            }
            // Give the new document (and Chromium's accessibility tree) a
            // tick before the first observation. Fetching immediately after
            // commit samples a pre-population tree, which then reads as a
            // change at the end of the quiet window and costs a second full
            // window. The tick is what keeps the settle at one window.
            let t_sleep = std::time::Instant::now();
            tokio::time::sleep(Duration::from_millis(tick_ms)).await;
            sleep_ms = sleep_ms.saturating_add(t_sleep.elapsed().as_millis() as u64);

            // (1) cheap change detection: is the document still moving?
            let mut ready_state = String::from("loading");
            let mut dom_count = 0u64;
            let mut text_len = 0u64;
            let mut provably_static = false;
            let t_poll = std::time::Instant::now();
            let needs_static_classification = raw_ax_nodes < 100 && !legacy_saw_change;
            let cheap_expression = if shadow_enabled {
                format!(
                        "{SETTLE_DOM_STATS_JS} + '|' + (document.scripts.length === 0 && document.querySelectorAll('iframe').length === 0 && !document.querySelector('meta[http-equiv=refresh]')) + '|' + (window.__flashbrowseMutationState?.count ?? 0) + '|' + (window.__flashbrowseMutationState?.last_ms ?? 0) + '|' + (window.__flashbrowseMutationState ? performance.now() - window.__flashbrowseMutationState.last_ms : 0)"
                    )
            } else if needs_static_classification {
                format!(
                        "{SETTLE_DOM_STATS_JS} + '|' + (document.scripts.length === 0 && document.querySelectorAll('iframe').length === 0 && !document.querySelector('meta[http-equiv=refresh]'))"
                    )
            } else {
                format!("{SETTLE_DOM_STATS_JS} + '|false'")
            };
            if let Ok(cheap) =
                runtime::evaluate(&self.client, &self.session_id, &cheap_expression).await
            {
                if let Some(Value::String(s)) = cheap.result.value.as_ref() {
                    let mut it = s.split('|');
                    ready_state = it.next().unwrap_or("loading").to_string();
                    dom_count = it.next().unwrap_or("0").parse().unwrap_or(0);
                    text_len = it.next().unwrap_or("0").parse().unwrap_or(0);
                    provably_static = it.next().unwrap_or("false") == "true";
                    if shadow_enabled {
                        mutation_count = it.next().and_then(|v| v.parse().ok());
                        mutation_last_ms = it
                            .next()
                            .and_then(|v| v.parse::<f64>().ok())
                            .map(|ms| ms.max(0.0) as u64);
                        mutation_quiet_ms = it
                            .next()
                            .and_then(|v| v.parse::<f64>().ok())
                            .map(|ms| ms.max(0.0) as u64);
                    }
                }
            }
            poll_ms = poll_ms.saturating_add(t_poll.elapsed().as_millis() as u64);
            // "loading" = the document is still streaming; "interactive" = the
            // DOM is parsed but subresources (JS bundles!) are still coming;
            // "complete" = the browser considers the page fully loaded.
            let page_complete = ready_state.eq_ignore_ascii_case("complete");
            let parsed = !ready_state.eq_ignore_ascii_case("loading");
            if !parsed {
                // Still parsing - the tree cannot be final.
                last_change = tokio::time::Instant::now();
                continue;
            }
            let cheap_now = (dom_count, text_len);
            if prev_dom == (0, 0) {
                prev_dom = cheap_now; // baseline, not a change
            } else if cheap_now != prev_dom {
                prev_dom = cheap_now;
                last_change = tokio::time::Instant::now();
                legacy_saw_change = true;
                candidate_saw_change = true;
                continue;
            }
            // Quiet window:
            //   - the DOM was moving          -> wait longer (600 ms)
            //   - the tree is still small      -> wait longer (800 ms): a
            //     small tree is either a tiny page or a page that has not
            //     hydrated yet (nhrdev.com truncated to 140 tokens with a
            //     250 ms window);
            //   - a big, static tree           -> settle fast (250 ms).
            let legacy_quiet_ms = if legacy_saw_change {
                QUIET_MS_DYNAMIC
            } else if raw_ax_nodes < 100 && !provably_static {
                QUIET_MS_SMALL_TREE
            } else {
                QUIET_MS_STATIC
            };
            let candidate_quiet_ms = if shadow_enabled && candidate_saw_change {
                QUIET_MS_DYNAMIC
            } else if raw_ax_nodes < 100 && !provably_static {
                QUIET_MS_SMALL_TREE
            } else {
                QUIET_MS_STATIC
            };
            let legacy_quiet_enough =
                last_change.elapsed() >= Duration::from_millis(legacy_quiet_ms);
            let candidate_quiet_enough =
                last_change.elapsed() >= Duration::from_millis(candidate_quiet_ms);

            // (2) the AX tree must not be obviously empty while the DOM
            // already has content.
            //
            // Chromium populates the accessibility tree asynchronously
            // (~400 ms after the DOM is final, measured on Hacker News), so a
            // DOM-stability window alone truncates the page (140 tokens
            // instead of 1770). A pre-hydration tree is tiny (2-21 nodes)
            // while the DOM already holds hundreds of elements.
            // A tree with a handful of nodes cannot be a real page (unless
            // the page itself is tiny) - treat it as "not populated yet".
            // `raw_ax_nodes == 0` means "not fetched yet" -> fetch now.
            let tree_short = raw_ax_nodes == 0 || (raw_ax_nodes < 20 && dom_count >= 20);
            // A short tree is final when Chrome itself reports the document's
            // accessibility tree complete. Without this escape a page whose
            // DOM is large but whose tree is legitimately tiny (deep wrapper
            // chains that compress to a single text node) can never satisfy
            // `tree_unready` and burns the whole 15 s deadline on every open.
            // The stability gate is unchanged: the samples must still match
            // across the quiet window.
            let sparse_final = tree_short && ax_complete && page_complete;
            // Chromium's completed accessibility tree can legitimately be
            // sparse, so only keep waiting when the candidate lacks both
            // completion signals.
            let candidate_unready = tree_short && !sparse_final;
            let legacy_unready =
                raw_ax_nodes == 0 || (raw_ax_nodes < 20 && dom_count >= 20 && !sparse_final);

            // Sample early enough that the confirming fetch lands as soon as
            // the quiet window elapses. The two samples still bracket the
            // window (they are ~PREFETCH_LEAD_MS apart), so the stability
            // evidence is at least as strong as before, but acceptance no
            // longer pays an extra poll interval plus a sleep.
            let elapsed_ms = last_change.elapsed().as_millis() as u64;
            let prefetch_due = prefetch_lead_ms > 0
                && !candidate_quiet_enough
                && elapsed_ms.saturating_add(prefetch_lead_ms) >= candidate_quiet_ms;

            // Fetch when the tree looks unready (it must be refreshed while
            // we wait for it), when the DOM settled, or when the confirming
            // sample is due.
            if (legacy_snapshot.is_some() || (!legacy_unready && !legacy_quiet_enough))
                && (candidate_accept_ms.is_some()
                    || (!candidate_unready && !candidate_quiet_enough && !prefetch_due))
            {
                continue;
            }

            // (3) verify the AX tree itself
            let t_fetch = std::time::Instant::now();
            let (fresh, node_sessions) = self.driver.get_full_ax_tree_with_sessions().await?;
            ax_fetch_ms = ax_fetch_ms.saturating_add(t_fetch.elapsed().as_millis() as u64);
            ax_fetches = ax_fetches.saturating_add(1);
            let t_compress = std::time::Instant::now();
            let raw_ax_nodes_before = raw_ax_nodes;
            raw_ax_nodes = fresh.len();
            let (n2, m2) = compress(&fresh);
            let fp2 = tree_fingerprint(&n2, &m2);
            let ratio_ok = dom_count < 100 || raw_ax_nodes as u64 >= dom_count.div_ceil(4);
            let same = raw_ax_nodes_before == raw_ax_nodes && fp2 == fingerprint;
            action_sessions = build_action_sessions(&n2, &node_sessions);
            nodes = n2;
            map = m2;
            fingerprint = fp2;
            compress_ms = compress_ms.saturating_add(t_compress.elapsed().as_millis() as u64);

            let net_idle = self.net.idle(250);
            tracing::debug!(target: "flashbrowse_core::session", dom_count,
                text_len,
                raw_ax_nodes,
                semantic = nodes.len(),
                same,
                legacy_quiet_enough,
                candidate_quiet_enough,
                ax_complete,
                prefetch_due,
                sparse_final,
                net_idle,
                "settle poll"
            );

            // Accept when the tree is stable and the page is done.
            //
            // `readyState == "complete"` is the browser's own verdict that
            // everything (including JS bundles) has loaded - the
            // authoritative, network-speed-independent signal. The network
            // idle check is only a fallback for pages that keep a socket
            // open forever (never "complete").
            //
            // The tree must contain *something* (`nodes.len() >= 1`), not
            // three nodes: a minimal page ("<h1>Hi</h1>") legitimately
            // compresses to one or two nodes, and requiring three made
            // every such page wait out the whole settle deadline.
            let first_fetch = raw_ax_nodes_before == 0;
            let stable = same && ratio_ok && !nodes.is_empty() && (page_complete || net_idle);
            let legacy_stable = stable
                && (!first_fetch || !legacy_saw_change)
                && legacy_quiet_enough
                && !legacy_unready;
            let candidate_stable = stable
                && shadow_enabled
                && (!first_fetch || !candidate_saw_change)
                && candidate_quiet_enough
                && !candidate_unready;
            if legacy_stable && !shadow_enabled {
                last_raw_nodes = fresh;
                exit_reason = "settled";
                break;
            }
            if legacy_stable && legacy_snapshot.is_none() {
                legacy_snapshot = Some(SettleSnapshot {
                    nodes: nodes.clone(),
                    map: map.clone(),
                    action_sessions: action_sessions.clone(),
                    raw_nodes: fresh.clone(),
                });
            }
            if candidate_stable && candidate_accept_ms.is_none() {
                candidate_nodes = Some(nodes.clone());
                candidate_accept_ms = Some(t_open.elapsed().as_millis() as u64);
                mutation_would_delay_candidate =
                    mutation_quiet_ms.map(|quiet| quiet < candidate_quiet_ms);
            }
            if !same {
                // The tree still moves - restart the quiet window.
                last_change = tokio::time::Instant::now();
                // AX changes are evidence of dynamic page content just like
                // DOM text changes. The next confirmation uses the existing
                // dynamic quiet interval instead of treating the page as an
                // untouched small tree and waiting another full 1200 ms.
                if shadow_enabled && raw_ax_nodes_before > 0 {
                    candidate_saw_change = true;
                }
            }
            last_raw_nodes = fresh;
            if shadow_enabled && legacy_snapshot.is_some() && candidate_accept_ms.is_some() {
                exit_reason = "settled";
                break;
            }
        }
        if legacy_snapshot.is_none() && exit_reason == "deadline" {
            self.last_settle = Some(crate::observability::SettleBreakdown {
                nav_ms,
                poll_ms,
                sleep_ms,
                ax_fetch_ms,
                compress_ms,
                title_url_ms,
                render_ms: 0,
                iterations,
                ax_fetches,
                exit_reason: exit_reason.to_string(),
                candidate_accept_ms,
                legacy_tree_sha256: None,
                candidate_tree_sha256: None,
                shadow_diverged: None,
                raw_ax_nodes: raw_ax_nodes as u64,
                semantic_nodes: nodes.len() as u64,
                mutation_count,
                mutation_last_ms,
                mutation_quiet_ms,
                mutation_would_delay_candidate,
            });
            self.invalidate_tree();
            return Err(CoreError::NavigationFailed(format!(
                "page did not reach a stable accessibility tree within 15 seconds: {}",
                self.url
            )));
        }
        let accepted_raw_nodes;
        if let Some(snapshot) = legacy_snapshot {
            nodes = snapshot.nodes;
            map = snapshot.map;
            action_sessions = snapshot.action_sessions;
            accepted_raw_nodes = snapshot.raw_nodes;
        } else {
            accepted_raw_nodes = last_raw_nodes;
        }
        if nodes.is_empty() {
            // The page never rendered (network hiccup, bot wall, JS-only
            // shell). Report it instead of returning an empty tree.
            self.last_settle = Some(crate::observability::SettleBreakdown {
                nav_ms,
                poll_ms,
                sleep_ms,
                ax_fetch_ms,
                compress_ms,
                title_url_ms,
                render_ms: 0,
                iterations,
                ax_fetches,
                exit_reason: "empty".to_string(),
                candidate_accept_ms,
                legacy_tree_sha256: None,
                candidate_tree_sha256: None,
                shadow_diverged: None,
                raw_ax_nodes: raw_ax_nodes as u64,
                semantic_nodes: nodes.len() as u64,
                mutation_count,
                mutation_last_ms,
                mutation_quiet_ms,
                mutation_would_delay_candidate,
            });
            self.invalidate_tree();
            return Err(CoreError::NavigationFailed(format!(
                "page produced no accessibility content: {}",
                self.url
            )));
        }
        // Re-read the title and URL now that the page has settled. The
        // first read happens right after `navigate`, when the new document
        // often has not been parsed yet - `document.title` is then empty
        // and the header line was blank about one page in five.
        let t_titles_final = std::time::Instant::now();
        self.refresh_title_url().await;
        title_url_ms = title_url_ms.saturating_add(t_titles_final.elapsed().as_millis() as u64);
        let t_render = std::time::Instant::now();
        let rendered = render_nested(&self.title, &self.url, &nodes);
        let render_ms = t_render.elapsed().as_millis() as u64;
        let (legacy_tree_sha256, candidate_tree_sha256, shadow_diverged) = if shadow_enabled {
            let legacy_hash = sha256_hex(rendered.as_bytes());
            let candidate_hash = candidate_nodes.as_ref().map(|candidate| {
                sha256_hex(render_nested(&self.title, &self.url, candidate).as_bytes())
            });
            let diverged = candidate_hash
                .as_ref()
                .map(|candidate| candidate != &legacy_hash);
            (Some(legacy_hash), candidate_hash, diverged)
        } else {
            (None, None, None)
        };
        self.last_settle = Some(crate::observability::SettleBreakdown {
            nav_ms,
            poll_ms,
            sleep_ms,
            ax_fetch_ms,
            compress_ms,
            title_url_ms,
            render_ms,
            iterations,
            ax_fetches,
            exit_reason: exit_reason.to_string(),
            candidate_accept_ms,
            legacy_tree_sha256,
            candidate_tree_sha256,
            shadow_diverged,
            raw_ax_nodes: raw_ax_nodes as u64,
            semantic_nodes: nodes.len() as u64,
            mutation_count,
            mutation_last_ms,
            mutation_quiet_ms,
            mutation_would_delay_candidate,
        });
        self.action_map = map;
        self.action_sessions = action_sessions;
        self.last_tree = rendered.clone();
        tracing::debug!(target: "flashbrowse_core::session", ms = t_open.elapsed().as_millis(),
            nodes = nodes.len(),
            settle = ?self.last_settle,
            "open complete"
        );
        Ok((rendered, accepted_raw_nodes))
    }

    /// Navigate back one entry in session history and return the refreshed tree.
    pub async fn back(&mut self) -> Result<String, CoreError> {
        let hist = page::get_navigation_history(&self.client, &self.session_id).await?;
        let target = adjacent_history_entry(&hist, -1)
            .ok_or_else(|| CoreError::NavigationFailed("no previous history entry".to_string()))?;
        self.net.reset();
        page::navigate_to_history_entry(&self.client, &self.session_id, target.id).await?;
        self.settle_after_history_nav().await
    }

    /// Navigate forward one entry in session history and return the refreshed tree.
    pub async fn forward(&mut self) -> Result<String, CoreError> {
        let hist = page::get_navigation_history(&self.client, &self.session_id).await?;
        let target = adjacent_history_entry(&hist, 1)
            .ok_or_else(|| CoreError::NavigationFailed("no next history entry".to_string()))?;
        self.net.reset();
        page::navigate_to_history_entry(&self.client, &self.session_id, target.id).await?;
        self.settle_after_history_nav().await
    }

    /// Reload the current page and return the refreshed tree.
    pub async fn reload(&mut self, ignore_cache: bool) -> Result<String, CoreError> {
        self.net.reset();
        page::reload(&self.client, &self.session_id, ignore_cache).await?;
        self.settle_after_history_nav().await
    }

    async fn settle_after_history_nav(&mut self) -> Result<String, CoreError> {
        flashbrowse_cdp::runtime::run(self.settle_after_history_nav_inner()).await?
    }

    async fn settle_after_history_nav_inner(&mut self) -> Result<String, CoreError> {
        // Brief grace period so Chrome begins transitioning out of the old document.
        tokio::time::sleep(Duration::from_millis(60)).await;
        self.wait_for_popup_settle().await?;
        self.refresh_title_url().await;
        if self.url.starts_with("chrome-error://") {
            self.invalidate_tree();
            return Err(CoreError::NavigationFailed(self.url.clone()));
        }
        self.rebuild_tree().await
    }

    /// Wait until `condition` holds or `timeout_ms` elapses (capped 100..=30_000 ms),
    /// then return the refreshed semantic tree.
    pub async fn wait_for(
        &mut self,
        condition: WaitCondition,
        timeout_ms: u64,
    ) -> Result<String, CoreError> {
        flashbrowse_cdp::runtime::run(self.wait_for_inner(condition, timeout_ms)).await?
    }

    async fn wait_for_inner(
        &mut self,
        condition: WaitCondition,
        timeout_ms: u64,
    ) -> Result<String, CoreError> {
        let clamped = timeout_ms.clamp(100, 30_000);
        let deadline = {
            let now = tokio::time::Instant::now();
            now.checked_add(Duration::from_millis(clamped))
                .unwrap_or(now)
        };
        loop {
            let matched = match &condition {
                WaitCondition::Text(needle) => {
                    let needle_js = serde_json::to_string(needle)?;
                    let expr = format!(
                        r#"(() => {{
      const want = {needle_js};
      const pending = [document];
      while (pending.length) {{
        const root = pending.pop();
        if (root === document && document.body && (document.body.innerText || '').includes(want)) return true;
        if (root instanceof ShadowRoot && (root.textContent || '').includes(want)) return true;
        for (const el of root.querySelectorAll('*')) {{
          if (el.shadowRoot) pending.push(el.shadowRoot);
        }}
      }}
      return false;
    }})()"#
                    );
                    self.driver
                        .evaluate(&expr)
                        .await
                        .ok()
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false)
                }
                WaitCondition::Selector(selector) => {
                    let sel_js = serde_json::to_string(selector)?;
                    let expr = format!(
                        r#"(() => {{
      const sel = {sel_js};
      const pending = [document];
      while (pending.length) {{
        const root = pending.pop();
        try {{ if (root.querySelector(sel)) return true; }} catch (_) {{ return false; }}
        for (const el of root.querySelectorAll('*')) {{
          if (el.shadowRoot) pending.push(el.shadowRoot);
        }}
      }}
      return false;
    }})()"#
                    );
                    self.driver
                        .evaluate(&expr)
                        .await
                        .ok()
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false)
                }
                WaitCondition::UrlContains(part) => self
                    .driver
                    .evaluate("location.href")
                    .await
                    .ok()
                    .and_then(|v| v.as_str().map(|s| s.contains(part.as_str())))
                    .unwrap_or(false),
                WaitCondition::NetworkIdle { quiet_ms } => self.net.idle(*quiet_ms),
            };
            if matched {
                self.refresh_title_url().await;
                return self.rebuild_tree().await;
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(CoreError::NavigationFailed(format!(
                    "wait_for timed out after {clamped}ms"
                )));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}
