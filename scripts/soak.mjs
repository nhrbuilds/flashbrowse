#!/usr/bin/env node
/**
 * flashbrowse soak test.
 *
 * Drives a long-running MCP stdio session against a mix of local and real
 * pages, then reports:
 *   - throughput (tools/second) over time
 *   - latency drift (p50 of the first tenth vs the last tenth)
 *   - error rate per tool
 *   - process health: RSS growth of the browser and the flashbrowse process,
 *     plus a count of orphaned Chrome processes
 *
 * Usage:
 *   node scripts/soak.mjs --minutes 90 --url http://localhost:8099/ --url https://example.com
 *
 * Flags:
 *   --minutes <n>     how long to soak (default 60)
 *   --url <u>         a URL to include; repeatable
 *   --interval <ms>   pause between iterations (default 250)
 *   --screenshot      include a screenshot tool call every N iterations
 *   --suite <mode>    default | v15 (v15 spins up a local HTTP page and exercises all 18 MCP tools)
 *   --out <file>      write the JSON report here (default: soak-report.json)
 *   --bin <path>      flashbrowse binary (default: target/release/flashbrowse)
 *   --check-report <file>
 *                     re-run the exit gates on a saved report, no browser
 *
 * The soak is intentionally SEQUENTIAL: one tool call at a time, one page at
 * a time. Running tools in parallel would measure the machine, not the tool.
 *
 * Exit codes:
 *   - non-zero if error rate > 5%
 *   - non-zero if latency drift (last-tenth p50 / first-tenth p50) > 1.2x
 *   - non-zero if flashbrowse steady-state RSS grew > 50% AND > 8 MiB after
 *     the warm-up window (see memoryGate; a cold process is not a leak)
 */

import { spawn } from "node:child_process";
import { existsSync, writeFileSync, readFileSync, mkdtempSync, rmSync } from "node:fs";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { execSync } from "node:child_process";

// ---------------------------------------------------------------- arguments

function parseArgs(argv) {
  const args = {
    minutes: 60,
    urls: [],
    interval: 250,
    screenshotEvery: 20,
    suite: "default",
    out: "soak-report.json",
    bin: "target/release/flashbrowse",
    checkReport: null,
  };
  for (let i = 2; i < argv.length; i++) {
    const a = argv[i];
    switch (a) {
      case "--minutes":
        args.minutes = Number(argv[++i]);
        break;
      case "--url":
        args.urls.push(argv[++i]);
        break;
      case "--interval":
        args.interval = Number(argv[++i]);
        break;
      case "--screenshot":
        args.screenshotEvery = Number(argv[++i]);
        break;
      case "--suite":
        args.suite = argv[++i];
        break;
      case "--out":
        args.out = argv[++i];
        break;
      case "--bin":
        args.bin = argv[++i];
        break;
      case "--check-report":
        args.checkReport = argv[++i];
        break;
      case "--help":
        console.log(readFileSync(new URL(import.meta.url)).toString().split("*/")[0]);
        process.exit(0);
        break;
      default:
        console.error(`unknown flag: ${a}`);
        process.exit(2);
    }
  }
  if (args.urls.length === 0) args.urls.push("https://example.com");
  return args;
}

const args = parseArgs(process.argv);

// Re-gate a saved report without spawning anything. Useful when the gate
// logic changes and old runs have to be re-judged from the same evidence.
if (args.checkReport) {
  const path = resolve(args.checkReport);
  const saved = JSON.parse(readFileSync(path, "utf8"));
  const gate = evaluateGates({
    timeline: saved.timeline,
    durationSec: saved.durationSec,
    errorCount: saved.errorCount,
    toolCalls: saved.toolCalls,
    driftRatio: saved.drift?.drift_ratio,
  });
  const m = gate.memory;
  const signed = `${m.growth_kb >= 0 ? "+" : ""}${m.growth_kb}kB`;
  const slope = `${m.slope_kb_per_hour >= 0 ? "+" : ""}${m.slope_kb_per_hour}kB/h`;
  console.log(`re-checking ${path}`);
  console.log(
    `error rate    ${saved.errorCount ?? 0}/${saved.toolCalls ?? 0}${gate.errorFail ? " FAIL" : " ok"}`
  );
  console.log(
    `latency drift ${saved.drift?.drift_ratio}x${gate.driftFail ? " FAIL" : " ok"}`
  );
  console.log(
    `memory steady ${m.baseline_kb}kB -> ${m.end_kb}kB (${m.ratio}x, ${signed}; trend ${slope} ` +
      `r2 ${m.trend_r2}, cold ${m.cold_start_kb}kB, ${m.warmup_sec}s warm-up)${gate.memoryFail ? " FAIL" : " ok"}`
  );
  process.exit(gate.errorFail || gate.driftFail || gate.memoryFail ? 1 : 0);
}

const BIN = resolve(args.bin);
if (!existsSync(BIN)) {
  console.error(`flashbrowse binary not found: ${BIN}`);
  console.error("build it first: cargo build --release");
  process.exit(2);
}

// ---------------------------------------------------------------- MCP client

class McpClient {
  constructor(child) {
    this.child = child;
    this.nextId = 1;
    this.pending = new Map();
    this.buffer = "";
    child.stdout.setEncoding("utf8");
    child.stdout.on("data", (chunk) => this.#onData(chunk));
    child.stderr.on("data", (d) => {
      if (process.env.SOAK_VERBOSE) process.stderr.write(`[fb] ${d}`);
    });
  }

  #onData(chunk) {
    this.buffer += chunk;
    let idx;
    while ((idx = this.buffer.indexOf("\n")) >= 0) {
      const line = this.buffer.slice(0, idx).trim();
      this.buffer = this.buffer.slice(idx + 1);
      if (!line) continue;
      let msg;
      try {
        msg = JSON.parse(line);
      } catch {
        continue;
      }
      if (msg.id !== undefined && this.pending.has(msg.id)) {
        const { resolve: res, reject } = this.pending.get(msg.id);
        this.pending.delete(msg.id);
        if (msg.error) reject(new Error(msg.error.message || "mcp error"));
        else res(msg.result);
      }
    }
  }

  request(method, params) {
    const id = this.nextId++;
    const payload = { jsonrpc: "2.0", id, method, params };
    return new Promise((res, rej) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        rej(new Error(`timeout after 180s: ${method}`));
      }, 180_000);
      this.pending.set(id, {
        resolve: (v) => {
          clearTimeout(timer);
          res(v);
        },
        reject: (e) => {
          clearTimeout(timer);
          rej(e);
        },
      });
      this.child.stdin.write(JSON.stringify(payload) + "\n");
    });
  }

  notify(method, params) {
    this.child.stdin.write(JSON.stringify({ jsonrpc: "2.0", method, params }) + "\n");
  }
}

function textOf(result) {
  if (!result || !Array.isArray(result.content)) return "";
  return result.content
    .filter((c) => c.type === "text")
    .map((c) => c.text)
    .join("\n");
}

// ---------------------------------------------------------------- monitoring

function rssKb(pid) {
  try {
    const status = readFileSync(`/proc/${pid}/status`, "utf8");
    const m = status.match(/VmRSS:\s+(\d+)\s+kB/);
    return m ? Number(m[1]) : 0;
  } catch {
    return 0;
  }
}

function chromeCount() {
  try {
    const out = execSync("ps -eo args 2>/dev/null || true", { encoding: "utf8" });
    return out.split("\n").filter((l) => l.includes("chrome") && l.includes("flashbrowse-")).length;
  } catch {
    return 0;
  }
}

function chromeRssKb() {
  try {
    const out = execSync(
      "ps -eo pid,rss,args 2>/dev/null || true",
      { encoding: "utf8" }
    );
    let total = 0;
    for (const line of out.split("\n")) {
      if (line.includes("chrome") && line.includes("flashbrowse-")) {
        const parts = line.trim().split(/\s+/);
        total += Number(parts[1]) || 0;
      }
    }
    return total;
  } catch {
    return 0;
  }
}

// ---------------------------------------------------------------- helpers

function percentile(sorted, p) {
  if (!sorted.length) return 0;
  const i = Math.min(sorted.length - 1, Math.floor(sorted.length * p));
  return sorted[i];
}

function summarize(samples) {
  if (!samples.length) return { n: 0 };
  const ms = samples.map((s) => s.ms).sort((a, b) => a - b);
  const tokens = samples.map((s) => s.tokens).sort((a, b) => a - b);
  return {
    n: samples.length,
    errors: samples.filter((s) => !s.ok).length,
    p50_ms: percentile(ms, 0.5),
    p95_ms: percentile(ms, 0.95),
    max_ms: ms[ms.length - 1] ?? 0,
    p50_tokens: percentile(tokens, 0.5),
    total_tokens: tokens.reduce((a, b) => a + b, 0),
  };
}

// ---------------------------------------------------------------- leak gate

function median(values) {
  if (!values.length) return 0;
  const s = [...values].sort((a, b) => a - b);
  const mid = Math.floor(s.length / 2);
  return s.length % 2 === 1 ? s[mid] : Math.round((s[mid - 1] + s[mid]) / 2);
}

function linearFit(points) {
  const n = points.length;
  if (n < 3) return { slope_kb_per_sec: 0, r2: 0 };
  const meanT = points.reduce((a, p) => a + p.t, 0) / n;
  const meanRss = points.reduce((a, p) => a + p.rssFb, 0) / n;
  let sxy = 0;
  let sxx = 0;
  let syy = 0;
  for (const p of points) {
    const dt = p.t - meanT;
    const dr = p.rssFb - meanRss;
    sxy += dt * dr;
    sxx += dt * dt;
    syy += dr * dr;
  }
  if (sxx === 0 || syy === 0) return { slope_kb_per_sec: 0, r2: 0 };
  return {
    slope_kb_per_sec: sxy / sxx,
    r2: (sxy * sxy) / (sxx * syy),
  };
}

/**
 * Leak gate.
 *
 * RSS is not flat from iteration one. The first CDP frames, the action map and
 * the JSON buffers fault in heap pages, so a cold process climbs for the first
 * ~30 s and then plateaus. Comparing timeline[0] against the last sample
 * therefore measures that warm-up, and a short run looks like a leak while a
 * long run amortizes it away. That is exactly what happened to the 5 minute CI
 * soak: RSS went 11.3 MB -> 18.3 MB in the first 30 s, then held ~17.9 MB for
 * the remaining 4.5 minutes, and the gate failed on the warm-up.
 *
 * Everything below is measured after the warm-up window, so the baseline is a
 * steady-state process, not a cold one. Two independent signals fail the gate:
 *
 *   ratio:  end median > 1.5x baseline median AND growth > 8 MiB. Catches a
 *           process that doubles. The absolute floor keeps 1.5x of a ~11 MB
 *           baseline (which is allocator noise between windows) from tripping.
 *
 *   trend:  a least-squares line through the steady-state samples with R2 >=
 *           0.8 whose slope, projected one more hour, would push RSS past
 *           1.5x of the baseline. A 5 minute run cannot see a slow leak as a
 *           ratio: a leak adding 5.4 MB in 4 minutes is only 1.35x by the end,
 *           yet it is ~500 MB/h and must fail. R2 guards against a one-time
 *           step (a cache filling, then holding) being read as a leak, and
 *           0.8 is safe by construction: the R2 of a pure two-level step is
 *           3p(1-p), which peaks at 0.75 when the step sits at the midpoint.
 *           Do not lower it below 0.75.
 *
 * `cold_start_kb` is reported for context. It is not a gate.
 */
function memoryGate(timeline, durationSec) {
  const points = (timeline ?? []).filter((p) => p && p.rssFb > 0);
  if (points.length === 0) {
    return {
      ok: true,
      evaluated: false,
      warmup_sec: 0,
      window_sec: 0,
      cold_start_kb: 0,
      baseline_kb: 0,
      end_kb: 0,
      growth_kb: 0,
      ratio: null,
      slope_kb_per_hour: 0,
      trend_r2: 0,
      projected_kb_1h: 0,
      ratio_fail: false,
      trend_fail: false,
    };
  }
  const warmupSec = Math.min(60, Math.max(5, Math.round(durationSec * 0.2)));
  const windowSec = Math.max(10, Math.round((durationSec - warmupSec) / 4));
  const steady = points.filter((p) => p.t >= warmupSec);
  const baselinePoints = steady.filter((p) => p.t < warmupSec + windowSec);
  const endPoints = points.filter((p) => p.t >= durationSec - windowSec);
  const baselineKb = median(
    (baselinePoints.length ? baselinePoints : steady.length ? steady : points).map(
      (p) => p.rssFb
    )
  );
  const endKb = median((endPoints.length ? endPoints : points).map((p) => p.rssFb));
  const growthKb = endKb - baselineKb;
  const ratio = baselineKb > 0 ? Number((endKb / baselineKb).toFixed(3)) : null;

  const fit = linearFit(steady.length ? steady : points);
  const slopePerHour = Math.round(fit.slope_kb_per_sec * 3600);
  const projectedKb = Math.round(endKb + fit.slope_kb_per_sec * 3600);

  const ratioFail = ratio != null && ratio > 1.5 && growthKb > 8192;
  const trendFail =
    fit.r2 >= 0.8 &&
    fit.slope_kb_per_sec > 0 &&
    baselineKb > 0 &&
    projectedKb > baselineKb * 1.5;

  return {
    ok: !(ratioFail || trendFail),
    evaluated: true,
    warmup_sec: warmupSec,
    window_sec: windowSec,
    cold_start_kb: points[0].rssFb,
    baseline_kb: baselineKb,
    end_kb: endKb,
    growth_kb: growthKb,
    ratio,
    slope_kb_per_hour: slopePerHour,
    trend_r2: Number(fit.r2.toFixed(3)),
    projected_kb_1h: projectedKb,
    ratio_fail: ratioFail,
    trend_fail: trendFail,
  };
}

function evaluateGates({ timeline, durationSec, errorCount, toolCalls, driftRatio }) {
  const memory = memoryGate(timeline, durationSec);
  return {
    errorFail: (errorCount ?? 0) > (toolCalls ?? 0) * 0.05,
    driftFail: driftRatio != null && driftRatio > 1.2,
    memoryFail: !memory.ok,
    memory,
  };
}

// ---------------------------------------------------------------- local v1.5 test fixture

async function startV15FixtureServer() {
  const pageHtml = `<!doctype html><html><head><meta name="viewport" content="width=device-width,initial-scale=1"><title>Soak15</title></head><body>
<h1>Soak V15</h1>
<input id="f1" aria-label="FullName" />
<input id="up" type="file" aria-label="UploadInput" />
<button id="dbl" ondblclick="this.textContent='DblDone'">DblBtn</button>
<button id="ctx" oncontextmenu="event.preventDefault();this.textContent='CtxDone'">CtxBtn</button>
<button id="drag">DragSrc</button>
<button id="drop" onmouseup="this.textContent='DropDone'">DropDst</button>
<div id="ael" style="cursor:pointer">AelItem</div>
<div id="scroller" tabindex="0" style="width:160px;height:50px;overflow:auto"><div style="height:300px">Inner</div></div>
<div id="shadow-host"></div>
<script>
console.log('soak-tick');
document.getElementById('ael').addEventListener('click', () => { document.title = 'AelClicked'; });
setTimeout(() => {
  const r = document.getElementById('shadow-host').attachShadow({mode: 'open'});
  r.innerHTML = '<p class="ready-shadow">ShadowUp</p>';
}, 50);
</script></body></html>`;
  const twoHtml = `<!doctype html><html><head><title>SoakTwo</title></head><body><h1>Page Two</h1></body></html>`;
  const server = createServer((req, res) => {
    if (req.url === "/two") {
      res.writeHead(200, { "Content-Type": "text/html; charset=utf-8" });
      res.end(twoHtml);
    } else if (req.url === "/api/echo") {
      res.writeHead(200, { "Content-Type": "application/json" });
      res.end(JSON.stringify({ live: true }));
    } else {
      res.writeHead(200, { "Content-Type": "text/html; charset=utf-8" });
      res.end(pageHtml);
    }
  });
  await new Promise((res) => server.listen(0, "127.0.0.1", res));
  const { port } = server.address();
  return { server, baseUrl: `http://127.0.0.1:${port}` };
}

// ---------------------------------------------------------------- soak run

const fileRoot = args.suite === "v15" ? mkdtempSync(join(tmpdir(), "flashbrowse-soak-")) : null;
const storageRoot =
  args.suite === "v15" ? mkdtempSync(join(tmpdir(), "flashbrowse-soak-state-")) : null;
if (fileRoot) {
  writeFileSync(join(fileRoot, "upload.txt"), "soak-upload-payload\n");
}
const v15Server = args.suite === "v15" ? await startV15FixtureServer() : null;
const mcpArgs = ["mcp", "--log-format", "json"];
if (fileRoot) mcpArgs.push("--file-root", fileRoot);
if (storageRoot) mcpArgs.push("--storage-state-root", storageRoot);

const child = spawn(BIN, mcpArgs, {
  stdio: ["pipe", "pipe", "pipe"],
});
const client = new McpClient(child);

const samples = [];
const perTool = new Map();
const timeline = [];
const errors = [];

let iteration = 0;
const startedAt = Date.now();
const deadline = startedAt + args.minutes * 60_000;

// Progress printer (one line every 30 s).
let lastPrint = 0;
function progress() {
  const now = Date.now();
  if (now - lastPrint < 30_000) return;
  lastPrint = now;
  const elapsedMin = ((now - startedAt) / 60_000).toFixed(1);
  const s = summarize(samples);
  const chrome = chromeCount();
  console.log(
    `[soak ${elapsedMin}m] iters=${iteration} tools=${samples.length} ` +
      `err=${s.errors ?? 0} p50=${s.p50_ms ?? 0}ms p95=${s.p95_ms ?? 0}ms ` +
      `tokens=${s.total_tokens ?? 0} chrome_procs=${chrome} ` +
      `rss_fb=${rssKb(child.pid)}kB rss_chrome=${chromeRssKb()}kB`
  );
}
const progressTimer = setInterval(progress, 5_000);

// Wait for the server to be ready.
const init = await client.request("initialize", {
  protocolVersion: "2025-06-18",
  capabilities: {},
  clientInfo: { name: "soak", version: "1.0.0" },
});
client.notify("notifications/initialized", {});

console.log(
  `soak starting: ${args.minutes} min, ${args.urls.length} url(s), ` +
    `interval ${args.interval}ms, pid ${child.pid}`
);
console.log(`initialize -> ${JSON.stringify(init?.serverInfo ?? {})}`);

// Action inventory for the current tree.
function actionIds(tree) {
  const ids = [];
  for (const line of tree.split("\n")) {
    const m = line.match(/^\s*\[(\d+)\]/);
    if (m) ids.push(Number(m[1]));
  }
  return ids;
}

async function timed(tool, argsObj) {
  const t0 = Date.now();
  let ok = true;
  let tokens = 0;
  let err = null;
  let text = "";
  try {
    const res = await client.request("tools/call", { name: tool, arguments: argsObj });
    text = textOf(res);
    tokens = Math.ceil(text.length / 4);
    if (res && res.isError) {
      ok = false;
      err = text.slice(0, 200);
    }
  } catch (e) {
    ok = false;
    err = String(e).slice(0, 200);
  }
  const sample = {
    tool,
    ms: Date.now() - t0,
    tokens,
    ok,
    err,
    at: Date.now(),
  };
  samples.push(sample);
  const bucket = perTool.get(tool) ?? [];
  bucket.push(sample);
  perTool.set(tool, bucket);
  if (!ok) {
    errors.push({ iteration, tool, err });
    console.error(`[soak] iteration ${iteration} ${tool} FAILED: ${err}`);
  }
  // The payload text is returned but NOT kept in the sample: the report
  // stays small and the timeline stays readable.
  return { ...sample, text };
}

let currentUrlIndex = 0;

while (Date.now() < deadline) {
  iteration++;
  if (args.suite === "v15" && v15Server) {
    const base = v15Server.baseUrl;
    const open = await timed("browser_open", { url: `${base}/` });
    progress();

    let currentTree = open.text ?? "";
    const idFor = (label, fallback) => {
      const line = currentTree.split("\n").find((l) => l.includes(label));
      const m = line?.match(/^\s*\[(\d+)\]/);
      return m ? Number(m[1]) : fallback;
    };

    await timed("browser_wait_for", { selector: ".ready-shadow", timeout_ms: 3000 });
    const filled = await timed("browser_fill_form", {
      fields: [{ action_id: idFor("FullName", 1), value: "Ada" }],
    });
    if (filled.ok && filled.text) currentTree = filled.text;
    const uploaded = await timed("browser_upload", {
      action_id: idFor("UploadInput", 2),
      files: ["upload.txt"],
    });
    if (uploaded.ok && uploaded.text) currentTree = uploaded.text;
    const dbl = await timed("browser_act", {
      action_id: idFor("DblBtn", 3),
      action: "double_click",
    });
    if (dbl.ok && dbl.text) currentTree = dbl.text;
    const ctxClick = await timed("browser_act", {
      action_id: idFor("CtxBtn", 4),
      action: "right_click",
    });
    if (ctxClick.ok && ctxClick.text) currentTree = ctxClick.text;
    const drag = await timed("browser_act", {
      action_id: idFor("DragSrc", 5),
      target_action_id: idFor("DropDst", 6),
      action: "drag_and_drop",
    });
    if (drag.ok && drag.text) currentTree = drag.text;
    const ael = await timed("browser_act", {
      action_id: idFor("AelItem", 7),
      action: "click",
    });
    if (ael.ok && ael.text) currentTree = ael.text;
    const keyAct = await timed("browser_act", { action: "press_key", value: "Control+k" });
    if (keyAct.ok && keyAct.text) currentTree = keyAct.text;
    const ids = actionIds(currentTree);
    const scrl = await timed("browser_act", {
      action_id: ids[0] ?? 1,
      action: "scroll",
      value: "down",
      amount: 120,
    });
    if (scrl.ok && scrl.text) currentTree = scrl.text;

    await timed("browser_tabs", { action: "list" });
    await timed("browser_downloads", { action: "list" });

    await timed("browser_open", { url: `${base}/two` });
    await timed("browser_navigate", { action: "back" });
    await timed("browser_navigate", { action: "forward" });
    await timed("browser_navigate", { action: "back" });
    const reloaded = await timed("browser_navigate", {
      action: "reload",
      ignore_cache: false,
    });
    if (reloaded.ok && reloaded.text) currentTree = reloaded.text;

    await timed("browser_route", {
      action: "add",
      url_pattern: "*/api/echo",
      body: '{"mocked":true}',
    });
    await timed("browser_evaluate", {
      expression: "(() => { const x = new XMLHttpRequest(); x.open('GET', '/api/echo', false); x.send(); return x.responseText; })()",
    });
    await timed("browser_route", { action: "clear" });
    await timed("browser_console", { action: "list" });
    await timed("browser_console", { action: "clear" });
    await timed("browser_network", { action: "list" });
    await timed("browser_network", { action: "clear" });

    if (iteration === 1 || iteration % 5 === 0) {
      await timed("browser_storage_state", { action: "export", name: "soak-state" });
      const imp = await timed("browser_storage_state", { action: "import", name: "soak-state" });
      if (imp.ok && imp.text) currentTree = imp.text;
      const ctx = `soak-ctx-${iteration}`;
      await timed("browser_contexts", { action: "create", name: ctx });
      await timed("browser_open", { url: `${base}/` });
      await timed("browser_contexts", { action: "close", name: ctx });
    }
    if (iteration === 1 || iteration % args.screenshotEvery === 0) {
      await timed("browser_emulate", {
        viewport: { width: 390, height: 844, device_scale_factor: 2, mobile: true },
        timezone: "UTC",
      });
      await timed("browser_screenshot", { action_id: idFor("DblBtn", 3) });
      await timed("browser_pdf", { name: "soak-sample" });
      await timed("browser_emulate", { reset: true });
    }
    await timed("browser_extract_markdown", {});
  } else {
    const url = args.urls[currentUrlIndex % args.urls.length];
    currentUrlIndex++;

    const open = await timed("browser_open", { url });
    progress();

    const tree = open.text ?? "";
    const ids = actionIds(tree);

    // Exercise a rotating subset of tools against whatever the page offers.
    await timed("browser_act", {
      action_id: ids.length ? ids[0] : 1,
      action: "scroll",
      value: "down",
    });

    if (ids.length > 1) {
      await timed("browser_act", {
        action_id: ids[1],
        action: "hover",
      });
    }

    await timed("browser_extract_markdown", {});

    if (iteration % args.screenshotEvery === 0) {
      await timed("browser_screenshot", { full_page: false });
    }
  }

  progress();

  if (args.interval > 0) {
    await new Promise((r) => setTimeout(r, args.interval));
  }

  const now = Date.now();
  timeline.push({
    t: Math.round((now - startedAt) / 1000),
    iteration,
    samples: samples.length,
    chrome: chromeCount(),
    rssFb: rssKb(child.pid),
    rssChrome: chromeRssKb(),
  });
}

clearInterval(progressTimer);
console.log("soak window elapsed; shutting the MCP server down...");
child.stdin.end();
await new Promise((r) => {
  child.on("exit", r);
  setTimeout(() => {
    child.kill("SIGTERM");
    r();
  }, 5000);
});
if (v15Server) v15Server.server.close();
if (fileRoot) {
  try {
    rmSync(fileRoot, { recursive: true, force: true });
  } catch {}
}
if (storageRoot) {
  try {
    rmSync(storageRoot, { recursive: true, force: true });
  } catch {}
}

// ---------------------------------------------------------------- report

const firstTenth = summarize(samples.slice(0, Math.max(1, Math.floor(samples.length / 10))));
const lastTenth = summarize(samples.slice(-Math.max(1, Math.floor(samples.length / 10))));
const durationSec = (Date.now() - startedAt) / 1000;

const report = {
  startedAt: new Date(startedAt).toISOString(),
  durationSec: Math.round(durationSec),
  minutes: Number(args.minutes),
  urls: args.urls,
  iterations: iteration,
  toolCalls: samples.length,
  throughput_per_sec: Number((samples.length / durationSec).toFixed(3)),
  overall: summarize(samples),
  drift: {
    first_tenth_p50_ms: firstTenth.p50_ms,
    last_tenth_p50_ms: lastTenth.p50_ms,
    drift_ratio:
      firstTenth.p50_ms > 0
        ? Number((lastTenth.p50_ms / firstTenth.p50_ms).toFixed(3))
        : null,
    first_tenth_p95_ms: firstTenth.p95_ms,
    last_tenth_p95_ms: lastTenth.p95_ms,
  },
  perTool: Object.fromEntries(
    [...perTool.entries()].map(([k, v]) => [k, summarize(v)])
  ),
  memory: {
    rss_flashbrowse_start_kb: timeline[0]?.rssFb ?? 0,
    rss_flashbrowse_end_kb: timeline.at(-1)?.rssFb ?? 0,
    rss_chrome_start_kb: timeline[0]?.rssChrome ?? 0,
    rss_chrome_end_kb: timeline.at(-1)?.rssChrome ?? 0,
    chrome_processes_end: chromeCount(),
    leak_gate: null,
  },
  errors: errors.slice(0, 100),
  errorCount: errors.length,
  timeline,
};

const gate = evaluateGates({
  timeline,
  durationSec,
  errorCount: errors.length,
  toolCalls: samples.length,
  driftRatio: report.drift.drift_ratio,
});
report.memory.leak_gate = gate.memory;

writeFileSync(resolve(args.out), JSON.stringify(report, null, 2));

console.log("\n===== SOAK REPORT =====");
console.log(`duration          ${report.durationSec}s (${report.iterations} iterations)`);
console.log(`tool calls        ${report.toolCalls} (${report.throughput_per_sec}/s)`);
console.log(`error rate        ${report.errorCount} (${((report.errorCount / Math.max(1, report.toolCalls)) * 100).toFixed(2)}%)`);
console.log(`latency p50/p95   ${report.overall.p50_ms}ms / ${report.overall.p95_ms}ms  max ${report.overall.max_ms}ms`);
console.log(
  `latency drift     first-tenth p50 ${report.drift.first_tenth_p50_ms}ms -> last-tenth p50 ${report.drift.last_tenth_p50_ms}ms (${report.drift.drift_ratio}x)`
);
console.log(`tokens total      ${report.overall.total_tokens}`);
const mem = gate.memory;
const memGrowth = `${mem.growth_kb >= 0 ? "+" : ""}${mem.growth_kb}kB`;
const memSlope = `${mem.slope_kb_per_hour >= 0 ? "+" : ""}${mem.slope_kb_per_hour}kB/h`;
console.log(
  `memory fb         cold ${mem.cold_start_kb}kB | steady ${mem.baseline_kb}kB -> ${mem.end_kb}kB ` +
    `(${mem.ratio}x, ${memGrowth}; trend ${memSlope} r2 ${mem.trend_r2} after ${mem.warmup_sec}s warm-up)`
);
console.log(
  `memory chrome     ${report.memory.rss_chrome_start_kb}kB -> ${report.memory.rss_chrome_end_kb}kB`
);
console.log(`chrome procs      ${report.memory.chrome_processes_end} at exit`);
console.log(`report written    ${resolve(args.out)}`);

if (gate.errorFail) console.error("soak FAILED: error rate above 5%");
if (gate.driftFail) {
  console.error(
    `soak FAILED: latency drift ${report.drift.drift_ratio}x exceeds 1.2x`
  );
}
if (gate.memoryFail) {
  const why = mem.ratio_fail
    ? `${mem.ratio}x baseline`
    : `trend ${memSlope} (r2 ${mem.trend_r2}), projected ${mem.projected_kb_1h}kB after one hour`;
  console.error(
    `soak FAILED: flashbrowse RSS grew from ${mem.baseline_kb}kB to ${mem.end_kb}kB ` +
      `after the ${mem.warmup_sec}s warm-up: ${why}`
  );
}
process.exit(gate.errorFail || gate.driftFail || gate.memoryFail ? 1 : 0);
