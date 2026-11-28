<p align="center">
  <img src="https://raw.githubusercontent.com/nhrbuilds/flashbrowse/main/assets/icon.png" alt="flashbrowse icon" width="128" height="128">
</p>

# flashbrowse

A single-binary browser controller and token-compression engine for LLM agents,
written in Rust.

- Uses Chrome through the Chrome DevTools Protocol, with POSIX pipe transport
  where supported and WebSocket transport for attachment and persistent
  sessions.
- Compresses the accessibility tree into an action-oriented text view while
  preserving visible content and document order.
- Provides a native MCP stdio server for Claude Code, Cursor, Codex, and other
  MCP clients.
- Runs without Python, Node, or JVM dependencies. Chrome is installed once by
  `flashbrowse setup` or can be provided by the user.

Stealth helps with basic bot checks, but does not hide canvas, audio, or TLS
fingerprints. `select` matches an option's text or value. Markdown extraction
is lightweight and does not replace a full article parser.

Firefox support is not production-ready. The `flashbrowse-bidi` crate contains
Firefox discovery and launch groundwork; active browser sessions still use
Chrome. See the [roadmap](https://github.com/nhrbuilds/flashbrowse/blob/main/README.md#roadmap-for-160)
and [architecture guide](https://github.com/nhrbuilds/flashbrowse/blob/main/docs/architecture.md).

## Install

macOS and Linux:

```sh
curl -fsSL https://raw.githubusercontent.com/nhrbuilds/flashbrowse/main/install.sh | sh
```

Windows PowerShell:

```powershell
irm https://raw.githubusercontent.com/nhrbuilds/flashbrowse/main/install.ps1 | iex
```

If Chrome is already installed, `flashbrowse setup` is optional. Otherwise,
install managed Chrome once, then open a page or start the MCP server:

```sh
flashbrowse setup
flashbrowse open https://example.com
flashbrowse mcp
```

Or install from this repository with Cargo:

```sh
cargo install --locked --git https://github.com/nhrbuilds/flashbrowse flashbrowse-cli
```

flashbrowse is not published on crates.io, npm, or Homebrew.

See the [usage guide](https://github.com/nhrbuilds/flashbrowse/blob/main/docs/usage.md)
for CLI commands, browser profiles, contexts, files, dialogs, and MCP setup.

## Build from source

```sh
git clone https://github.com/nhrbuilds/flashbrowse
cd flashbrowse
cargo build --release
./target/release/flashbrowse setup
./target/release/flashbrowse open https://example.com
```

Chrome discovery checks `--chrome-path`, `CHROME_PATH`, and `CHROME_BINARY`
before system and managed installations. Set `CHROME_PATH=/path/to/chrome`
when Chrome is installed outside the usual locations.

The release configuration prioritizes binary size with `opt-level = "z"`, LTO,
one codegen unit, and symbol stripping. The ZIP Zopfli encoder and tar
extended-attribute support are disabled. ZIP DEFLATE reading and update archive
handling remain available. Panic unwinding is retained for recovery paths. The
repository does not contain a matched v1.4.0 and v1.5.0 executable-size
measurement, so it does not claim a measured release-to-release size reduction.
See [release notes](https://github.com/nhrbuilds/flashbrowse/blob/main/CHANGELOG.md)
and [release checks](https://github.com/nhrbuilds/flashbrowse/blob/main/RELEASE.md).

## What changed in v1.5.0

- MCP tool panics and tool deadlines produce tool errors. The server attempts
  bounded cleanup and retains usable browser contexts where it can. The CLI
  reports an unexpected unwind and exits with a failure status.
- Opening a page waits for a stable, non-empty accessibility tree. A settle
  timeout returns an error and invalidates stale action IDs instead of
  returning a partial snapshot.
- Browser contexts keep cookies and storage isolated. If the active tab closes,
  flashbrowse can attach to another page in that same context or create a blank
  page there.
- Release builds use a size-oriented profile while keeping panic unwinding.

The [usage guide](https://github.com/nhrbuilds/flashbrowse/blob/main/docs/usage.md)
describes the supported browser and MCP features. The [benchmark report](https://github.com/nhrbuilds/flashbrowse/blob/main/docs/benchmarks.md) separates historical
measurements, release results, and development soaks.

## Project structure

The workspace uses Rust modules organized around responsibilities rather than a
single large session or tool file:

| Crate | Responsibility |
|---|---|
| `flashbrowse-cdp` | Chrome launch, CDP protocol calls, event handling, and transport |
| `flashbrowse-bidi` | Firefox discovery and launch groundwork |
| `flashbrowse-core` | Browser session, actions, tree compression, policies, and observability |
| `flashbrowse-mcp` | MCP framing, request recovery, and tool handlers |
| `flashbrowse-cli` | CLI commands and persistent session files |

See the [architecture guide](https://github.com/nhrbuilds/flashbrowse/blob/main/docs/architecture.md)
for module responsibilities and change entry points.

## Roadmap for 1.6.0

This is a working plan for contributors, not a release promise.

### Maintainer focus

- **Faithful output.** Fix PDF export so it renders the page instead of a
  degraded one: lift the blocked fonts and images during export, wait for fonts
  (`document.fonts`) and images to be ready, and expose paper size, margins, and
  `prefer_css_page_size`. Apply the same font-readiness wait to screenshots,
  which today checks network and images but not fonts.
- **Shared render preparation.** One `prepare_for_faithful_render` path used by
  both `browser_screenshot` and `browser_pdf`, with a bounded timeout so a slow
  page cannot pin the serial MCP server.
- **Land the module split.** The `session/`, `transport/`, `tools/`, `server/`,
  `compressor/`, `launcher/`, and `observability_session/` modules are in
  progress. Finish, test, and merge them before this release.
- **Recovery and performance gates:** extend fault-injection and context
  cleanup coverage. Keep sequential p95 comparisons reproducible before
  changing shared session or transport behavior.

### Good contributor areas

- Add small HTML fixtures and focused regression coverage for dialogs, popups,
  file transfer, storage isolation, shadow roots, and form interactions.
- Improve user documentation, runnable examples, and MCP setup recipes. Check
  commands and schemas against the current CLI and MCP tool definitions.
- Improve cross-platform CI reliability and test-server portability.
- Improve benchmark and soak report readability without changing settle
  conditions or tree-compression behavior.

The maintainer is reserving the Firefox BiDi driver, CDP transport, core
`BrowserSession` recovery, settle loop, compressor semantics, and p95 gates.
Please open an issue before making broad changes in those areas. See
[contribution requirements](https://github.com/nhrbuilds/flashbrowse/blob/main/CONTRIBUTING.md)
for test and report expectations.

## Roadmap for 1.7.0

Working plan, not a release promise.

- **Firefox through BiDi:** continue from discovery and launch groundwork
  toward browser sessions and accessibility-tree support. Chrome remains the
  production backend until tree and action behavior has passing coverage.
- **Candidates to pull forward (higher user value than Firefox):** set-of-marks
  screenshots (`browser_screenshot --marks`) so vision models can act, and
  `select` support for framework comboboxes that are not native `<select>`.
  These close documented limitations and are smaller than the BiDi driver.

## Benchmark summary

The repository records historical sequential comparisons and development soak
results. The measurements use different runs and are not a matched v1.4.0 to
v1.5.0 comparison. Do not interpret them as proof that every workload improved.

| Measurement | Recorded result |
|---|---:|
| Earlier warm agent loop, flashbrowse p50 | 524 ms |
| v1.4.0 cold open p95 change | 1,285.7 ms to 908.0 ms, -29.4% |
| v1.4.0 warm open p95 change | 522.8 ms to 435.2 ms, -16.8% |
| v1.4.0 warm action p95 change | 445.2 ms to 63.3 ms, -85.8% |
| v1.5.0 development 45-minute soak | 7,522 calls, 0 errors, leak gate passed |

Details, methods, limitations, and soak records are in the
[benchmark report](https://github.com/nhrbuilds/flashbrowse/blob/main/docs/benchmarks.md).
Accuracy and task-evaluation limits are in
[EVAL.md](https://github.com/nhrbuilds/flashbrowse/blob/main/EVAL.md).

## Contributing

Pull requests must include the requested test results, a 30-minute sequential
soak for code changes, machine details, and a comparison with the latest
release. Read [CONTRIBUTING.md](https://github.com/nhrbuilds/flashbrowse/blob/main/CONTRIBUTING.md)
before opening a pull request.

## License

Apache-2.0. See [LICENSE](https://github.com/nhrbuilds/flashbrowse/blob/main/LICENSE).
