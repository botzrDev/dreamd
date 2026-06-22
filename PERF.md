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
| P50 warm at 10k | < 5 ms  | TBD      | `cargo bench -p dreamd-core` |
| P99 cold at 10k | < 50 ms | TBD      | `cargo bench -p dreamd-core` |

---

_Last measured:_ 2026-06-06, dreamd 0.1.0-rc.1 (6e7504e, x86_64-unknown-linux-gnu).
Local measured idle VmRSS jitters ~11.2–11.4 MB across runs (recorded: 11.37 MB).
