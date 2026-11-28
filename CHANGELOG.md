# Changelog - flashbrowse

All notable changes. Format: [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).

## [Unreleased]

## [1.5.1] - 2026-11-28

### Fixed
- Record network requests and their responses in Chrome's event order. HTTP
  failures remain visible in `browser_network` with `failed_only: true` during
  request bursts, subject to the existing 200-entry log limit.
- Exercise network log overflow without depending on task scheduling, and wait
  for the expected response with a deadline in the browser integration test.

### Changed
- Split browser launch, transport, session, tree compression, and MCP code
  into smaller modules. Moved usage, architecture, benchmarks, and contributor
  instructions into dedicated guides.

## [1.5.0] - 2026-11-27

### Added
- Navigation controls & explicit waits: `browser_navigate` (`back`, `forward`,
  `reload`) and `browser_wait_for` (`text`, `selector`, `url`,
  `network_idle_ms`, traversing open `shadowRoot`s for text and selector
  matches).
- Richer input primitives in `browser_act` and CLI `act`: `double_click`,
  `right_click`, `drag_and_drop` (`target_action_id`), modifier key chords
  (`Control+K`, `Meta+Enter`, `Shift+Tab`), and element-targeted `scroll` via
  `action_id`.
- Framework clickables detection: discovers `addEventListener('click', ...)`
  on `cursor: pointer` elements verified via `DOMDebugger.getEventListeners`
  and React 17+ `__reactProps$.onClick` handlers on visible non-ARIA elements.
- Console & network observability plus lazy request interception:
  `browser_console` (`list`/`clear`, level filter, uncaught exceptions),
  `browser_network` (`list`/`body`/`clear`/`set_headers`, bounded 200-entry
  ring buffer, 64 KiB body cap, `extra_headers` preserved across tab switches),
  and `browser_route` (lazy `Fetch.enable` mocking and request aborting with
  wildcard glob patterns).
- Isolated multi-context sessions: `browser_contexts` (`list`, `create`,
  `select`, `close`) backed by `Target.createBrowserContext` and
  `Target.disposeBrowserContext` sharing one Chromium process across isolated
  cookie/storage contexts.
- Device emulation, permissions, clipped screenshots, and PDF export:
  `browser_emulate` (viewport/mobile/DPR, timezone, locale, geolocation,
  permissions, reset), `browser_screenshot` `action_id` bounding-box clipping,
  and `browser_pdf` confined under `--file-root/pdfs/<name>.pdf`.
- `breaking_v1_5` torture suite, release-profile `mcp_malformed_smoke`
  subprocess test, and a `--suite v15` soak mode that calls all 18 tools. The
  saved 10-minute development report covers all 18 tools.
- Five-minute all-tool smoke on the initial v1.5.0 build: 923 calls, zero
  errors, p50 32 ms, p95 1,345 ms, 0.97x latency drift, 148 kB flashbrowse
  RSS growth, and zero Chrome processes at exit.

### Changed
- Replaced the direct `regex` dependency in `flashbrowse-cdp` with a DevTools
  endpoint parser and enabled only the Tokio features the workspace uses.
- Configured release builds with `opt-level = "z"`, LTO, one codegen unit, and
  symbol stripping. Disabled the unused ZIP Zopfli encoder and tar
  extended-attribute support while retaining ZIP DEFLATE reading and archive
  handling for self-update. Release builds retain `panic = "unwind"`; production
  compilation rejects `panic = "abort"`.

### Fixed
- MCP reports tool panics, including those caught inside browser operations,
  and enforces the 120 s tool deadline. After a failed task stops, connected
  browser contexts preserve their cookies and storage. Page operations clear
  the active tree so action IDs must be refreshed with `browser_wait_for` or
  `browser_open`. Recoverable context create/close failures preserve sibling
  contexts and schedule cleanup for contexts left half-created or half-closed.
  A disconnected browser or tool that does not stop after cancellation resets
  the session manager. A request-frame panic before output returns an MCP error
  and leaves the server available. An input failure returns a transport error
  when stdout remains usable, then closes that connection. A partial response
  cannot be retried safely; a write failure after output starts, including a
  writer panic whose written byte count is unknown, closes that connection.
  Cleanup runs separately. A later `browser_open` can restore
  access to a disconnected Chrome session.
- Contained unexpected CLI panics at runtime setup, command execution, and
  runtime shutdown boundaries, reporting failure through stderr and the exit
  status. MCP catches panics while polling async operations, dropping their
  futures, and polling or dropping MCP I/O objects. Screenshot cleanup
  disconnects the CDP client if a panic interrupts temporary resource-policy
  changes. Shutdown logs now report when the top-level MCP task stopped after
  a panic instead of claiming the server recovered and continued.
- Tab listing stays scoped to the stored browser context after its active tab
  closes. `browser_open` reattaches to another page in that context, or creates
  a blank page there if needed. Isolated contexts are disposed when their CDP
  connection detaches.
- Chrome pipe startup falls back to WebSocket when the CDP handshake fails.
  Windows Chrome launches clear inheritance on the parent's standard handles.
- Scoped CDP background event listeners (`Page.javascriptDialogOpening`,
  `NetActivity`, `ConsoleTracker`, `NetworkLogTracker`, `RouteInterceptor`, and
  `Accessibility.loadComplete` in `open`) to `session_id` via `EventEnvelope`,
  filtered non-popup (`opener_id: None`) targets out of `PopupTracker`, and
  wired `watch` cancellation on session drop/shutdown so closed contexts or
  switched tabs never leak tasks or broadcast buffers.
- Other background event listeners continue after
  `broadcast::error::RecvError::Lagged`, and event channel capacity is 256.
  The Fetch route listener disables interception for its target after a queue
  overflow, releasing unresolved requests whose pause event was lost. All V8
  remote handles in `collect_clickable_non_aria_backends` are released with
  `Runtime.releaseObjectGroup`.
- `browser_open` now returns an error and invalidates stale action IDs when
  the page does not produce a stable, non-empty accessibility tree within the
  15-second settle deadline. It does not return the latest partial snapshot.
- Preserved active `browser_route` rules across `select_tab` (propagating
  restore errors instead of ignoring them), validated route HTTP status codes
  (`100..=599`) and `content_type` headers, attempting `Fetch.continueRequest`
  when fulfill or abort fails, fixed `drag_and_drop`
  scrolled-target coordinate ordering and `buttons: 1` state, cleared `timezone`
  and `locale` in `reset_emulation`, restored `Network.setBlockedURLs` resource
  policy on clipped screenshot errors, validated emulation/PDF/route/scroll
  input bounds, rejected Windows reserved device stems (`CON`, `NUL`, `COM1..9`,
  `LPT1..9`) in `browser_pdf` and `browser_storage_state` plus PDF symlink
  confinement, and fixed multi-wildcard backtracking in `url_matches`.
- Hardened Windows `TcpListener` test servers across `flashbrowse-core` and
  `flashbrowse-mcp` integration tests to drain full HTTP headers per connection
  thread and perform `Shutdown::Write` so Winsock does not emit TCP `RST`.
- Scoped Unix-only test imports so Windows CI can run with warnings denied.
- Fixed persistent session cleanup on macOS by stopping the Chromium process
  group on Unix and retrying temporary profile removal if the directory is
  still being modified during shutdown.
- CDP pipe and WebSocket writers now cancel in-flight writes when the reader
  fails. Pending calls return a disconnected error instead of leaving a writer
  task blocked in I/O.

## [1.4.0] - 2026-11-25

### Added
- Multi-tab and popup support: `browser_tabs` MCP tool (`list`, `select`,
  `close`), real-time `Target.targetCreated` / `Target.attachedToTarget` popup
  tracking with `list_tabs` recovery, delayed popup settle, and opener-first
  fallback on close.
- Native JavaScript dialog handling (`alert`, `confirm`, `prompt`) via
  `Page.javascriptDialogOpening` with safe defaults so dialogs never stall
  automation.
- External browser attach (`--cdp-url`, `--user-data-dir`) and
  `browser_storage_state` MCP tool for importing/exporting cookies and storage
  state with `--storage-state-root` confinement.
- File upload and download support (`browser_upload`, `browser_downloads`) with
  `--file-root` path confinement.
- `browser_fill_form` MCP tool for updating multiple form fields from one
  starting tree and returning one authoritative refreshed tree (with completed
  IDs reported on partial failure).
- POSIX pipe CDP transport (`--remote-debugging-pipe`) for flashbrowse-launched
  Chrome on Unix with WebSocket fallback (WebSocket retained for Windows and
  persistent `--session-id` sessions).
- Open `shadowRoot` coverage in settle mutation tracking and
  `browser_extract_markdown`.
- Action references for visible non-ARIA elements with explicit click handlers
  (`<div onclick>`, `.onclick`), preserving document order without duplicating
  text or wrapping already-actionable elements.
- Configurable `--stealth-profile host-native|windows` (default `host-native`)
  stripping `HeadlessChrome` and keeping `User-Agent`, `navigator.platform`,
  and Client Hints coherent.

### Changed
- Default session tree output now uses `render_nested` (indented hierarchy)
  while keeping flat `render` available to explicit callers.
- Replaced hardcoded post-action sleeps with adaptive `post_action_settle` and
  bounded concurrent child iframe fetching (`MAX_FRAME_CONCURRENCY = 4`).
- Switching MCP `resource_policy` now updates `Network.setBlockedURLs` on the
  live session without restarting Chrome.
- Sequential 15-iteration release benchmark improvements vs pre-latency
  baseline: `cold_open_ms` p95 `1,285.7 ms -> 908.0 ms` (-29.4%),
  `warm_open_ms` p95 `522.8 ms -> 435.2 ms` (-16.8%), and `warm_act_ms` p95
  `445.2 ms -> 63.3 ms` (-85.8%).

## [1.3.0] - 2026-11-24

### Added
- `dump --format fidelity`: prints both the raw CDP AX nodes and the compressed
  rendered tree from the same accepted open for single-observation fidelity
  diagnostics.
- `scripts/settle_bench.mjs`: sequential warm-open settle benchmark harness
  recording per-open SHA-256 tree determinism, latency percentiles, and settle
  stage breakdowns.
- `SettleBreakdown` timing instrumentation in `observability` (and optional
  `FLASHBROWSE_SETTLE_SHADOW=1` diagnostics).

### Changed
- Settle loop now correlates `Page.lifecycleEvent` (`commit` / `load`) and
  `Accessibility.loadComplete` with adaptive DOM quiet windows (250 ms for
  provably static or large trees, 600 ms after observed changes, 1,200 ms for
  small pages that may still hydrate).
- Same-length ARIA/text mutations are now detected reliably during settle
  fingerprinting instead of treating equal-length trees as unchanged.

### Fixed
- Eliminated deadline-stall tail latency on complex and deeply nested pages in
  matched Chrome 147 benchmarks: `a11y.html` p95 improved from 1,370 ms to
  445 ms (-67.5%), `forms.html` p95 from 1,365 ms to 454 ms (-66.7%), and
  `nested-depth.html` p95 from 15,194 ms to 1,364 ms (-91.0%).

## [1.2.1] - 2026-11-23

### Changed
- `flashbrowse update` now downloads the matching platform release archive from
  GitHub Releases and replaces the running executable in place. Pass `--check`
  to only check and report without modifying the installation.

### Fixed
- Windows persistent `--session-id` launches no longer inherit the CLI's
  stdout/stderr pipe handles, preventing callers like `Command::output()` from
  hanging until Chromium exits. `close --session-id` now terminates the Windows
  browser process tree cleanly (`/T`) with null stdio.

## [1.2.0] - 2026-11-23

### Added
- Child `<iframe>` accessibility trees are now spliced directly into the root
  page AX tree in document order. Discovery walks the frame hierarchy via
  `Page.getFrameTree` and discovers child frames, giving agents visibility and
  action references into buttons, inputs, links, and text inside iframes (Fixes #2).
- Out-of-process iframe (OOPIF) coordinate translation: clicks and interactions
  targeted at child frame nodes calculate layout offsets from the frame's
  bounding box and route clicks to their exact viewport coordinates.
- Recursive markdown extraction: `browser_markdown` extracts content from child
  frames and appends them in document order, including iframe titles and text.

### Fixed
- Linux persistent sessions (`--session-id`) no longer terminate when the
  spawning CLI process exits. `PR_SET_PDEATHSIG` is skipped for persistent
  sessions so background browser processes survive across independent command
  invocations (Fixes #1).
- Windows PowerShell installer (`install.ps1`) resolves the latest GitHub
  release redirect reliably when determining the download version tag (PR #3).
- Install documentation updated to list valid, existing installation channels
  only (PR #4).

## [1.1.0] - 2026-11-23

### Added
- `dump --format raw` prints the uncompressed `Accessibility.getFullAXTree`
  JSON, so the compressor's input can be compared with its output. The AX
  protocol types now serialize with the same camelCase field names Chromium
  uses, and a round-trip test guards that.
- Sliders and spin buttons render their current position as `value=...`.
  Without geometry, that number is the only way an agent can reason about
  where a slider sits.
- A fidelity suite compares the compressed tree with the raw AX tree across
  forms, tables, accessible names, unicode, shadow DOM, frames and a spatial
  page. It asserts text recall, action recall, document order, no duplication,
  hidden-text exclusion, state round-trip and the 120-character truncation
  bound. `scripts/fidelity.mjs` runs the same checks against real URLs.
- Render modes for eval: `SemanticNode::depth`, `render_nested`,
  `dump --format axtree-nested`, and `dump --no-prune` (full non-ignored AX
  tree). `compress_with_options` exposes the pruned vs full split.
- Dev-only task eval under `eval/`: pilot tasks, MCP runner, Playwright
  success checks, and `report.mjs` for markdown summaries.
- MCP `browser_evaluate` runs JavaScript on the live session so task success
  checks use the same page the agent mutated, not a fresh browser tab.
- ARIA `gridcell` support in `compressor.rs` (`role="grid"` / `role="gridcell"`
  rows now collapse into `Row: ... | [N]` lines without duplicate `Gridcell:`
  output).
- Brand icon (`assets/icon.png`, `assets/icon.svg`).

### Fixed
- `checked`, `selected` and `expanded` were missing from the tree. Chromium
  sends `checked` as the string `"true"` rather than a JSON bool, and extras
  were only rendered on actionable lines, so a checked checkbox and a selected
  option looked exactly like their unselected siblings.
- An input value appeared twice (`value="Ada"` plus `Text: Ada`), and an icon
  link's `alt` text appeared twice (the link name plus an image line).
- Long or wide table rows (`> 120` chars) no longer truncate trailing `[N]`
  action references off the `Row:` summary line; `truncate_row` budgets
  non-ref cells while preserving `[N]` refs.

## [1.0.2] - 2026-11-22

### Added
- `open`, `dump`, `bench` and MCP `browser_open` accept a scheme-less URL.
  `flashbrowse open example.com` now works like an address bar: `https://` is
  assumed, local hosts (`localhost:3000`, `127.0.0.1`, `::1`, `*.local`,
  RFC1918 and link-local addresses) use `http://`, and an explicit scheme is
  never rewritten. Before this, a bare host reached `Page.navigate` verbatim
  and Chrome answered `-32000: Cannot navigate to invalid URL`.
- Input that cannot be a URL now fails before a browser launches, with the fix
  in the message: a bare path suggests `file:///`, a search query says
  flashbrowse opens URLs, and an unsupported scheme (`javascript:`,
  `mailto:`) is named. The MCP allowlist still runs on the normalized URL, so
  `file://` remains CLI-only.

## [1.0.1] - 2026-11-22

### Added
- `ResourcePolicy` for network blocking: `agent-default` (default), `text-only`,
  `full-render` (loads images/fonts; tree tokens unchanged). CLI:
  `--resource-policy`, `--full-render` (conflicts with `--text-only`), env
  `FLASHBROWSE_RESOURCE_POLICY`.
- Trustworthy `browser_screenshot`: lifts fonts and images during prepare,
  network-idle wait, lazy-content scroll, configurable `timeout_ms` / MCP
  `prepare_lazy_content`. Use the same flashbrowse session instead of a second
  browser MCP.
- MCP `browser_open` accepts `resource_policy`; tool descriptions steer agents
  to `browser_screenshot` on the active session.
- `CdpDriver` seam in `flashbrowse-core` for future engines.
- `flashbrowse-bidi` crate: Firefox binary discovery (`find_firefox`, doctor
  informational check); full BiDi sessions are not wired yet.
- **Observability**: every CLI command and every MCP tool call appends a
  record to `~/.cache/flashbrowse/metrics.jsonl` (op, target, duration, agent
  payload tokens, action count, ok/error).
- `flashbrowse stats [--limit N] [--json]`: latency percentiles (p50/p95),
  token totals, action averages and error rate over the metrics trail.
- `flashbrowse doctor [--json]`: environment diagnostics (Chromium presence
  and version, managed Chrome, cache-dir writability, free temp space,
  available memory, stored sessions, orphaned browser processes). Exits `1`
  when a blocking check fails.
- `flashbrowse update [--check]`: update-channel aware release check.
  Channel selected via `FLASHBROWSE_UPDATE_CHANNEL` = `stable` (default) |
  `beta` | `nightly` | `off`.
- `--log-format text|json` global flag: JSON logs are one object per line,
  ready for a log shipper. Logs always go to stderr (stdout stays pure
  JSON-RPC for MCP).
- `scripts/soak.mjs`: soak/load harness that drives a long-running MCP
  session and reports throughput, per-tool latency, first-tenth vs
  last-tenth drift, RSS growth, orphan-browser count and error list. CI runs
  a 5-minute version on every push.
- CI caches the managed Chrome-for-Testing download, keyed by the exact
  Chrome version, so the browser is fetched once instead of once per job.
- `FLASHBROWSE_CHROME_VERSION` pins the Chrome-for-Testing version used by
  `flashbrowse setup`.
- CDP `dispatch_frame` unit tests; WebSocket connect retries on early Chrome
  startup races.
- Soak exit checks for latency drift (> 1.2x) and flashbrowse RSS growth
  (> 1.5x).
- CI uploads `doctor --json` on Linux.

### Fixed (screenshot, policy, navigation)
- `browser_screenshot` no longer burns the full prepare timeout on a page whose
  images are blocked. The readiness check required `naturalWidth > 0`, which a
  permanently broken image never reaches; it now waits on `img.complete`
  (loaded *or* failed). A screenshot of an image-bearing page went from
  ~8.2 s to ~0.4 s.
- Blocked images are now actually rendered. Lifting the blocklist alone did
  nothing: `Network.setBlockedURLs` only affects *future* requests and Chrome
  never retries a blocked one. The capture now re-requests the images, so an
  agent-default screenshot shows the same pixels as a `full-render` one.
- The screenshot prepare no longer resets the scroll position. The agent asked
  for a picture of what it was looking at; the viewport is now restored after
  the lazy-content scroll.
- The screenshot prepare timeout is capped (500 ms - 60 s). An agent could
  previously ask for an unbounded wait and pin the serial MCP server.
- An invalid MCP `resource_policy` is now an `InvalidParams` error instead of
  being silently ignored (the CLI already rejected it).
- Resource-policy flag precedence is explicit and deterministic:
  `--resource-policy` > `--full-render`/`--text-only` > env > default.
  Conflicting flags now fail loudly.
- `doctor` no longer spawns `firefox --version`; it only reports the path.
- Plain text in generic containers (`<div>`, `<span>`) is no longer dropped.
  Chromium gives every `StaticText` an `InlineTextBox` child; treating that as
  "has children, so it is a concatenation duplicate" made the compressor drop
  every piece of plain text on the page. `InlineTextBox` is now recognized as
  layout scaffolding, and `StaticText` renders as a `Text:` line.
- A minimal page (one or two semantic nodes) no longer waits out the whole
  15 s settle deadline: the exit condition required `nodes.len() >= 3`, which
  a tiny page never reaches. Such pages now open in ~0.6 s instead of ~16 s.
- The `Title:` header no longer comes up blank about one page in five. The
  title was read right after `navigate`, before the new document was parsed;
  it is now re-read once the page has settled.
- Content arriving shortly after `readyState == "complete"` (hydration
  timers up to ~1.2 s) is no longer truncated: the small-tree quiet window
  grew from 800 ms to 1200 ms.

### Fixed (accuracy)
- **Table contents are no longer dropped.** Table rows are now emitted as one
  line per row (`a | b | c`) instead of one line per cell, and the row's own
  accessible name (empty in Chromium) no longer hides the cell text.
- **List item text is no longer dropped.** A `listitem` keeps its text.
- **Output is in document order.** Chromium returns the accessibility tree
  breadth-first, which interleaved siblings with their descendants (a table
  header row could arrive after its body rows). The compressor now rebuilds
  the tree and walks it depth-first.
- An actionable element inside a container no longer causes its text to be
  emitted twice.
- `listmarker` bullet glyphs are dropped (decoration, not content).
- Unnamed links no longer occupy action IDs in the action map when dropped.

### Fixed
- `doctor` memory and orphan-browser checks on macOS and Windows.
- Metrics `record_op` for `setup`, `close`, `bench`, and failed `act` paths.
- CI soak job Chrome cache key (`ubuntu-latest`, not empty `matrix.os`).
- The Windows build did not compile: `tokio::signal::unix` was called
  unconditionally, so `cargo build` and clippy died with E0433 and a cascading
  E0599. Signal handling is now unix-only, with console Ctrl+C on Windows;
  `libc` is a unix-only dependency and the `ps`-only orphan scan helper is
  gated so clippy `-D warnings` passes on every OS.
- An explicit Firefox path is authoritative. `find_firefox(Some(p))` used to
  fall back to PATH when `p` did not exist, which silently ran a different
  browser than the caller pinned.
- The soak memory gate no longer fails on allocator warm-up. It compared the
  first RSS sample against the last, so a cold process climbing from 11 MB to
  18 MB in its first 30 s read as a leak; the 5 minute CI soak failed on a run
  whose RSS was flat for the remaining 4.5 minutes (and ended below its own
  30 s peak). The gate now baselines after a warm-up window and fails only on
  real growth: a 1.5x ratio with an 8 MiB floor, or a fitted trend (R2 >= 0.8,
  which a one-time step cannot reach) that would exceed 1.5x within an hour.
  `node scripts/soak.mjs --check-report <file>` re-runs the gates on a saved
  report.

### Security
- `url` 2.5.8 carried RUSTSEC-2026-0592: `Url::origin()` recursed once per
  `blob:` prefix with no depth limit, so a deeply nested `blob:` URL overflowed
  the stack and aborted the process. Bumped to 2.5.9, which takes the inner
  origin only for `http`, `https` and `file` and never recurses
  (`form_urlencoded` 1.2.3 comes with it). `url` arrives transitively through
  `reqwest`; flashbrowse never calls `Url::origin()`, but the bump keeps
  `cargo audit` clean and costs nothing.
- `zip` 4.6.1 carried six advisories (RUSTSEC-2026-0429, -0367, -0548,
  -0368, -0369, -0545). Upgraded to 9.8.1 with only the deflate feature
  enabled, since Chrome-for-Testing archives are deflate/store and the AES,
  LZMA, Zstd, bzip2 and ppmd decoders were never exercised. `cargo audit` is
  clean.

### Changed
- License is **Apache-2.0** (the same license as Playwright and Puppeteer).
  `LICENSE` added; `Cargo.toml` and the README updated.

## [1.0.0] - 2026-11-14

First public release. Single Rust binary, zero Python/Node/JVM, raw CDP over
WebSocket, AXTree-first compression, native MCP stdio server.

### Added
- `flashbrowse setup`: managed Chrome-for-Testing download
  (`~/.cache/flashbrowse/chrome/`, ~120 MB, once) - no manual Chrome install.
- MCP stdio server (protocol 2025-06-18, falls back to 2025-03-26 / 2024-11-05)
  with four tools: `browser_open`, `browser_act`, `browser_extract_markdown`,
  `browser_screenshot`.
- CLI: `mcp`, `open`, `act`, `dump`, `bench`, `setup`, `close` with
  `--chrome-path`, `--text-only`, `--session-id` persistence.
- `select` action (combobox/dropdown) via `DOM.resolveNode` +
  `Runtime.callFunctionOn` - never opens the native popup.
- Actionability checks for clicks (visible + stable, Playwright parity).
- Adaptive settle engine: cheap DOM polling, network-idle gate, AX-vs-DOM
  readiness gate and content-fingerprint verification, so the tree is never
  truncated (byte-identical output across repeated opens).
- Network interception: fonts, trackers and images blocked by default
  (image `alt` text survives in the AX tree); CSS deliberately not blocked.
- Stealth script: `navigator.webdriver`, `window.chrome`, plugins, mimeTypes,
  languages, platform/vendor, CPU/memory, permissions, WebGL masking.
- One-script installers (`install.sh`, `install.ps1`), CI on Linux/macOS/Windows,
  release workflow for 5 targets.

### Fixed (correctness)
- `AXNode` now matches the CDP schema exactly (`nodeId`, `backendDOMNodeId`,
  `value`, `ignoredReasons`, `properties`) - the old mapping silently dropped
  every action ID.
- Heading levels read from `properties` (was always 1).
- macOS select-all uses Cmd (Meta=4), not Shift (8).
- Key events send the DOM `code` (`a` -> `KeyA`).
- A malformed `tools/call` no longer terminates the MCP server.
- Stealth and network blocking are re-applied on session re-attach (they are
  per-`sessionId` settings).
- `select` no longer leaves the native dropdown open.
- Profile directories are removed reliably (synchronously, not in a thread
  that process exit could kill).
- `close` verifies the pid still belongs to Chrome before killing it; corrupt
  session files are cleaned instead of blocking cleanup.
- Invalid session ids are rejected before detaching (no orphaned browser).
- Oversize CDP frames fail their pending calls instead of timing out.

### Performance
- 3x - 23x fewer agent tokens than Playwright/Puppeteer accessibility
  snapshots (measured against the full tree, `interestingOnly: false`).
- 1.08x - 1.10x faster than Playwright/Puppeteer on the agent loop
  (524 ms vs 564/579 ms median, local page, same Chrome).
- 98.8% token reduction on a 50k-node synthetic AX tree (182 ms).
- 234 ms average for 20 sequential opens, 286 ms for a 100-action storm.

> The agent payload grew from 813 to 1215 tokens on the local benchmark page
> because the compressor stopped dropping table cells. The old number was
> smaller because the data was missing.

### Known limits (documented in README)
- Stealth defeats naive bots, not canvas/audio/TLS fingerprinting.
- MCP `browser_open` is URL-allowlisted (`http`, `https`, `data`, `about:blank`).
- Markdown extraction is lightweight, not a full Readability port.
- macOS/Windows binaries are ad-hoc signed; notarization needs an Apple
  Developer certificate (see `RELEASE.md`).
