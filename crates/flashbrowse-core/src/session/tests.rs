use super::*;

#[cfg(test)]
mod settle_shadow_tests {
    use std::time::{Duration, Instant};

    use super::{target, BrowserContextCleanup, BrowserSession, SessionConfig};

    #[cfg(unix)]
    #[tokio::test]
    async fn screenshot_guard_disconnects_the_client_during_unwind() {
        use super::ScreenshotGuard;
        use tokio::io::duplex;

        let (client_rx, _peer_tx) = duplex(16);
        let (_peer_rx, client_tx) = duplex(16);
        let client = flashbrowse_cdp::CdpClient::connect_pipe(client_rx, client_tx)
            .await
            .expect("create an in-memory CDP client");
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = ScreenshotGuard {
                client: &client,
                armed: true,
            };
            panic!("simulate a screenshot preparation panic");
        }));

        assert!(panic.is_err());
        assert!(
            !client.is_connected(),
            "a partially prepared screenshot must invalidate its CDP session"
        );
    }

    #[test]
    fn malformed_history_index_cannot_overflow() {
        let history = super::page::GetNavigationHistoryResponse {
            current_index: usize::MAX,
            entries: Vec::new(),
        };
        assert!(super::adjacent_history_entry(&history, 1).is_none());
        assert!(super::adjacent_history_entry(&history, -1).is_none());
    }

    #[test]
    fn sha256_matches_standard_vector() {
        assert_eq!(
            super::sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[tokio::test]
    async fn dropped_browser_context_guard_schedules_context_disposal() {
        if flashbrowse_cdp::find_chrome(None).is_err() {
            return;
        }
        let session = match BrowserSession::launch(SessionConfig {
            no_sandbox: Some(true),
            ..SessionConfig::default()
        })
        .await
        {
            Ok(session) => session,
            Err(error) => {
                eprintln!("SKIP: Chrome could not launch for context cleanup test: {error}");
                return;
            }
        };
        let client = session.client();
        let browser_context_id = super::target::create_browser_context(&client)
            .await
            .expect("create isolated browser context");
        let target_id = super::target::create_target_in_context(
            &client,
            "about:blank",
            Some(&browser_context_id),
        )
        .await
        .expect("create page in isolated context")
        .target_id;

        drop(BrowserContextCleanup::new(
            client.clone(),
            browser_context_id,
        ));

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut disposed = false;
        while Instant::now() < deadline {
            let targets = super::target::get_targets(&client)
                .await
                .expect("list Chrome targets");
            if !targets
                .target_infos
                .iter()
                .any(|target| target.target_id == target_id)
            {
                disposed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        session.shutdown().await;
        assert!(disposed, "dropping the guard should dispose its context");
    }

    #[tokio::test]
    async fn externally_closed_active_page_recovers_inside_its_browser_context() {
        if flashbrowse_cdp::find_chrome(None).is_err() {
            return;
        }
        let config = SessionConfig {
            no_sandbox: Some(true),
            ..SessionConfig::default()
        };
        let default_session = match BrowserSession::launch(config.clone()).await {
            Ok(session) => session,
            Err(error) => {
                eprintln!("SKIP: Chrome could not launch for tab recovery test: {error}");
                return;
            }
        };
        let mut isolated = default_session
            .create_isolated_context(&config)
            .await
            .expect("create isolated context");
        let isolated_context = isolated
            .browser_context_id()
            .expect("isolated context id")
            .to_string();
        let closed_target = isolated.target_id().to_string();
        let isolated_client = isolated.client();

        assert!(target::close_target(&isolated_client, &closed_target)
            .await
            .expect("close active target externally"));
        let close_deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < close_deadline {
            let tabs = isolated.list_tabs().await.expect("list isolated tabs");
            if !tabs.iter().any(|tab| tab.target_id == closed_target) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        let tabs = isolated
            .recover_closed_active_tab(&config)
            .await
            .expect("recover active tab");
        assert_ne!(isolated.target_id(), closed_target);
        assert!(tabs.iter().any(|tab| tab.target_id == isolated.target_id()));
        assert!(tabs
            .iter()
            .all(|tab| tab.browser_context_id.as_deref() == Some(&isolated_context)));
        assert_eq!(
            isolated.browser_context_id(),
            Some(isolated_context.as_str())
        );

        let tree = isolated
            .open("data:text/html,%3Ctitle%3ERecovered%3C/title%3E%3Ch1%3EIsolated%20Recovery%3C/h1%3E")
            .await
            .expect("open after external tab closure");
        assert!(tree.contains("Isolated Recovery"));
        let default_tabs = default_session
            .list_tabs()
            .await
            .expect("list default context tabs");
        let all_targets = target::get_targets(&default_session.client())
            .await
            .expect("list all browser targets");
        assert!(
            !default_tabs.is_empty(),
            "default context tabs: {default_tabs:?}; all targets: {:?}",
            all_targets.target_infos
        );
        assert!(default_tabs.iter().all(|tab| {
            tab.browser_context_id.as_deref() == default_session.target_context_id.as_deref()
        }));

        isolated
            .close_isolated_context()
            .await
            .expect("close context");
        default_session.shutdown().await;
    }
}

#[cfg(test)]
mod popup_tracker_tests {
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};

    use flashbrowse_cdp::protocol::target::TargetInfo;

    use super::{record_popup_target, take_popup_from_tracker, PopupTracker};

    fn target(id: &str, kind: &str, opener: Option<&str>) -> TargetInfo {
        TargetInfo {
            target_id: id.to_string(),
            kind: kind.to_string(),
            url: format!("https://example.com/{id}"),
            title: id.to_string(),
            opener_id: opener.map(str::to_string),
            browser_context_id: None,
        }
    }

    #[test]
    fn records_newest_page_for_opener_and_ignores_non_page_and_existing() {
        let tracker: PopupTracker = Arc::new(Mutex::new(Vec::new()));
        record_popup_target(&tracker, target("sw1", "service_worker", Some("opener1")));
        record_popup_target(&tracker, target("old_tab", "page", Some("opener1")));
        record_popup_target(&tracker, target("other", "page", Some("opener2")));
        record_popup_target(&tracker, target("popup1", "page", Some("opener1")));

        let mut existing = HashSet::new();
        existing.insert("old_tab".to_string());

        let picked = take_popup_from_tracker(&tracker, "opener1", &existing).expect("popup1");
        assert_eq!(picked.target_id, "popup1");
        assert!(take_popup_from_tracker(&tracker, "opener1", &existing).is_none());
        assert!(take_popup_from_tracker(&tracker, "opener2", &existing).is_some());
    }
}
