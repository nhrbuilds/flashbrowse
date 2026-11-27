<p align="center">
  <img src="assets/icon.png" alt="flashbrowse icon" width="128" height="128">
</p>

# flashbrowse

Industrial-grade, single-binary, headless browser controller and token-compression
engine written in pure Rust - built for LLM agents.

- Zero Python/Node/JVM. No Playwright/Puppeteer/Selenium shims.
- Raw CDP JSON-RPC 2.0 over POSIX pipe (`--remote-debugging-pipe` on Unix) or
  WebSocket (Windows, `--cdp-url`, and persistent `--session-id` sessions),
  using browser-level `flatten` multiplexing.
- AXTree-first compression: **3x - 23x fewer tokens** than Playwright/Puppeteer
  accessibility snapshots (and 75-99% less than raw HTML on content pages).
- Native MCP stdio server for Claude Code, Cursor, Codex.
- Lower latency than Playwright and Puppeteer in the documented agent-loop
  benchmark (see the benchmark method and limits below).

> **Honest limits.** Stealth defeats naive bots, not determined fingerprinting
> (Canvas/Audio/TLS entropy remain). `select` matches text/value. Markdown
> extraction is lightweight. `flashbrowse setup` fetches managed Chrome (~120 MB
> once). See `RELEASE.md` and the benchmarks below.

Workspace layout:

```text
crates/flashbrowse-cdp    CDP client, launcher, transport, protocol types
crates/flashbrowse-bidi   Firefox BiDi groundwork (discovery; engine not live yet)
crates/flashbrowse-core   session, stealth, interceptor, tree compression, actions
crates/flashbrowse-mcp    MCP JSON-RPC stdio server
crates/flashbrowse-cli    CLI binary (`flashbrowse`)
scripts/soak.mjs          long-running soak/load harness
```

## Install (one script)

macOS / Linux:
```sh
curl -fsSL https://raw.githubusercontent.com/nhrbuilds/flashbrowse/main/install.sh | sh
```

Windows (PowerShell):
```powershell
irm https://raw.githubusercontent.com/nhrbuilds/flashbrowse/main/install.ps1 | iex
```

Then (browser once - or skip if you already have Chrome):
```sh
flashbrowse setup                          # Chrome-for-Testing -> ~/.cache/flashbrowse/chrome (~120MB)
flashbrowse open https://example.com       # just works
flashbrowse mcp                            # stdio server for Claude Code / Cursor / Codex
```

Or with Cargo, straight from this repository (flashbrowse is not on
crates.io, npm or Homebrew yet):
```sh
cargo install --locked --git https://github.com/nhrbuilds/flashbrowse flashbrowse-cli
```

**No signing or notarization is required.** The install script and
`cargo install` never set the macOS quarantine attribute (Gatekeeper only
blocks files that a *browser* marked; the installer clears it for you). See
`RELEASE.md` for the details.

## Build from source

```sh
git clone https://github.com/nhrbuilds/flashbrowse
cargo build --release        # single binary, LTO+strip
./target/release/flashbrowse setup
./target/release/flashbrowse open https://example.com
```

Requires a Chromium: use `flashbrowse setup` (managed), a system
`google-chrome-stable`/`google-chrome`/`chromium` on PATH, or
`CHROME_PATH=/path/to/chrome`.

---

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

### Existing browser profiles and file transfer

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

### v1.5.0 release highlights

- **Size-oriented release configuration:** Release builds use
  `opt-level = "z"`, LTO, one codegen unit, and symbol stripping. The ZIP
  Zopfli encoder and tar extended-attribute support are disabled; ZIP DEFLATE
  reading and self-update archive handling remain. `panic = "unwind"` is
  retained, and production builds reject `panic = "abort"` because panic
  recovery depends on unwinding.
  These settings target smaller binaries, but there is no matched v1.4.0 to
  v1.5.0 executable-size measurement in the repository.
- **Panic and timeout recovery:** MCP tool panics and the 120 s tool deadline
  return tool errors. The server cancels failed work and waits briefly for it
  to stop. If it stops and the CDP connection remains usable, recovery keeps
  valid browser contexts and invalidates stale action IDs after page-changing
  operations. A disconnected browser or work that does not stop resets the
  session manager. Failures while creating or closing one isolated context
  preserve sibling contexts when their connection remains usable and schedule
  best-effort cleanup for the uncertain context. A panic while handling a
  complete request frame returns an error and the loop can continue before
  response output begins. If a write fails after output starts, the connection
  closes because another JSON-RPC frame could corrupt the partial response.
  The CLI catches an unexpected unwind at its command boundary and exits with
  a failure status after logging to stderr. If the active tab is closed
  externally, `browser_open` reattaches to another page in the same browser
  context or creates a blank page there. This preserves cookie and storage
  isolation between contexts.
- **Settle accuracy:** `browser_open` returns only after a stable, non-empty
  accessibility tree is verified. If the 15-second settle deadline expires,
  it returns an error and clears the old action IDs rather than publishing a
  partial tree.

### Multi-context isolation, observability, emulation, and v1.5.0 tools

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

## Roadmap for 1.6.0

This is a working plan for contributors, not a release promise.

### Maintainer focus

- **Firefox through BiDi:** extend the current Firefox discovery and launch
  groundwork toward browser sessions and accessibility-tree support. Chrome
  remains the production backend until the required tree and action behavior
  has passing coverage.
- **Recovery and performance gates:** extend fault-injection and context
  cleanup coverage, and keep sequential p95 comparisons reproducible before
  accepting changes to shared session or transport behavior.

### Good contributor areas

- Add small HTML fixtures and focused regression tests for supported features
  such as dialogs, popups, file transfer, storage isolation, shadow roots, and
  form interactions.
- Improve user documentation, runnable examples, and setup recipes for MCP
  clients. Keep commands and schemas checked against the current CLI and tool
  definitions.
- Improve cross-platform CI reliability and test-server portability.
- Improve benchmark and soak report readability without changing settle
  conditions or tree-compression behavior.

### Avoiding overlapping work

The maintainer is reserving the Firefox BiDi driver, CDP transport, core
`BrowserSession` recovery, settle loop, compressor semantics, and p95 gates.
Please open an issue before making broad changes in those areas. Self-contained
fixtures, tests, documentation, examples, and CI portability fixes are the
best areas to start independently. Accuracy rules still apply: preserve visible
text and document order, and do not shorten waits to improve a benchmark.

### Multi-tab, forms, shadow DOM, and v1.4.0 improvements

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

**One MCP for hands and eyes:** after `browser_open` / `browser_act`, call
`browser_screenshot` on the same session (do not start Playwright or a second
browser MCP). In agent-default mode, screenshots temporarily allow fonts and
images to load (and re-request the images the network layer blocked, so the
capture shows real pixels); the action tree token count is unchanged.

### Observability

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

### Update channel

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

### `flashbrowse doctor`

Pre-flight diagnostics for agents and CI: Chromium presence and version,
managed-Chrome install, cache-dir writability, free temp space, available
memory, stored sessions, and orphaned browser processes. Exits `1` if a
blocking check fails.

```text
flashbrowse doctor  (v1.0.2)
  [PASS] chromium         /usr/bin/google-chrome-stable (Google Chrome 147.0.7727.116)
  [PASS] managed-chrome   installed at /home/user/.cache/flashbrowse/chrome
  [PASS] cache-dir        /home/user/.cache/flashbrowse (writable=true)
  [PASS] temp-space       6327 MB free in /tmp
  [PASS] memory           15526 MB available
  [PASS] sessions         0 stored (flashbrowse close --session-id <id> to stop)
  [PASS] orphan-browsers  0 flashbrowse-managed browser(s) still running
```

---

## Benchmarks

**Original agent-loop comparison:** every tool was measured **sequentially** -
one tool at a time, one page open at a time - against the same Chrome 147 binary
and the same local page (a 30-row table with 60 links). Nothing runs in parallel,
so the numbers are not distorted by machine load. 30 iterations per tool, 0
failures, 0 leaked browser processes. Token estimate: 1 token = 4 characters.

The comparison below is an earlier baseline, not a matched v1.5.0 release
comparison. The v1.4.0 measurements and v1.5.0 development soaks later in this
section use separate runs and should not be read as proof of a p95 change
between those versions.

### Agent loop (the real workload: one browser, N pages)

| | min | median | p95 | max | agent payload |
|---|---|---|---|---|---|
| **flashbrowse** | **490 ms** | **524 ms** | **568 ms** | 1579 ms | **1215 tokens** |
| Puppeteer | 538 ms | 564 ms | 666 ms | 731 ms | 21817 tokens |
| Playwright | 541 ms | 579 ms | 786 ms | 843 ms | 8589 tokens |

**flashbrowse is 1.08x faster than Puppeteer, 1.10x faster than Playwright,
and uses 18.0x / 7.1x fewer tokens.** (The single 1579 ms outlier is the very
first open of a session, which spawns Chromium.)

> The payload grew from an earlier 813 tokens to 1215 because the compressor
> stopped **dropping table cells**. The old number was smaller because the
> table content was missing, not because it was compressed better.

### Cold open (a fresh browser per page)

| | min | median | p95 | tokens |
|---|---|---|---|---|
| flashbrowse | 1066 ms | 2066 ms | 2490 ms | 1215 |
| Puppeteer | 827 ms | 896 ms | 1008 ms | 21555 |
| Playwright | 884 ms | 1059 ms | 1291 ms | 8589 |

Cold open is a browser-startup benchmark more than a page benchmark: everyone
pays the Chrome spawn, and flashbrowse additionally waits for the page to be
*complete* (see below). The agent loop is where flashbrowse is designed to win.

### Real sites (tokens, observed)

| URL | flashbrowse | Puppeteer | Playwright | raw HTML |
|---|---|---|---|---|
| example.com | **272 t** | 1358 t | 814 t | 145 t |
| news.ycombinator.com | **2143 t** | 47 930 t | 19 269 t | 8620 t |
| github.com | **1719 t** | 40 137 t | 17 102 t | 131 319 t |

> The Puppeteer and Playwright columns are much larger than in earlier
> revisions of this table because they are measured with
> `interestingOnly: false` - the full accessibility tree, which is what an
> agent actually has to sift through. The flashbrowse column is the
> compressed action tree, which is what flashbrowse returns.

### Stress suite (`cargo test --workspace`)

| Test | Result |
|---|---|
| 50k-node synthetic AXTree | **98.8%** reduction (34.4 MB -> 408 KB) |
| 20x sequential `open` | avg **234 ms**, p95 283 ms |
| 100x action storm | avg **286 ms**, p95 347 ms, zero stale IDs |
| 5x parallel sessions | 1.94 s total |
| MCP x50 tool calls | 11.5 s (~4.3/s serial) |
| Truncation guard | 20 opens each of a hydration page and three fixtures, exact tree equality on every repeat |

### Soak / load test

`scripts/soak.mjs` drives a long-running MCP session the way an agent would
(open -> act -> scroll -> markdown -> screenshot) and watches for the failure
modes that only show up after hours: memory growth, latency drift, error
creep, and leaked browsers.

```sh
cargo build --release
node scripts/soak.mjs --minutes 120 \
  --url http://localhost:8099/ --url https://example.com \
  --interval 300 --out soak-report.json
```

The run is **strictly sequential** (one tool call at a time) so it measures
flashbrowse, not the machine. The report contains throughput, per-tool
latency percentiles, first-tenth vs last-tenth drift, RSS of the flashbrowse
process and of the browser, the orphan-browser count, and the error list.
CI runs a 5-minute version of the same harness on every push.

Observed 90-minute mixed soak (`5,401 s`, 7 local + public URLs):
- **Throughput:** 3,366 iterations, 13,152 tool calls (`2.435/s`), **0 errors** (`0.00%`)
- **Latency & drift:** `p50 = 255 ms`, `p95 = 1,288 ms`, `drift_ratio = 0.985` (first-tenth `259 ms` -> last-tenth `255 ms`)
- **Memory & cleanup:** `leak_gate.ok = true` (post-warmup `119,804 kB` -> `116,340 kB`, ratio `0.971`), **0** leaked Chrome processes at exit

Observed v1.3.0 settle improvements (matched Chrome 147, sequential warm opens):
- **Complex & nested pages (p95):** `a11y.html` `1,370 ms -> 445 ms` (-67.5%), `forms.html` `1,365 ms -> 454 ms` (-66.7%), `nested-depth.html` `15,194 ms -> 1,364 ms` (-91.0%)
- **Simple pages (p50 / p95):** `table.html` `499 / 551 ms -> 502 / 526 ms` (p95 -25 ms), synthetic table `506 / 564 ms -> 507 / 522 ms` (p95 -42 ms)
- **v1.3.0 release-gate 5-minute soak:** 451 tool calls (`148` iterations), **0 errors** (`0.00%`), `p50 = 257 ms`, `drift_ratio = 1.004`, steady-state RSS `+242 kB`, **0** leaked Chrome processes at exit

Observed v1.4.0 transport and action improvements (sequential 15-iteration release benchmark):
- **Cold open (`p50 / p95`):** `1,183.7 / 1,285.7 ms -> 835.2 / 908.0 ms` (p95 -29.4%)
- **Warm open (`p50 / p95`):** `455.8 / 522.8 ms -> 427.2 / 435.2 ms` (p95 -16.8%)
- **Warm action (`p50 / p95`):** `414.0 / 445.2 ms -> 60.7 / 63.3 ms` (p95 -85.8%)
- **v1.4.0 release-gate 5-minute soak:** 512 tool calls (`168` iterations), **0 errors** (`0.00%`), `p50 = 33 ms`, `p95 = 1,395 ms`, `drift_ratio = 1.065`, steady-state RSS `+200 kB`, **0** leaked Chrome processes at exit
- **v1.5.0 development 5-minute mixed-page soak** ([report](eval/results/soak-v1.5.0-panic-hardening-smoke.json)): 863 calls across a local fixture and `example.com`, **0 errors**, `p50 = 39 ms`, `p95 = 1,356 ms`, `max = 1,436 ms`, `drift_ratio = 0.975`, leak gate passed (ratio `1.009`), and **0** Chrome processes remained at exit.
- **v1.5.0 development 5-minute all-tool soak** (`--suite v15`, all 18 tools, [report](eval/results/soak-v1.5.0-panic-hardening-v15.json)): 923 calls on a local fixture, **0 errors**, `p50 = 32 ms`, `p95 = 1,344 ms`, `max = 1,640 ms`, `drift_ratio = 0.939`, leak gate passed (ratio `1.005`), and **0** Chrome processes remained at exit.
- **v1.5.0 development 45-minute mixed-page soak** ([report](eval/results/soak-v1.5.0-panic-hardening-heavy.json)): 7,522 calls across a local fixture and `example.com`, **0 errors**, `p50 = 40 ms`, `p95 = 1,358 ms`, `max = 2,267 ms`, `drift_ratio = 1.077`, leak gate passed (ratio `0.991`), and **0** Chrome processes remained at exit.
- **Initial v1.5.0 build, 5-minute all-tool smoke** ([report](eval/results/soak-v1.5.0-final-smoke.json)): 923 calls, **0 errors**, `p50 = 32 ms`, `p95 = 1,345 ms`, `max = 1,715 ms`, `drift_ratio = 0.97`, flashbrowse RSS growth `+148 kB`, and **0** Chrome processes at exit.

### What is blocked, and why

Measured byte shares of a real page load (Chrome `Network.loadingFinished`):

| resource | share | blocked? | why |
|---|---|---|---|
| JS | 9-55% | **no** | it hydrates the page we read - blocking it breaks everything |
| images | 17-42% | **yes** | useless to an LLM agent, and the AX tree keeps the `alt` text either way |
| fonts | 2-12% | **yes** | no rendering for us |
| trackers | ~5% | **yes** | noise + latency |
| CSS | **1-3%** (18% on HN) | **no** | see below |

**`full-render` policy:** only trackers stay blocked; images and fonts load
for faithful screenshots while the compressed AX tree stays the same size as
agent-default.

**Why CSS is not blocked:** `display:none` / `visibility:hidden` elements are
excluded from the accessibility tree *because of CSS*. Measured on a local page:
with CSS blocked, `SECRET HIDDEN CONTENT` (a `display:none` div) appears in the
tree - content the user cannot see would be exposed to the agent, and the agent
could click invisible elements. For 1-3% of bytes that is a bad trade.

**Why images are blocked:** a blocked image still exposes its `alt` text as the
accessible name (verified against Chromium 147), so the AX tree is unchanged -
the page is just 20-40% lighter. `browser_screenshot` lifts the block for the
capture so pixels are still real.

### Why flashbrowse is faster (and still complete)

Playwright/Puppeteer snapshot after `domcontentloaded` + 500 ms. That is fast
because it can catch a half-rendered page - measured on Hacker News, a
`domcontentloaded` snapshot yields **65 tokens instead of 1770**.

flashbrowse waits for a stable accessibility tree before returning it:

1. cheap poll (one `Runtime.evaluate`) every 100 ms - `readyState`, DOM size,
   `innerText` length;
2. the DOM must be quiet (250 ms for a provably static page or a large tree,
   600 ms after observed changes, and 1200 ms for a small page that may still
   hydrate);
3. the browser must report `readyState == "complete"` or the network must be
   idle;
4. the AX tree must not be obviously empty while the DOM has content
   (Chromium fills the AX tree ~400 ms after the DOM is final);
5. the AX content fingerprint must match the previous sample.

The settle loop allows up to 15 seconds. If it cannot verify a stable,
non-empty tree by then, `browser_open` returns an error and invalidates stale
action IDs instead of returning its latest partial snapshot. The repeatability
test checks exact tree equality across 20 opens of a hydration page and three
fixtures. That result applies to those test pages, not to every dynamic site.

### What is measured vs not measured

Fidelity and pilot task checks live in [`EVAL.md`](EVAL.md) (dev-only harness
under [`eval/`](eval/)):
- **Fidelity oracle:** the current test corpus contains 14 local fixture pages.
  A 17-URL development sweep (14 fixtures and 3 public pages) reported 0 failed
  pages and 0 missing strings, with 90.1% to 98.5% byte reduction versus raw
  AXTree JSON ([report](eval/results/fidelity-v1.5.0-panic-hardening.json)).
- **Pilot task eval (10 tasks):** 8/10 read-only oracle pass; 8/10 LLM pilot
  pass with nested/full observation arms (`nvidia/nemotron-3.5-super-vl:free`)
  vs 5/10 flat.

There is no WebArena, Mind2Web or WebVoyager run in this repo yet, so broad
external benchmark claims are not made.

Known limits of the text tree:

- **Nesting is approximate.** Output is indented by semantic depth up to 128
  levels. This suggests containment, but does not provide exact DOM ancestry
  or geometry.
- **No geometry.** No bounding boxes or coordinates. `browser_screenshot`
  returns pixels on the same session, but no set-of-marks overlay, so a vision
  model can look and still has to act through the text tree's IDs.
- **Spatial and visual tasks are out of scope** for the text tree: canvas,
  charts, color, drag distance, map clicks.
- **Truncation.** Names longer than 120 characters and extras longer than 80
  are elided with an ellipsis.
- **Repeated identical text may appear once** because layout containers are
  folded. The text survives; the count of duplicates does not.
- **Images without alt text contribute nothing**, because they have no
  accessible name.

Task-level numbers (when they exist) live in [`EVAL.md`](EVAL.md). The
dev-only harness is under [`eval/`](eval/).

`dump --format raw` prints the uncompressed CDP AX tree, which is the ground
truth the compressor is measured against.

`dump --format fidelity` prints the raw AX nodes and rendered tree from the
same accepted open. It supports fidelity diagnostics on pages whose text can
change between navigations.

### Where the rivals still win

- Cross-browser (Firefox/WebKit), trace viewer, visual regression tooling.
- Pixel-perfect rendering, WebGL, extensions - flashbrowse is an agent layer
  on top of Chrome, not a replacement for it.
- A one-off snapshot where a partially rendered page is acceptable.

---

## Contributing and pull request requirements

Base pull requests on the current `main` branch and update the branch before
requesting review. For code or behavior changes, run these commands against the
final proposed commit:

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --release --workspace
node scripts/soak.mjs --minutes 30 --url <representative-url> --out soak-report.json
```

Run the soak and any before/after benchmarks sequentially on an otherwise idle
machine. Do not run benchmark jobs in parallel. Include the exact commands and
commit SHA, operating system and version, CPU model, physical and logical core
counts, installed RAM, Rust version, and Chrome version in the pull request
description. Include other machine details that could affect performance, such
as architecture or power mode. Report pass/fail and test counts, and attach the
soak JSON report or commit it under `eval/results/`. Summarize soak calls,
errors, p50, p95, max latency, drift, RSS growth, and remaining Chrome
processes.

Every pull request must compare its results with the current latest release
using the same machine, Chrome version, corpus, and settings. Report relevant
changes in latency, accuracy, memory, binary size, and behavior. Mark metrics
that do not apply as `N/A` and explain why. Documentation-only changes may mark
runtime tests and soak results as `N/A`.

Pull requests without the required test results, soak report, or release
comparison may be returned for updates or closed without merging. Maintainers
may request another review after the author updates the code or evidence.

## License

Apache-2.0 - the same license as Playwright and Puppeteer. See `LICENSE`.
