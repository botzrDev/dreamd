# Pre-1.0.0 testing audits — 2026-10-01

Audits of local commit `bb84721` (`chore: release version 1.0.0`) before any tag or push. Toolchain: rustc 1.95.0, cargo 1.95.0, Linux x86_64. Default features. The working tree also had an uncommitted `AGENTS.md` edit; that file is not compiled into the binary. `dreamd version` on the debug binary printed commit `bb84721`, schema `1.0`, `vectors: off`.

| Audit | Verdict |
| --- | --- |
| [Workspace Rust suite](01-workspace-suite.md) | Red. 1,072 tests executed, 1 failed. |
| [CI and release gates](02-ci-and-release-gates.md) | The tag workflow does not run the suite. Pushing `main` would fail CI on the same assertion, and the npm shim fails on `PENDING_LINUX`. |
| [Memory invariants](03-memory-invariants.md) | The claims in the 1.0.0 changelog that this tree can execute on Linux held under the tests that ran. |
| [End-to-end](04-end-to-end.md) | Green. Alpha 7/7, quality 14/14, install-funnel 30/30. Shim 37/38, and the one failure is the placeholder sha. |

**Hold the tag** until `crates/dreamd-cli/tests/version_output.rs` accepts the version line that has shipped since `f786bff` (`schema:1.0 vectors:off` or `vectors:on`). The binary already prints `dreamd 1.0.0`. The assertion still requires the closing paren to sit immediately after `schema:1.0`.

A second run on the same commit, without reading these files first, reproduced the counts and both failures. Corrections from that comparison: `dream_cycle.rs` has 26 tests, not 9, and `rrf::fuse` is not a string that appears in `crates/`. Timings differ between machines. The second run kept a real `target/debug/dreamd` from `cargo build -p dreamd --bin dreamd`.
