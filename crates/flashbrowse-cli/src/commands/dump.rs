//! `flashbrowse dump <url> --format markdown|axtree|axtree-nested|raw|fidelity`.

use std::path::PathBuf;

use flashbrowse_core::{
    compress_with_options, normalize_url, render, render_nested, BrowserSession, CompressOptions,
    ResourcePolicy, SessionConfig, StealthProfile,
};

use flashbrowse_core::observability;

/// Emit Markdown, a compressed tree, a raw CDP AX tree, or a same-open
/// raw-and-rendered bundle for fidelity diagnostics.
/// Format is validated BEFORE launch so a typo costs zero browsers (H7).
pub async fn run(
    url: String,
    format: String,
    no_prune: bool,
    chrome_path: Option<PathBuf>,
    resource_policy: ResourcePolicy,
    stealth_profile: StealthProfile,
) -> anyhow::Result<()> {
    let started = std::time::Instant::now();
    match format.as_str() {
        "markdown" | "axtree" | "axtree-nested" | "raw" | "fidelity" => {}
        other => {
            return Err(anyhow::anyhow!(
                "unknown --format '{other}' (expected markdown|axtree|axtree-nested|raw|fidelity)"
            ))
        }
    }
    let url = match normalize_url(&url) {
        Ok(u) => u,
        Err(e) => {
            observability::record_op("dump", &url, started, None, false, Some(e.to_string()));
            return Err(anyhow::anyhow!("{e}"));
        }
    };
    let config = SessionConfig {
        chrome_path,
        resource_policy,
        stealth_profile,
        ..Default::default()
    };
    let mut session = BrowserSession::launch(config).await?;
    let result = async {
        let (opened_tree, fidelity_raw) = if format == "fidelity" {
            session.open_with_raw_snapshot(&url).await?
        } else {
            (session.open(&url).await?, Vec::new())
        };
        let out = match format.as_str() {
            "markdown" => session.extract_markdown().await?,
            "axtree" if !no_prune => session.last_tree().to_string(),
            "fidelity" => serde_json::to_string(&serde_json::json!({
                "raw_nodes": fidelity_raw,
                "tree": opened_tree,
            }))?,
            "axtree" | "axtree-nested" => {
                let raw = session.raw_ax_nodes().await?;
                let opts = CompressOptions { prune: !no_prune };
                let (nodes, _) = compress_with_options(&raw, opts);
                if format == "axtree-nested" {
                    render_nested(session.page_title(), session.page_url(), &nodes)
                } else {
                    render(session.page_title(), session.page_url(), &nodes)
                }
            }
            "raw" => {
                // The uncompressed tree Chromium returned, before any pruning.
                // Pretty-printed because this is a debugging and eval surface,
                // not something an agent consumes in a loop.
                let nodes = session.raw_ax_nodes().await?;
                serde_json::to_string_pretty(&nodes)?
            }
            _ => {
                return Err(flashbrowse_core::CoreError::NavigationFailed(format!(
                    "unsupported dump format '{format}'"
                )))
            }
        };
        Ok::<String, flashbrowse_core::CoreError>(out)
    }
    .await;
    session.shutdown().await;
    let out = match result {
        Ok(o) => o,
        Err(e) => {
            let msg = e.to_string();
            observability::record_op("dump", &url, started, None, false, Some(msg));
            return Err(e.into());
        }
    };
    crate::write_stdout(format_args!("{out}"))?;
    let actions = if format == "axtree" || format == "axtree-nested" {
        observability::count_actions(&out)
    } else {
        0
    };
    observability::record_op(
        "dump",
        &url,
        started,
        Some((out.len(), actions)),
        true,
        None,
    );
    Ok(())
}
