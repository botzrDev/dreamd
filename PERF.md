# dreamd — Performance Baseline

CI enforces the limits below. This file records the last measured values.
**CI does not auto-commit this file** — update manually after each significant build change.

## Idle daemon RSS (NFR-1)

| Limit   | Measured   | Gate                        |
| ------- | ---------- | --------------------------- |
| < 30 MB | **11.37** MB | `idle-rss-gate` (CI, Linux) |

_Methodology:_ `dreamd watch` (release build) in a temp workspace with an empty
`.agent/`. Readiness = `~/.agent/dreamd.sock` present. Settle = 2 s. Metric =
`VmRSS` from `/proc/<daemon_pid>/status`. Linux only; macOS deferred (separate
ticket — phys_footprint accounting is not comparable to VmRSS).

## Stripped binary size (NFR-2)

| Limit   | Measured | Gate                    |
| ------- | -------- | ----------------------- |
| < 15 MB | see CI   | `size-gate` (CI, Linux) |

## Recall latency

| Metric          | Target  | Measured | Gate                         |
| --------------- | ------- | -------- | ---------------------------- |
| P50 warm at 10k | < 5 ms  | ~0.32 ms              | `cargo bench -p dreamd-core` |
| P99 warm at 10k | < 50 ms | **~0.46** ms          | `cargo bench -p dreamd-core` |

---

_Last measured:_ 2026-06-24, dreamd 0.1.0-rc.2 (x86_64-unknown-linux-gnu, WSL2).
Recall rows: warm in-RAM index, Criterion 0.5, 100 samples at n=10k (`benches/recall.rs`).
P50 ≈ median per-iteration sample; P99 = 99th percentile of the same samples.
Local measured idle VmRSS jitters ~11.2–11.4 MB across runs (recorded: 11.37 MB).
