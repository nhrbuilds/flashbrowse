---
name: flashbrowse-verify-and-soak
description: >-
  Run CI-parity formatting, clippy, workspace tests, Chrome provisioning checks,
  and the sequential MCP soak harness for flashbrowse. Use when verifying a
  change before completion, debugging test or soak regressions, or preparing a
  release.
---

# flashbrowse Verification & Soak Runbook

Run all checks sequentially on an idle machine when benchmarking or soaking.

## 1. Static & Unit/Integration Gates

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Note: `cdp_connection` and browser integration tests use system Chrome or managed Chrome (`~/.cache/flashbrowse/chrome`) and skip gracefully if absent.

## 2. Browser Provisioning & Environment Check

```sh
cargo run --release --bin flashbrowse -- setup
cargo run --release --bin flashbrowse -- doctor
```

Use `doctor --json` for machine-readable checks. `doctor` exits `1` if a blocking check fails (missing Chromium, unwritable cache dir, low temp space/memory).

## 3. Sequential Soak Test (`scripts/soak.mjs`)

1. Build the release binary:
   ```sh
   cargo build --release --bin flashbrowse
   ```
2. Serve the test fixtures locally on port `8099`:
   ```sh
   python3 -m http.server 8099 --directory crates/flashbrowse-core/tests/fixtures &
   SERVER_PID=$!
   ```
3. Run the smoke soak (5 min) or release soak (120 min):
   ```sh
   node scripts/soak.mjs \
     --minutes 5 \
     --url http://localhost:8099/ \
     --url https://example.com \
     --interval 200 \
     --bin target/release/flashbrowse \
     --out soak-report.json
   ```
4. Stop the fixture server (`kill "$SERVER_PID"`).

## 4. Validate Acceptance Thresholds

- `scripts/soak.mjs` exits non-zero if:
  - error rate > `5%`
  - latency drift (`last_tenth_p50_ms / first_tenth_p50_ms`) > `1.2x`
  - flashbrowse RSS growth > `1.5x`
- Inspect `soak-report.json` (`memory.chrome_processes_end == 0`) and run:
  ```sh
  cargo run --release --bin flashbrowse -- stats --json
  ```
