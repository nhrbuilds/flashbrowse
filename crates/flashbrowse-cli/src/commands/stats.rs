//! `flashbrowse stats` - aggregated metrics from the metrics trail.

use flashbrowse_core::observability::{read_records, Stats};

/// Print aggregated statistics for the last `limit` operations.
pub fn run(limit: usize, json_output: bool) -> anyhow::Result<()> {
    let records = read_records(limit);
    let stats = Stats::from(&records);
    if json_output {
        let out = serde_json::json!({
            "window": limit,
            "count": stats.count,
            "failures": stats.failures,
            "error_rate": stats.error_rate,
            "p50_ms": stats.p50_ms,
            "p95_ms": stats.p95_ms,
            "max_ms": stats.max_ms,
            "avg_ms": stats.avg_ms,
            "p50_tokens": stats.p50_tokens,
            "total_tokens": stats.total_tokens,
            "avg_actions": stats.avg_actions,
        });
        crate::write_stdout(format_args!("{}", serde_json::to_string_pretty(&out)?))?;
        return Ok(());
    }
    if stats.count == 0 {
        crate::write_stdout(format_args!(
            "no metrics recorded yet (metrics live in ~/.cache/flashbrowse/metrics.jsonl)"
        ))?;
        return Ok(());
    }
    crate::write_stdout(format_args!(
        "flashbrowse stats  (last {} operations)",
        stats.count
    ))?;
    crate::write_stdout(format_args!(
        "  latency   p50 {} ms   p95 {} ms   max {} ms   avg {} ms",
        stats.p50_ms, stats.p95_ms, stats.max_ms, stats.avg_ms
    ))?;
    crate::write_stdout(format_args!(
        "  payload   p50 {} tokens   total {} tokens   avg {} actions",
        stats.p50_tokens, stats.total_tokens, stats.avg_actions
    ))?;
    crate::write_stdout(format_args!(
        "  errors    {} ({:.1}%)",
        stats.failures,
        stats.error_rate * 100.0
    ))?;
    Ok(())
}
