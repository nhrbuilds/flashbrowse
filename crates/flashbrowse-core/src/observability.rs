//! Lightweight observability: structured logs + a rolling metrics trail.
//!
//! - Logs go to stderr only (stdout must stay pure JSON-RPC for MCP).
//! - `text` (default) is human readable; `json` is one JSON object per line
//!   with stable field names, ready for a log shipper.
//! - Every operation appends a record to `~/.cache/flashbrowse/metrics.jsonl`
//!   so `flashbrowse stats` can report latency/token/error trends.

use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Log output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    Text,
    Json,
}

impl std::str::FromStr for LogFormat {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "text" | "plain" => Ok(LogFormat::Text),
            "json" => Ok(LogFormat::Json),
            other => Err(format!("unknown log format '{other}' (text|json)")),
        }
    }
}

/// A metrics record for one operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricRecord {
    /// Unix timestamp (seconds).
    pub ts: u64,
    /// Operation: `open`, `act`, `dump`, `bench`, `mcp_tool`, ...
    pub op: String,
    /// Target URL (or tool name).
    pub target: String,
    /// Duration in milliseconds.
    pub ms: u64,
    /// Agent-visible payload size in tokens (4 chars/token).
    pub tokens: u64,
    /// Number of actionable elements (when known).
    pub actions: u64,
    /// `true` when the operation succeeded.
    pub ok: bool,
    /// Error message (when it failed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Per-open settle timing breakdown (see [`SettleBreakdown`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settle: Option<SettleBreakdown>,
}

/// Wall-clock attribution of one `BrowserSession::open` settle.
///
/// Every stage of the settle loop is timed so a latency claim can be checked
/// against evidence instead of a guess. All durations are milliseconds.
///
/// The stages are deliberately disjoint: `nav_ms` covers only the
/// `Page.navigate` round-trip, `poll_ms` only the cheap readiness evaluates,
/// `sleep_ms` only the fixed inter-poll sleeps, `ax_fetch_ms` only
/// `Accessibility.getFullAXTree`, `compress_ms` only compress + fingerprint,
/// and `render_ms` only the final render. Whatever is left over is CDP
/// serialization and the remaining bookkeeping.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettleBreakdown {
    /// `Page.navigate` round-trip.
    pub nav_ms: u64,
    /// Cheap readiness polls (`Runtime.evaluate`).
    pub poll_ms: u64,
    /// Fixed inter-poll sleeps inside the settle loop.
    pub sleep_ms: u64,
    /// `Accessibility.getFullAXTree` calls made by the settle loop.
    pub ax_fetch_ms: u64,
    /// Compress + fingerprint of each fetched tree.
    pub compress_ms: u64,
    /// Initial and post-settle title/URL reads.
    pub title_url_ms: u64,
    /// Render of the final tree.
    pub render_ms: u64,
    /// Settle-loop iterations.
    pub iterations: u64,
    /// AX tree fetches inside the settle loop.
    pub ax_fetches: u64,
    /// Why the settle ended: `settled`, `deadline`, `empty` or `error`.
    pub exit_reason: String,
    /// Shadow candidate acceptance offset from the start of `open`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_accept_ms: Option<u64>,
    /// SHA-256 of the rendered tree accepted by the legacy decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy_tree_sha256: Option<String>,
    /// SHA-256 of the rendered tree accepted by the shadow candidate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_tree_sha256: Option<String>,
    /// Whether the two rendered trees differ byte-for-byte.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shadow_diverged: Option<bool>,
    /// Final raw Chromium AX node count.
    #[serde(default)]
    pub raw_ax_nodes: u64,
    /// Final compressed semantic node count.
    #[serde(default)]
    pub semantic_nodes: u64,
    /// MutationObserver records since navigation, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mutation_count: Option<u64>,
    /// `performance.now()` timestamp of the last mutation in the document.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mutation_last_ms: Option<u64>,
    /// Milliseconds since the last observed DOM mutation at candidate accept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mutation_quiet_ms: Option<u64>,
    /// Whether the observer would have extended the candidate's quiet window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mutation_would_delay_candidate: Option<bool>,
}

impl MetricRecord {
    /// Build a record.
    pub fn new(op: &str, target: &str, ms: u64) -> Self {
        Self {
            ts: now_secs(),
            op: op.to_string(),
            target: target.to_string(),
            ms,
            tokens: 0,
            actions: 0,
            ok: true,
            error: None,
            settle: None,
        }
    }

    /// Attach the settle breakdown (open operations only).
    pub fn with_settle(mut self, settle: SettleBreakdown) -> Self {
        self.settle = Some(settle);
        self
    }

    /// Attach the payload size.
    pub fn with_payload(mut self, chars: usize, actions: usize) -> Self {
        self.tokens = estimate_tokens(chars);
        self.actions = actions as u64;
        self
    }

    /// Mark the record as failed.
    pub fn failed(mut self, err: impl Into<String>) -> Self {
        self.ok = false;
        self.error = Some(err.into());
        self
    }
}

/// Metrics trail location (`~/.cache/flashbrowse/metrics.jsonl`).
pub fn metrics_path() -> PathBuf {
    crate::paths::cache_dir().join("metrics.jsonl")
}

/// Count the numbered actionable entries in a rendered action tree.
///
/// Rendered trees use one line per element, actionable ones prefixed with
/// `[<id>]`. Non-actionable lines (headings, plain text) are ignored.
pub fn count_actions(tree: &str) -> usize {
    tree.lines()
        .filter(|l| {
            let t = l.trim_start();
            t.strip_prefix('[')
                .and_then(|tail| tail.chars().next())
                .is_some_and(|c| c.is_ascii_digit())
        })
        .count()
}

/// Estimate the token cost of a payload (4 chars per token, the usual
/// English average). Deliberately an estimate: the point is to compare
/// payload sizes, not to bill anyone.
pub fn estimate_tokens(chars: usize) -> u64 {
    chars.div_ceil(4) as u64
}

/// Record one operation and return the record (for logging).
pub fn record_op(
    op: &str,
    target: &str,
    started: std::time::Instant,
    payload: Option<(usize, usize)>,
    ok: bool,
    error: Option<String>,
) -> MetricRecord {
    record_op_with_settle(op, target, started, payload, ok, error, None)
}

/// Like [`record_op`], with an optional settle breakdown for open operations.
pub fn record_op_with_settle(
    op: &str,
    target: &str,
    started: std::time::Instant,
    payload: Option<(usize, usize)>,
    ok: bool,
    error: Option<String>,
    settle: Option<SettleBreakdown>,
) -> MetricRecord {
    let ms = started.elapsed().as_millis() as u64;
    let mut r = MetricRecord::new(op, target, ms);
    if let Some((chars, actions)) = payload {
        r = r.with_payload(chars, actions);
    }
    if let Some(s) = settle {
        r = r.with_settle(s);
    }
    if !ok {
        r = r.failed(error.unwrap_or_else(|| "unknown error".to_string()));
    }
    record(&r);
    r
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Append a record to the metrics trail (best effort, never fails a call).
pub fn record(r: &MetricRecord) {
    let path = metrics_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(line) = serde_json::to_string(r) {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let _ = writeln!(f, "{line}");
        }
    }
}

/// Read the most recent records (oldest first).
pub fn read_records(limit: usize) -> Vec<MetricRecord> {
    let path = metrics_path();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let mut out: Vec<MetricRecord> = text
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    if out.len() > limit {
        out = out.split_off(out.len().saturating_sub(limit));
    }
    out
}

/// Aggregated statistics over a set of records.
#[derive(Debug, Clone, Serialize)]
pub struct Stats {
    /// Number of records.
    pub count: usize,
    /// Number of failed records.
    pub failures: usize,
    /// Failure rate (0..1).
    pub error_rate: f64,
    /// Median latency in ms.
    pub p50_ms: u64,
    /// 95th percentile latency in ms.
    pub p95_ms: u64,
    /// Maximum latency in ms.
    pub max_ms: u64,
    /// Mean latency in ms.
    pub avg_ms: u64,
    /// Median payload tokens.
    pub p50_tokens: u64,
    /// Total payload tokens.
    pub total_tokens: u64,
    /// Mean number of actions per operation.
    pub avg_actions: f64,
}

impl Stats {
    /// Compute stats over `records`.
    ///
    /// Percentile convention: sort ascending, then take the value at index
    /// `floor(n * p)`. For even counts this is the *upper* median
    /// (`p50` of 10 samples is the 6th smallest). Upper median keeps the
    /// metric integral and matches how most APM tools report it.
    pub fn from(records: &[MetricRecord]) -> Self {
        if records.is_empty() {
            return Self {
                count: 0,
                failures: 0,
                error_rate: 0.0,
                p50_ms: 0,
                p95_ms: 0,
                max_ms: 0,
                avg_ms: 0,
                p50_tokens: 0,
                total_tokens: 0,
                avg_actions: 0.0,
            };
        }
        let mut ms: Vec<u64> = records.iter().map(|r| r.ms).collect();
        let mut tok: Vec<u64> = records.iter().map(|r| r.tokens).collect();
        ms.sort_unstable();
        tok.sort_unstable();
        let failures = records.iter().filter(|r| !r.ok).count();
        let total_tokens = records
            .iter()
            .fold(0u64, |total, record| total.saturating_add(record.tokens));
        let total_actions = records
            .iter()
            .fold(0u64, |total, record| total.saturating_add(record.actions));
        let count = records.len();
        let percentile_95_index = ((count as f64 * 0.95).ceil() as usize)
            .saturating_sub(1)
            .min(count.saturating_sub(1));
        Self {
            count,
            failures,
            error_rate: failures as f64 / count as f64,
            p50_ms: ms.get(ms.len() / 2).copied().unwrap_or_default(),
            p95_ms: ms.get(percentile_95_index).copied().unwrap_or_default(),
            max_ms: ms.last().copied().unwrap_or_default(),
            avg_ms: ms
                .iter()
                .fold(0u64, |sum, value| sum.saturating_add(*value))
                .checked_div(count as u64)
                .unwrap_or_default(),
            p50_tokens: tok.get(tok.len() / 2).copied().unwrap_or_default(),
            total_tokens,
            avg_actions: total_actions as f64 / records.len() as f64,
        }
    }
}

/// Emit a structured log line to stderr.
pub fn log_event(format: LogFormat, level: &str, event: &str, fields: &[(&str, String)]) {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    match format {
        LogFormat::Json => {
            let mut obj = serde_json::Map::new();
            obj.insert("ts".into(), serde_json::Value::from(ts));
            obj.insert("level".into(), serde_json::Value::from(level));
            obj.insert("event".into(), serde_json::Value::from(event));
            for (k, v) in fields {
                obj.insert((*k).to_string(), serde_json::Value::from(v.clone()));
            }
            let line = serde_json::Value::Object(obj).to_string();
            let mut err = std::io::stderr();
            let _ = writeln!(err, "{line}");
        }
        LogFormat::Text => {
            let mut line = event.to_string();
            for (k, v) in fields {
                line.push_str(&format!(" {k}={v}"));
            }
            let mut err = std::io::stderr();
            let _ = writeln!(err, "{line}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{count_actions, estimate_tokens, MetricRecord, Stats};

    #[test]
    fn counts_numbered_actions_only() {
        let tree = "Title: X\nURL: https://x\n---\n[1] Link: \"a\"\nHeading 1: Hi\n[12] Button: \"b\"\nplain text\n[ 3] Broken\n";
        // Only lines that start with `[` followed by a digit count.
        assert_eq!(count_actions(tree), 2);
    }

    #[test]
    fn empty_tree_has_no_actions() {
        assert_eq!(count_actions(""), 0);
        assert_eq!(count_actions("Title: X\nURL: y\n---\n"), 0);
    }

    #[test]
    fn token_estimate_is_four_chars_per_token() {
        assert_eq!(estimate_tokens(0), 0);
        assert_eq!(estimate_tokens(4), 1);
        assert_eq!(estimate_tokens(5), 2);
        assert_eq!(estimate_tokens(400), 100);
    }

    #[test]
    fn stats_computes_percentiles() {
        // 10 samples of 10,20,...,100 ms.
        let recs: Vec<MetricRecord> = (1..=10)
            .map(|i| MetricRecord::new("open", "https://x", i * 10).with_payload(400, 20))
            .collect();
        let s = Stats::from(&recs);
        assert_eq!(s.count, 10);
        assert_eq!(s.failures, 0);
        // Upper median of 10 sorted samples = 6th smallest = 60.
        assert_eq!(s.p50_ms, 60);
        assert_eq!(s.p95_ms, 100);
        assert_eq!(s.max_ms, 100);
        assert_eq!(s.avg_ms, 55);
        // 10 records x 400 chars / 4 chars-per-token = 10 x 100 tokens.
        assert_eq!(s.total_tokens, 1000);
        assert_eq!(s.p50_tokens, 100);
        assert_eq!(s.avg_actions, 20.0);
    }

    #[test]
    fn stats_count_failures() {
        let recs = vec![
            MetricRecord::new("open", "a", 10),
            MetricRecord::new("open", "b", 20).failed("boom"),
        ];
        let s = Stats::from(&recs);
        assert_eq!(s.failures, 1);
        assert!((s.error_rate - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn empty_stats_are_zero() {
        let s = Stats::from(&[]);
        assert_eq!(s.count, 0);
        assert_eq!(s.p95_ms, 0);
    }
}
