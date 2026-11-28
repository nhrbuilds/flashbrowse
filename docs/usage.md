# Usage guide

This guide covers the `flashbrowse` command line interface and the browser tools
provided by its MCP server. For repository structure, see
[Architecture](architecture.md). For measurement methods and results, see
[Benchmarks](benchmarks.md).

## Commands

```text
flashbrowse setup                    # download managed Chrome-for-Testing (~120 MB, once)
flashbrowse open <url>               # compressed action tree (ephemeral browser)
flashbrowse open <url> --session-id s1   # ...and keep the browser alive
flashbrowse act <id> --session-id s1 --action click --value ...
flashbrowse dump <url> --format markdown|axtree|raw|fidelity
flashbrowse bench <url>              # latency + token self-measurement
flashbrowse mcp                      # MCP stdio server for Claude Code / Cursor / Codex
flashbrowse close --session-id s1    # stop a persistent browser

flashbrowse doctor                   # environment diagnostics (exit 1 on failure)
flashbrowse doctor --json            # same, machine readable
flashbrowse stats                    # latency / token / error stats from the metrics trail
flashbrowse stats --limit 500 --json
flashbrowse update                   # download and install the latest release
flashbrowse update --check           # check only, do not install
```

Global flags: `--chrome-path <p>`, `--text-only`, `--full-render`,
`--resource-policy agent-default|text-only|full-render`,
`--stealth-profile host-native|windows`, `--verbose`,
`--log-format text|json`.

Chrome discovery checks an explicit `--chrome-path`, then `CHROME_PATH` and
`CHROME_BINARY`, followed by system and managed installations. Set either
environment variable to use a browser outside the usual locations. Set
`FLASHBROWSE_PREFER_MANAGED=1` to check the managed installation before Chrome
on `PATH`. Run `flashbrowse setup` only when no suitable Chrome installation
is available.

## Existing browser profiles and file transfer

`flashbrowse open <url> --user-data-dir <directory>` launches Chrome with the
selected profile. Use `--cdp-url <http://host:port>` or a browser `ws://`
endpoint to attach to an existing Chrome process. Attached browsers remain
running when flashbrowse disconnects. The two options cannot be combined.
The MCP server accepts the same flags:

```text
flashbrowse mcp --cdp-url http://127.0.0.1:9222 --file-root ./browser-files
flashbrowse mcp --storage-state-root ./states
flashbrowse act 4 --session-id s1 --action upload --value ./report.pdf
```

MCP `browser_storage_state` imports or exports cookies and per-origin
localStorage using named JSON files under `--storage-state-root` (default
`~/.cache/flashbrowse/storage-state`). MCP `browser_upload` accepts paths only
within `--file-root` (default `~/.cache/flashbrowse/files`). Downloads are
saved under that root's `downloads` directory. `browser_downloads` reports
download progress and can wait for completion or cancellation. Native
JavaScript alerts are accepted by default; confirms and prompts are dismissed.
Pass `dialog_action` to `browser_act` to override the next dialog, and include
`prompt_text` only when accepting a prompt.

## MCP tool groups

- **History & explicit waits:** `browser_navigate` (`back`, `forward`,
  `reload`) and `browser_wait_for` (`text`, `selector`, `url`,
  `network_idle_ms` with open `shadowRoot` traversal).
- **Richer inputs & framework clickables:** `browser_act` supports
  `double_click`, `right_click`, `drag_and_drop` (`target_action_id`), modifier
  key chords (`Control+K`, `Meta+Enter`, `Shift+Tab`), and element-targeted
  `scroll`. Non-ARIA clickable detection covers React `__reactProps$.onClick`
  and `cursor: pointer` elements (`cap = 48`) verified via
  `DOMDebugger.getEventListeners` (with `Runtime.releaseObjectGroup` cleanup).
- **Console, network, headers & route mocking:** `browser_console`,
  `browser_network` (request/response logs, failed-only filter, response body
  inspection up to 64 KiB, persistent `set_headers`), and lazy `browser_route`
  (`Fetch.enable` wildcard fulfill/abort rules preserved across tab switches).
- **Isolated browser contexts:** `browser_contexts` (`list`, `create`,
  `select`, `close`) runs isolated incognito-like `BrowserContext` sessions in
  one Chrome process without leaking cookies, `localStorage`, or background
  tasks.
- **Emulation, permissions, element capture & PDF:** `browser_emulate`
  (viewport/mobile/DPR, timezone, locale, geolocation, permissions, full
  `reset`), `browser_screenshot` with `action_id` bounding-box clipping, and
  `browser_pdf` confined under `--file-root/pdfs/`.

## Tabs, forms, and accessibility trees

- **Multi-tab & popup lifecycle:** `browser_tabs` lists, selects, and closes
  pages (rejecting closing the last tab). Popups opened by `browser_open` or
  `browser_act` are tracked via `Target.targetCreated` / `attachedToTarget`,
  settled automatically, and fall back to their opener when closed.
- **Batched form filling (`browser_fill_form`):** update multiple
  `{action_id, value}` fields from one starting tree in a single call and get
  back one authoritative refreshed tree (or completed IDs plus the refreshed
  tree if a field becomes unavailable mid-batch).
- **Nested trees, open shadow roots, and clickable non-ARIA elements:**
  sessions default to indented `render_nested` output (flat `render` remains
  available), settle tracking and `browser_extract_markdown` traverse open
  `shadowRoot`s, and visible non-ARIA elements with explicit click handlers
  (`<div onclick>`, `.onclick`) receive action IDs in document order without
  duplicating text or wrapping already-actionable buttons/links.
- **Live policy switching & coherent stealth profiles:** changing
  `resource_policy` in MCP updates `Network.setBlockedURLs` on the live session
  without restarting Chrome. `--stealth-profile host-native|windows` (default
  `host-native`) strips `HeadlessChrome` and keeps `User-Agent`,
  `navigator.platform`, and Client Hints coherent.
- **Transport & latency gains:** POSIX pipe transport on Unix, adaptive
  post-action settle, and bounded concurrent child iframe fetching reduced
  `cold_open` p95 by 29.4%, `warm_open` p95 by 16.8%, and `warm_act` p95 by
  85.8% (`445.2 ms -> 63.3 ms`).

## URL handling and resource policy

The scheme is optional, like an address bar: `flashbrowse open example.com`
uses `https://`, while local hosts (`localhost:3000`, `127.0.0.1`, `::1`,
`*.local`, RFC1918 addresses) use `http://` because a dev server is plain HTTP
in practice. An explicit scheme is never rewritten. Anything that cannot be a
URL (a search query, a bare path) fails with the fix in the message instead of
a CDP error. `dump` and `bench` resolve the same way. MCP `browser_open`
resolves the same way too, then still applies its allowlist (`http`, `https`,
`data`, `about:blank`; `file://` stays CLI-only).

Precedence: `--resource-policy` > `--full-render` / `--text-only` >
`FLASHBROWSE_RESOURCE_POLICY` > `agent-default`. Conflicting flags fail
loudly rather than silently picking one.

## Screenshots

**One MCP for hands and eyes:** after `browser_open` / `browser_act`, call
`browser_screenshot` on the same session (do not start Playwright or a second
browser MCP). In agent-default mode, screenshots temporarily allow fonts and
images to load (and re-request the images the network layer blocked, so the
capture shows real pixels); the action tree token count is unchanged.

## Observability

Every operation (CLI **and** MCP tool calls) appends a record to
`~/.cache/flashbrowse/metrics.jsonl`:

```json
{"ts":1794660615,"op":"mcp:browser_open","target":"https://example.com","ms":512,"tokens":1215,"actions":24,"ok":true}
```

`flashbrowse stats` aggregates that trail:

```text
flashbrowse stats  (last 200 operations)
  latency   p50 282 ms   p95 1453 ms   max 1962 ms   avg 351 ms
  payload   p50 1215 tokens   total 1834631 tokens   avg 12.4 actions
  errors    15 (0.60%)
```

Logs go to **stderr only** (stdout stays pure JSON-RPC, which MCP requires).
`--log-format json` makes every log line a single JSON object, ready for a log
shipper:

```sh
flashbrowse open https://example.com --log-format json
```

## Update channel

`flashbrowse update` checks GitHub Releases for a newer tag. The channel is
selected with `FLASHBROWSE_UPDATE_CHANNEL`:

| value | picks |
|---|---|
| `stable` (default) | the latest `vX.Y.Z` release |
| `beta` | the latest `vX.Y.Z-beta*` / `-rc*` |
| `nightly` | the latest `vX.Y.Z-nightly*` |
| `off` | never check |

Without `--check`, `flashbrowse update` downloads the matching platform
archive from GitHub Releases and replaces the running executable in place. Pass
`--check` to only report whether a newer release is available.

## `flashbrowse doctor`

Pre-flight diagnostics for agents and CI: Chromium presence and version,
managed-Chrome install, cache-dir writability, free temp space, available
memory, stored sessions, and orphaned browser processes. Exits `1` if a
blocking check fails.

```text
flashbrowse doctor
flashbrowse doctor --json
```

