# Release checklist - flashbrowse

## Pre-flight
- [ ] `git status` clean, `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace` (incl. stress)
- [ ] `cargo build --release --workspace` to verify the optimized release
      profile and panic-unwind requirement.
- [ ] `node scripts/soak.mjs --minutes 30 --url <url> --out eval/results/soak-vX.Y.Z.json` (machine idle, run against the final release build and attach the report to the release notes).
- [ ] `node scripts/soak.mjs --minutes 5 ...` smoke (the same harness CI runs)
- [ ] Re-run the sequential benchmark on an idle machine and update the numbers in `README.md`
- [ ] Bump `workspace.package.version` in `Cargo.toml` and `CHANGELOG.md` with date + highlights
- [ ] One clean-VM smoke: `curl -fsSL .../install.sh | sh` → `flashbrowse setup` → `flashbrowse open https://example.com` → `flashbrowse mcp` ping
- [ ] `flashbrowse doctor` passes on the clean VM

## Tag + publish (GitHub Actions)
- [ ] `git tag vX.Y.Z && git push origin main --tags` ← `release.yml` builds 5 targets and publishes

## CI browser provisioning
`ci.yml` provisions the managed Chrome-for-Testing itself so the browser
tests run on **every** OS instead of skipping where Chrome is missing:

1. Resolve the exact Chrome-for-Testing stable version from Google's
   `last-known-good-versions.json` metadata.
2. Restore `~/.cache/flashbrowse/chrome` from `actions/cache` keyed by
   `chrome-<os>-<version>`.
3. Run `flashbrowse setup` - it is idempotent, so on a cache hit it prints
   "already installed" and exits in milliseconds.
4. Save the cache on a miss.

There is deliberately **no prefix fallback** on the cache key: serving an
older Chrome would silently change what the tests exercise. A new Chrome
release invalidates the key and the browser is fetched once more.

The cache holds ~120 MB per OS. GitHub's cache quota is 10 GB per repo, so
the browser is downloaded roughly once per Chrome release, not once per CI
run.

## Observability at release time
- [ ] `flashbrowse stats --json` shows a healthy error rate (< 1%)
- [ ] The soak report shows no latency drift (last-tenth p50 within ~1.2x of
      the first-tenth p50) and no orphan browsers at exit

## About signing (why it is optional, not required)

**OS-specific binaries are unavoidable** - a Rust binary is compiled per target
triple, exactly like `ripgrep`, `fd` or `bat`. We ship 5: linux x86_64/aarch64,
macOS arm64/x64, Windows x86_64. There is no universal binary for Linux or
Windows.

**Notarization is NOT required for our install path.** macOS Gatekeeper only
blocks files that carry the `com.apple.quarantine` attribute, and that
attribute is set by *browsers* (and some GUI downloaders) - not by `curl`, not
by `npm`, not by `cargo install`. So:

| how the user gets it | quarantine? | result |
|---|---|---|
| `curl ... \| sh` (our `install.sh`) | no | runs, no signing needed |
| `cargo install --git ... flashbrowse-cli` | no | compiles locally, no signing needed |
| `npm i -g flashbrowse` (not published yet) | no | would run, no signing needed |
| `brew install flashbrowse` (not published yet) | removed by Homebrew | would run |
| downloaded the `.tar.gz` in a browser and extracted it | **yes** | Gatekeeper blocks it → see below |

For the last case the installer already removes the attribute from the binary
it just installed (`xattr -dr com.apple.quarantine`), so even a
browser-downloaded tarball works after running `install.sh`. The manual
equivalent for users who copy the binary by hand:

```sh
xattr -dr com.apple.quarantine /usr/local/bin/flashbrowse
```

**Windows:** SmartScreen shows a warning for unsigned `.exe` files but
does not block them ("More info → Run anyway"). `install.ps1` calls
`Unblock-File`, which clears the "downloaded from the internet" flag, so the
warning does not appear for installer users. Code signing with an EV
certificate is optional polish (and costs ~$200/yr).

**Optional polish (when there is budget):**
- [ ] Apple Developer cert as a `MACOS_CERT_P12` secret → `codesign --sign
      "Developer ID Application"` → `xcrun notarytool submit --wait` →
      `xcrun stapler staple` in `release.yml` before packaging. Only needed
      if you want a signed, notarized macOS binary (e.g. for a `.pkg`/`.dmg`).
- [ ] Windows code signing (`signtool`) with an EV cert.
- [ ] Publish to npm (`npm i -g flashbrowse`) and Homebrew for the smoothest
      upgrade path - neither needs notarization.

## Limitations (ship honest)
- Stealth: defeats naive bot checks only. Not undetectable against Canvas/Audio/fingerprinting.
- MCP `browser_open` is URL-allowlisted (`http/https/data/about`) by design.
- `select` matches visible text or value; virtualized/complex widgets may need `press_key` fallback.
- Markdown extraction is lightweight (not full Readability) - tables/iframes partial.
- The MCP server is deliberately **serial** (one tool call at a time). It is
  not built for concurrent agents sharing one server; give each agent its
  own `flashbrowse mcp` process.
- Real-network navigation can fail (proxy, captive portal, bot wall). These
  surface as explicit errors, never as a silently truncated tree.
