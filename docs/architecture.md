# Architecture

flashbrowse is a Rust workspace split by responsibility. Each crate owns a
clear boundary, and larger components use submodules for related operations.
The layout follows Rust module conventions rather than forcing a web framework
pattern such as MVC onto a command-line and protocol client.

## Workspace crates

| Crate | Owns |
|---|---|
| `flashbrowse-cdp` | Chrome discovery and launch, CDP protocol wrappers, event delivery, and WebSocket or pipe transport |
| `flashbrowse-bidi` | Firefox discovery and launch groundwork. It does not provide production browser sessions. |
| `flashbrowse-core` | Browser session lifecycle, navigation, user actions, tree conversion, resource policy, stealth, and operation metrics |
| `flashbrowse-mcp` | MCP JSON-RPC framing, request handling, recovery boundaries, tool schemas, and tool handlers |
| `flashbrowse-cli` | Command-line parsing, command entry points, session-file persistence, and user-facing output |

Dependencies flow from the CLI and MCP crates into core, and from core into
CDP. CDP does not depend on core or MCP. This keeps protocol mechanics
independent of browser-agent policy.

## Main module boundaries

### CDP client and browser launch

- `flashbrowse-cdp/src/transport.rs` owns request IDs, frame limits, connection
  setup, send operations, and the transport task. Its tests live in
  `transport/tests.rs`.
- `flashbrowse-cdp/src/launcher.rs` keeps launch options and exports the
  existing launch functions. `launcher/discovery.rs` locates Chrome,
  `launcher/profile.rs` handles profile setup and cleanup, `launcher/process.rs`
  owns the child-process guard, and `launcher/startup.rs` starts the browser and
  waits for its debugging endpoint.
- `flashbrowse-cdp/src/protocol/` groups typed CDP calls by protocol domain,
  including page, target, network, input, and accessibility operations.

### Browser behavior

`flashbrowse-core/src/session.rs` owns the shared `BrowserSession` type,
configuration, errors, and state used by its implementation modules. The
inherent methods are grouped by purpose:

| Module | Responsibility |
|---|---|
| `session/lifecycle.rs` | Launch, attach, detach, shutdown, and session recovery |
| `session/navigation.rs` | Open, history navigation, waiting, and settle decisions |
| `session/actions.rs` | Accessibility tree refresh and click, fill, select, key, and scroll actions |
| `session/tabs.rs` | Tabs, popups, isolated contexts, and live resource policy changes |
| `session/storage.rs` | Cookies, local storage, uploads, and download state |
| `session/capture.rs` | Markdown, screenshots, emulation, permissions, and PDF output |
| `session/observability.rs` | Console, network, headers, and route operations |
| `session/settling.rs` | Shared network activity tracking and settle helpers |

The public types remain exported from `flashbrowse_core::session` and the
crate root. The module split is intended to keep that API stable while making
implementation changes easier to locate.

Tree types and rendering are in `tree/node.rs` and `tree/formatter.rs`.
`tree/compressor.rs` coordinates AX tree conversion. The full-tree and pruned
algorithms live in `tree/compressor/full.rs` and
`tree/compressor/pruned.rs`.

Compression has two important correctness constraints: retain every visible
text value at least once, and assign output in document order rather than
Chromium's breadth-first AX array order. Do not trade either guarantee for
smaller output or faster settling.

### MCP server and tools

- `flashbrowse-mcp/src/server.rs` exposes the server entry points.
  `server/framing.rs` reads bounded JSON-RPC lines and writes responses.
  `server/request.rs` handles protocol dispatch, while `server/recovery.rs`
  contains panic, timeout, cancellation, and session-recovery boundaries.
  `server/stdio.rs` owns process stdin and stdout integration.
- `flashbrowse-mcp/src/tools.rs` declares tool schemas, validates arguments,
  selects a browser session, and routes tool calls.
  `tools/session.rs` owns session selection and cleanup.
  `tools/actions.rs`, `tools/navigation.rs`, and `tools/files.rs` hold related
  tool handlers.

Tool output and errors are returned through the MCP response. Logs go to
stderr. Keep stdout reserved for JSON-RPC frames.

### CLI

`flashbrowse-cli/src/main.rs` defines the CLI entry point and shared parsing.
Command implementations live in `flashbrowse-cli/src/commands/`, with one
module per command or closely related command support. Keep command-specific
behavior there rather than adding a large branch to `main.rs`.

## Change entry points

- **Add or change a CLI command:** update the argument definition in
  `flashbrowse-cli/src/main.rs`, implement it in `src/commands/`, and update
  `docs/usage.md`.
- **Add or change an MCP tool:** update the schema and dispatch in
  `flashbrowse-mcp/src/tools.rs`, place the handler in the matching `tools/`
  module, and add protocol or end-to-end coverage under
  `flashbrowse-mcp/tests/`.
- **Add a CDP method:** place it in the protocol-domain file under
  `flashbrowse-cdp/src/protocol/`. Keep connection and framing details in the
  transport module.
- **Change browser behavior:** locate the relevant `session/` module and keep
  `BrowserSession` public behavior routed through the existing methods.
- **Change compression or rendering:** work under `flashbrowse-core/src/tree/`
  and preserve visible text, document order, action ID behavior, and settle
  requirements.

Tests follow the same boundaries. Unit tests are kept beside the component or
in a sibling `tests.rs` module. Larger MCP scenarios are grouped by behavior in
`flashbrowse-mcp/tests/mcp_stdio_e2e/`. Shared harness helpers remain in
`mcp_stdio_e2e.rs`.

## Review and verification

Run formatting, Clippy, and the workspace suite after code changes:

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Browser integration tests use Chromium when it is available. Contribution and
release checks, including the sequential soak and machine details to report,
are listed in [Contributing](../CONTRIBUTING.md).
