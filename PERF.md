# dreamd — Performance Baseline

CI enforces the limits below. This file records the last measured values.
**CI does not auto-commit this file** — update manually after each significant build change.

## Idle daemon RSS (NFR-1)

| Limit                     | Measured                         | Gate                                 |
| ------------------------- | -------------------------------- | ------------------------------------ |
| < 30 MB                   | **11.37** MB                     | `idle-rss-gate` (CI, Linux)          |
| (informational, no limit) | pending first `macos-latest` run | `idle-rss-report-macos` (CI, macOS)  |

_Methodology:_ `dreamd watch` (release build) in a temp workspace with an empty
`.agent/`. Readiness = `~/.agent/dreamd.sock` present. Settle = 2 s. Linux
metric = `VmRSS` from `/proc/<daemon_pid>/status`, gated at 30 MB. macOS metric
= `ps -o rss=` (KiB) of the `dreamd watch` child after the same socket + settle
path. It is **not** `phys_footprint` (Activity Monitor "Memory" uses a different
accounting that excludes clean file-backed pages such as shared dylib text), and
it is **not** compared to the Linux 30 MB gate. The macOS measured cell stays
`pending first macos-latest run` until read off the `idle-rss-report-macos`
step summary — CI does not auto-commit this file. A macOS threshold (observed +
20% headroom) is a later founder decision (AILAB-176).

## Stripped binary size (NFR-2)

| Limit   | Measured | Gate                    |
| ------- | -------- | ----------------------- |
| < 20 MB | see CI   | `size-gate` (CI, Linux) |

## Recall latency

| Metric          | Target  | Measured | Gate                         |
| --------------- | ------- | -------- | ---------------------------- |
| P50 warm at 10k | < 5 ms  | ~0.30 ms              | `cargo bench -p dreamd-core` |
| P99 warm at 10k | < 50 ms | **~0.34** ms          | `cargo bench -p dreamd-core` |

---

_Last measured:_ 2026-07-28, dreamd 0.1.0-rc.7, commit e8e27fd (x86_64-unknown-linux-gnu, WSL2). Numbers have not been re-run against the current `0.1.0` binary; treat them as a methodology stamp, not a live scoreboard.
Recall rows: warm in-RAM index, Criterion 0.5, 100 samples at n=10k (`benches/recall.rs`).
P50 ≈ median per-iteration sample; P99 = 99th percentile of the same samples.
Local measured idle VmRSS jitters ~11.2–11.4 MB across runs (recorded: 11.37 MB).
