//! `flashbrowse act <id>` - dispatch one action against a persistent tab.
//!
//! The session file pins `browser_ws_url + target_id`. Because CDP
//! flatten sockets die with each CLI process, `act` re-connects, re-attaches
//! (fresh `sessionId` for the same live target), rebuilds the tree so IDs
//! are current, then performs the requested action and prints the new tree.

use std::path::PathBuf;

use flashbrowse_core::{actions::ScrollDirection, BrowserSession, SessionConfig};

use super::session_file::{effective_resource_policy, read_session};
use flashbrowse_core::observability;

/// Perform one action (`click|fill|hover|press_key|scroll`) on `[action_id]`.
pub async fn run(
    session_id: String,
    action_id: u32,
    action: String,
    value: Option<String>,
    chrome_path: Option<PathBuf>,
) -> anyhow::Result<()> {
    let started = std::time::Instant::now();
    let target = format!("{session_id}#{action_id}:{action}");
    let persisted = match read_session(&session_id)? {
        Some(p) => p,
        None => {
            let msg = format!("no session '{session_id}' (open one with --session-id first)");
            observability::record_op("act", &target, started, None, false, Some(msg.clone()));
            return Err(anyhow::anyhow!(msg));
        }
    };

    let config = SessionConfig {
        chrome_path,
        resource_policy: effective_resource_policy(&persisted),
        ..Default::default()
    };
    let result: anyhow::Result<String> = async {
        let mut session = BrowserSession::attach(
            persisted.browser_ws_url.clone(),
            Some(persisted.target_id.clone()),
            &config,
        )
        .await?;

        let tree = match action.as_str() {
            "click" => session.click(action_id).await?,
            "double_click" => session.double_click(action_id).await?,
            "right_click" => session.right_click(action_id).await?,
            "drag_and_drop" => {
                let target_id = value
                    .ok_or_else(|| {
                        anyhow::anyhow!("drag_and_drop requires --value <target_action_id>")
                    })?
                    .parse::<u32>()
                    .map_err(|_| anyhow::anyhow!("target_action_id must be an integer"))?;
                session.drag_and_drop(action_id, target_id).await?
            }
            "fill" => {
                let v = value.ok_or_else(|| anyhow::anyhow!("fill requires --value <text>"))?;
                session.fill(action_id, &v).await?
            }
            "hover" => session.hover(action_id).await?,
            "press_key" => {
                let k = value.ok_or_else(|| anyhow::anyhow!("press_key requires --value <key>"))?;
                session.press_key(&k).await?
            }
            "scroll" => {
                let dir = ScrollDirection::parse(value.as_deref().unwrap_or("down"))
                    .unwrap_or(ScrollDirection::Down);
                session
                    .scroll_at((action_id > 0).then_some(action_id), dir, 300.0)
                    .await?
            }
            "select" => {
                let v = value.ok_or_else(|| anyhow::anyhow!("select requires --value <option>"))?;
                session.select(action_id, &v).await?
            }
            "upload" => {
                let path =
                    value.ok_or_else(|| anyhow::anyhow!("upload requires --value <file-path>"))?;
                session
                    .upload_files(action_id, &[std::path::PathBuf::from(path)])
                    .await?
            }
            other => {
                anyhow::bail!(
                    "unknown action '{other}' (expected click|double_click|right_click|drag_and_drop|fill|hover|press_key|scroll|select|upload)"
                );
            }
        };
        session.shutdown().await;
        Ok(tree)
    }
    .await;

    match result {
        Ok(tree) => {
            crate::write_stdout(format_args!("{tree}"))?;
            let payload = (tree.len(), observability::count_actions(&tree));
            observability::record_op("act", &target, started, Some(payload), true, None);
            Ok(())
        }
        Err(e) => {
            let msg = e.to_string();
            observability::record_op("act", &target, started, None, false, Some(msg.clone()));
            Err(e)
        }
    }
}
