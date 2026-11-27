#!/usr/bin/env node
// Real-site fidelity check.
//
// For every URL, fetch the raw AX snapshot and rendered tree from one accepted
// open, then apply the same rules as `crates/flashbrowse-core/tests/fidelity.rs`.
// Comparing separate navigations can report changing timestamps as lost text.
//
// Sequential by design: one page at a time, one browser per dump.
//
//   node scripts/fidelity.mjs --url https://example.com --url https://github.com
//   node scripts/fidelity.mjs --url http://127.0.0.1:8099/forms.html --binary ./target/debug/flashbrowse
//
// Exits 1 when any page reports missing text, so it can gate a release
// manually without pretending to be a CI test.

import { execFile } from "node:child_process";
import { createHash } from "node:crypto";
import { writeFile } from "node:fs/promises";
import { promisify } from "node:util";

const run = promisify(execFile);

const MAX_NAME_CHARS = 120;

const NAME_BEARING = new Set([
  "statictext",
  "heading",
  "button",
  "link",
  "menuitem",
  "menuitemcheckbox",
  "menuitemradio",
  "tab",
  "checkbox",
  "radio",
  "switch",
  "cell",
  "columnheader",
  "rowheader",
  "listitem",
  "paragraph",
  "textbox",
  "searchbox",
  "combobox",
  "spinbutton",
  "slider",
  "option",
]);

function usage() {
  console.error(
    "usage: node scripts/fidelity.mjs --url <url> [--url <url>] " +
      "[--out fidelity-report.json] [--binary ./target/release/flashbrowse] " +
      "[--timeout 120000]",
  );
}

function parseArgs(argv) {
  const opts = {
    urls: [],
    out: "fidelity-report.json",
    binary: "./target/release/flashbrowse",
    timeout: 120_000,
  };
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    const next = () => {
      const value = argv[++i];
      if (value === undefined) {
        usage();
        process.exit(2);
      }
      return value;
    };
    if (arg === "--url") opts.urls.push(next());
    else if (arg === "--out") opts.out = next();
    else if (arg === "--binary") opts.binary = next();
    else if (arg === "--timeout") opts.timeout = Number(next());
    else if (arg === "--help" || arg === "-h") {
      usage();
      process.exit(0);
    } else {
      console.error(`unknown argument: ${arg}`);
      usage();
      process.exit(2);
    }
  }
  return opts;
}

function normalize(text) {
  return String(text).split(/\s+/).filter(Boolean).join(" ");
}

function truncate(text) {
  const chars = [...text];
  if (chars.length <= MAX_NAME_CHARS) return text;
  return chars.slice(0, MAX_NAME_CHARS - 1).join("") + "…";
}

function isPunctuationOnly(text) {
  return (
    /^[\s!-/:-@[-`{-~]+$/.test(text) && /[!-/:-@[-`{-~]/.test(text)
  );
}

function roleOf(node) {
  return String(node?.role?.value ?? "").trim().toLowerCase();
}

function nameOf(node) {
  return String(node?.name?.value ?? "").trim();
}

function valueOf(node) {
  return String(node?.value?.value ?? "").trim();
}

function analyze(rawNodes, tree) {
  const index = new Map(rawNodes.map((n) => [n.nodeId, n]));
  const parent = new Map();
  const children = new Map();
  for (const node of rawNodes) {
    for (const child of node.childIds ?? []) {
      parent.set(child, node.nodeId);
      if (!children.has(node.nodeId)) children.set(node.nodeId, []);
      children.get(node.nodeId).push(child);
    }
  }

  const hasMeaningfulChild = (id) =>
    (children.get(id) ?? []).some((childId) => {
      const role = roleOf(index.get(childId));
      return role !== "inlinetextbox" && role !== "none";
    });

  const hasNamedAncestor = (id, depth = 3) => {
    let current = id;
    for (let i = 0; i < depth; i++) {
      const parentId = parent.get(current);
      if (!parentId) return false;
      const ancestor = index.get(parentId);
      if (!ancestor) return false;
      if (NAME_BEARING.has(roleOf(ancestor)) && nameOf(ancestor)) return true;
      current = parentId;
    }
    return false;
  };

  const hasValueAncestor = (id, text, depth = 3) => {
    if (!text) return false;
    let current = id;
    for (let i = 0; i < depth; i++) {
      const parentId = parent.get(current);
      if (!parentId) return false;
      const ancestor = index.get(parentId);
      if (!ancestor) return false;
      if (valueOf(ancestor) === text) return true;
      current = parentId;
    }
    return false;
  };

  const expected = [];
  for (const node of rawNodes) {
    if (node.ignored) continue;
    if (roleOf(node) !== "statictext") continue;
    const text = normalize(nameOf(node));
    if (!text || isPunctuationOnly(text)) continue;
    if (hasMeaningfulChild(node.nodeId)) continue;
    if (hasNamedAncestor(node.nodeId)) continue;
    if (hasValueAncestor(node.nodeId, text)) continue;
    expected.push({ id: node.nodeId, text: truncate(text) });
  }

  const missing = expected.filter((item) => !tree.includes(item.text));

  return {
    staticTexts: expected.length,
    missing: missing.map((item) => item.text),
    rawBytes: Buffer.byteLength(JSON.stringify(rawNodes)),
    treeBytes: Buffer.byteLength(tree),
    treeSha256: createHash("sha256").update(tree).digest("hex"),
  };
}

async function dumpSnapshot(binary, url, timeout) {
  const { stdout } = await run(binary, ["dump", url, "--format", "fidelity"], {
    timeout,
    maxBuffer: 128 * 1024 * 1024,
  });
  return JSON.parse(stdout);
}

function formatBytes(n) {
  if (n >= 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MB`;
  return `${(n / 1024).toFixed(1)} KB`;
}

async function main() {
  const opts = parseArgs(process.argv.slice(2));
  if (opts.urls.length === 0) {
    usage();
    process.exit(2);
  }

  const pages = [];
  for (const url of opts.urls) {
    const started = Date.now();
    try {
      const { raw_nodes: raw, tree } = await dumpSnapshot(opts.binary, url, opts.timeout);
      const body = tree.split("\n---\n")[1] ?? "";
      if (body.trim().length === 0) {
        // A header-only tree means the navigation produced nothing usable.
        // Reporting it as a 100% reduction would be a lie.
        throw new Error("empty compressed tree (navigation likely failed)");
      }
      const result = analyze(raw, tree);
      pages.push({
        url,
        ok: true,
        ms: Date.now() - started,
        ...result,
        reduction: result.rawBytes > 0 ? 1 - result.treeBytes / result.rawBytes : 0,
      });
    } catch (error) {
      pages.push({
        url,
        ok: false,
        ms: Date.now() - started,
        error: String(error?.message ?? error).split("\n")[0],
      });
    }
  }

  const report = {
    generatedAt: new Date().toISOString(),
    binary: opts.binary,
    pages,
    summary: {
      pages: pages.length,
      failed: pages.filter((p) => !p.ok).length,
      missing: pages.reduce((sum, p) => sum + (p.missing?.length ?? 0), 0),
    },
  };

  await writeFile(opts.out, JSON.stringify(report, null, 2) + "\n");

  console.log(
    [
      "page".padEnd(46),
      "static".padStart(7),
      "missing".padStart(8),
      "raw".padStart(10),
      "tree".padStart(10),
      "reduction".padStart(10),
    ].join(" "),
  );
  for (const page of pages) {
    if (!page.ok) {
      console.log(`${page.url.padEnd(46)} FAILED: ${page.error}`);
      continue;
    }
    console.log(
      [
        page.url.padEnd(46),
        String(page.staticTexts).padStart(7),
        String(page.missing.length).padStart(8),
        formatBytes(page.rawBytes).padStart(10),
        formatBytes(page.treeBytes).padStart(10),
        `${(page.reduction * 100).toFixed(1)}%`.padStart(10),
      ].join(" "),
    );
    for (const text of page.missing.slice(0, 5)) {
      console.log(`    missing: ${JSON.stringify(text.slice(0, 100))}`);
    }
  }
  console.log(`\nreport: ${opts.out}`);

  if (report.summary.failed > 0 || report.summary.missing > 0) {
    process.exitCode = 1;
  }
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
