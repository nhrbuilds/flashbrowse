//! `flashbrowse bench [urls...]` - token/latency/memory benchmarks.

use std::path::PathBuf;
use std::time::Instant;

use flashbrowse_cdp::protocol::runtime;
use flashbrowse_core::observability;
use flashbrowse_core::{
    normalize_url, BrowserSession, ResourcePolicy, SessionConfig, StealthProfile,
};

/// Standard default targets (kept network-minimal for CI speed).
const DEFAULT_URLS: &[&str] = &["https://example.com"];

/// Token estimate in *characters* (caller passes char count, not bytes).
fn token_estimate(chars: usize) -> usize {
    chars.div_ceil(4)
}

fn char_count(s: &str) -> usize {
    s.chars().count()
}

#[cfg(unix)]
fn rss_mb(pid: Option<u32>) -> Option<f64> {
    let pid = pid?;
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let line = status.lines().find(|l| l.starts_with("VmRSS"))?;
    let kb: f64 = line.split_whitespace().nth(1)?.parse::<f64>().ok()?;
    Some(kb / 1024.0)
}

#[cfg(not(unix))]
fn rss_mb(_pid: Option<u32>) -> Option<f64> {
    None
}

/// Run benchmarks against `urls` (or defaults) and print a table.
pub async fn run(
    urls: Vec<String>,
    chrome_path: Option<PathBuf>,
    resource_policy: ResourcePolicy,
    stealth_profile: StealthProfile,
) -> anyhow::Result<()> {
    let started = std::time::Instant::now();
    let targets: Vec<String> = if urls.is_empty() {
        DEFAULT_URLS.iter().map(|s| s.to_string()).collect()
    } else {
        urls
    };
    // Resolve every target before the first browser launches: a typo should
    // fail once, not once per row after paying for a browser each time.
    let targets: Vec<String> = match targets
        .iter()
        .map(|u| normalize_url(u))
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(resolved) => resolved,
        Err(e) => {
            // Record the failure like `open` and `dump` do: every operation
            // must leave a trace in metrics.jsonl.
            observability::record_op(
                "bench",
                &targets.join(","),
                started,
                None,
                false,
                Some(e.to_string()),
            );
            return Err(anyhow::anyhow!("{e}"));
        }
    };

    crate::write_stdout(format_args!(
        "{:<38} {:>10} {:>12} {:>10} {:>10}",
        "URL", "Raw HTML", "AXTree", "Latency", "RSS"
    ))?;
    crate::write_stdout(format_args!(
        "{:-<38} {:-<10} {:-<12} {:-<10} {:-<10}",
        "", "", "", "", ""
    ))?;

    let mut any_fail = false;
    let mut last_settle = None;
    for url in &targets {
        match bench_one(url, chrome_path.clone(), resource_policy, stealth_profile).await {
            Ok(row) => {
                last_settle = row.settle.clone();
                crate::write_stdout(format_args!(
                    "{:<38} {:>8}t {:>10}t {:>8}ms {:>7}{}",
                    trim_url(url),
                    row.raw_tokens,
                    row.ax_tokens,
                    row.latency_ms,
                    row.rss_mb
                        .map(|m| format!("{m:.0}M"))
                        .unwrap_or_else(|| "n/a".to_string()),
                    if row.raw_tokens > 0 && row.ax_tokens > 0 {
                        format!(
                            "  ({:.0}% reduction)",
                            (1.0 - row.ax_tokens as f64 / row.raw_tokens as f64) * 100.0
                        )
                    } else {
                        String::new()
                    },
                ))?;
                crate::write_stdout(format_args!(
                    "{:<38} browser startup {}ms",
                    "", row.startup_ms
                ))?;
            }
            Err(e) => {
                any_fail = true;
                crate::write_stdout(format_args!("{:<38} FAIL: {e}", trim_url(url)))?;
            }
        }
        if let Some(s) = last_settle.take() {
            crate::write_stdout(format_args!(
                "{:<38} settle nav {}ms poll {}ms sleep {}ms ax {}ms/{} compress {}ms title {}ms render {}ms iters {} ({})",
                "",
                s.nav_ms,
                s.poll_ms,
                s.sleep_ms,
                s.ax_fetch_ms,
                s.ax_fetches,
                s.compress_ms,
                s.title_url_ms,
                s.render_ms,
                s.iterations,
                s.exit_reason
            ))?;
        }
    }
    let joined = targets.join(",");
    observability::record_op(
        "bench",
        &joined,
        started,
        None,
        !any_fail,
        if any_fail {
            Some("one or more targets failed".to_string())
        } else {
            None
        },
    );
    Ok(())
}

struct BenchRow {
    raw_tokens: usize,
    ax_tokens: usize,
    latency_ms: u128,
    startup_ms: u128,
    rss_mb: Option<f64>,
    settle: Option<flashbrowse_core::observability::SettleBreakdown>,
}

async fn bench_one(
    url: &str,
    chrome_path: Option<PathBuf>,
    resource_policy: ResourcePolicy,
    stealth_profile: StealthProfile,
) -> anyhow::Result<BenchRow> {
    let config = SessionConfig {
        chrome_path,
        resource_policy,
        stealth_profile,
        ..Default::default()
    };
    let start = Instant::now();
    let startup_start = Instant::now();
    let mut session = BrowserSession::launch(config).await?;
    let startup_ms = startup_start.elapsed().as_millis();
    let tree = match session.open(url).await {
        Ok(t) => t,
        Err(e) => {
            session.shutdown().await;
            return Err(e.into());
        }
    };
    let latency_ms = start.elapsed().as_millis();
    let settle = session.last_settle().cloned();
    let pid = session.pid();

    // Raw HTML token estimate from in-page outerHTML.
    let raw_chars = match runtime::evaluate(
        &session.client(),
        session.session_id(),
        "document.documentElement.outerHTML.length",
    )
    .await
    {
        Ok(r) => r
            .result
            .value
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(0),
        Err(_) => 0,
    };

    let rss = rss_mb(pid);
    session.shutdown().await;

    let tree_chars = char_count(&tree);
    Ok(BenchRow {
        raw_tokens: token_estimate(raw_chars),
        ax_tokens: token_estimate(tree_chars),
        latency_ms,
        startup_ms,
        rss_mb: rss,
        settle,
    })
}

fn trim_url(url: &str) -> String {
    if url.chars().count() > 38 {
        let s: String = url.chars().take(35).collect();
        format!("{s}...")
    } else {
        url.to_string()
    }
}
