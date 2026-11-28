# Contributing to flashbrowse

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
