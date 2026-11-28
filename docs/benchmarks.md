# Benchmark results and methods

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
- **v1.5.0 development 5-minute mixed-page soak** ([report](../eval/results/soak-v1.5.0-panic-hardening-smoke.json)): 863 calls across a local fixture and `example.com`, **0 errors**, `p50 = 39 ms`, `p95 = 1,356 ms`, `max = 1,436 ms`, `drift_ratio = 0.975`, leak gate passed (ratio `1.009`), and **0** Chrome processes remained at exit.
- **v1.5.0 development 5-minute all-tool soak** (`--suite v15`, all 18 tools, [report](../eval/results/soak-v1.5.0-panic-hardening-v15.json)): 923 calls on a local fixture, **0 errors**, `p50 = 32 ms`, `p95 = 1,344 ms`, `max = 1,640 ms`, `drift_ratio = 0.939`, leak gate passed (ratio `1.005`), and **0** Chrome processes remained at exit.
- **v1.5.0 development 45-minute mixed-page soak** ([report](../eval/results/soak-v1.5.0-panic-hardening-heavy.json)): 7,522 calls across a local fixture and `example.com`, **0 errors**, `p50 = 40 ms`, `p95 = 1,358 ms`, `max = 2,267 ms`, `drift_ratio = 1.077`, leak gate passed (ratio `0.991`), and **0** Chrome processes remained at exit.
- **Initial v1.5.0 build, 5-minute all-tool smoke** ([report](../eval/results/soak-v1.5.0-final-smoke.json)): 923 calls, **0 errors**, `p50 = 32 ms`, `p95 = 1,345 ms`, `max = 1,715 ms`, `drift_ratio = 0.97`, flashbrowse RSS growth `+148 kB`, and **0** Chrome processes at exit.

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

Fidelity and pilot task checks live in [`EVAL.md`](../EVAL.md) (dev-only harness
under [`eval/`](../eval/)):
- **Fidelity oracle:** the current test corpus contains 14 local fixture pages.
  A 17-URL development sweep (14 fixtures and 3 public pages) reported 0 failed
  pages and 0 missing strings, with 90.1% to 98.5% byte reduction versus raw
  AXTree JSON ([report](../eval/results/fidelity-v1.5.0-panic-hardening.json)).
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

Task-level numbers (when they exist) live in [`EVAL.md`](../EVAL.md). The
dev-only harness is under [`eval/`](../eval/).

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
