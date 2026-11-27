# flashbrowse - agent conventions

- **Commits:** Ask the maintainer which branch to commit on and wait for their answer before every commit, including documentation changes and amendments.
- **Project name:** Always write `flashbrowse` in lowercase, including at the start of a sentence and in commit messages, tag annotations, and release notes. Preserve the exact spelling of existing identifiers such as `FLASHBROWSE_*` environment variables.
- **Decision lock:** session-file hybrid, `--chrome-path`/`CHROME_PATH` support, `ResourcePolicy` (`agent-default` | `text-only` | `full-render`, legacy `--text-only`), scheme-less URL resolution (`url_norm`, https assumed, local hosts get http), MCP latest (2025-06-18).
- **Screenshots:** same MCP session (`browser_screenshot`); blocking lift must not change AX tree output. Screenshot prepare uses network idle + image readiness (timeout capped), not a fixed 400 ms sleep. Lifting the blocklist is not enough on its own: Chrome never retries a blocked request, so blocked images must be re-requested for the capture.
- **Firefox:** `flashbrowse-bidi` crate = discovery/launch groundwork only; production sessions remain Chrome until BiDi AX tree lands.
- **Phase gates:** `cargo clippy --workspace --all-targets -- -D warnings` must be clean before advancing phases.
- **Tracer test:** `cargo test --workspace` (needs system Chrome for `cdp_connection`; skips gracefully when absent).
- **Non-goal (MVP):** no daemon socket - `close` + session files only.

## Accuracy rules (do not break these)

1. **Never drop visible text.** Every pruning rule in the compressor must
   remove a *duplicate*, never the only copy of the content. A table cell, a
   list item and a paragraph must all survive compression.
2. **Document order.** Chromium returns the AX tree breadth-first; the
   compressor rebuilds it and walks depth-first (`document_order`). Do not
   replace that with the raw array order.
3. **Never trade accuracy for speed.** No truncating waits, no early
   snapshots, no "probably good enough" tree. If the tree is not final,
   wait for it.
4. **No em dashes or en dashes** anywhere in the repo (source, docs, commits).
5. **Benchmarks are sequential.** One tool at a time, one page at a time, and
   the machine must be idle. A parallel or loaded benchmark is a lie.
6. **Settle exit conditions.** The open/settle loop must accept a tree of ANY
   size (`!nodes.is_empty()`), never `nodes.len() >= 3`; a minimal page would
   otherwise wait out the whole deadline. `InlineTextBox` is layout noise,
   never content: it exists on every `StaticText` and must not count as a
   meaningful child.

## Architecture & invariants

- **Workspace crates:** `flashbrowse-cdp` (raw WS CDP, flatten-mode mux, 15s `CALL_TIMEOUT`, 32 MiB `MAX_FRAME_BYTES`), `flashbrowse-bidi` (Firefox discovery only), `flashbrowse-core` (`BrowserSession`, `BrowserDriver`/`CdpDriver` seam, `tree::{compress, render}`, `interceptor`, `stealth`, `observability`, `paths`, `url_norm`), `flashbrowse-mcp` (serial `SessionHandle`, 1 MiB line cap, 120s outer `tools/call` timeout), `flashbrowse-cli` (CLI + `~/.cache/flashbrowse/sessions/<id>.json`).
- **Never block CSS:** `interceptor.rs` blocks trackers, fonts, and images (and AV in `text-only`), never JS or CSS. Blocking CSS leaks `display:none` / `visibility:hidden` elements into the AX tree.
- **Re-apply per-session CDP state on attach:** `Page.addScriptToEvaluateOnNewDocument` (`STEALTH_SCRIPT`) and `Network.setBlockedURLs` are scoped to CDP `sessionId`; `BrowserSession::attach` must re-apply both on every attach.
- **Ephemeral `ActionMap`:** Every `open` and `act` rebuilds `action_map`. Failed navigations (`chrome-error://` or empty semantic output) must call `invalidate_tree()` so stale IDs fail immediately with `ActionNotFound`.
- **Non-popup `<select>`:** `BrowserSession::select` uses `DOM.resolveNode` + `Runtime.callFunctionOn` so native dropdown popups never open and swallow input.
- **Screenshot readiness:** `images_ready` checks `img.complete` (not `naturalWidth > 0`) so broken images do not burn the timeout; `prepare_lazy_content_for_screenshot` must restore `window.scrollY`.
- **Resource policy precedence:** `--resource-policy` > `--full-render` / `--text-only` > `FLASHBROWSE_RESOURCE_POLICY` > `agent-default`; conflicting flags must fail loudly.
- **MCP URL allowlist:** `validate_mcp_url` permits only `http://`, `https://`, `data:`, and `about:blank` (`file://` stays CLI-only). MCP normalizes first (`resolve_mcp_url`) and applies the allowlist to the normalized value, so a bare host is judged as the `https://` host it becomes. Normalization never invents a scheme for input that already has one, so `file://` and `javascript:` are still rejected.
- **URL normalization boundary:** `normalize_url` (`url_norm`) is a boundary concern for `open`, `dump`, `bench` and MCP `browser_open` only. `BrowserSession::open` stays exact and never rewrites input. An unbracketed IPv6 literal is bracketed so it is a valid authority (`::1` becomes `http://[::1]`); a bracketed address is left as typed.

## Observability

- Every operation records a `MetricRecord` in
  `~/.cache/flashbrowse/metrics.jsonl` via
  `flashbrowse_core::observability::record_op`.
- Logs go to **stderr only** (stdout must stay pure JSON-RPC for MCP).
- `--log-format json` emits one JSON object per line.

## Release gates

- `cargo fmt --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace`
- `node scripts/soak.mjs --minutes 5 --url <url>` (smoke version of the
  soak; the full 1-2 h run is done before a release)
