# Audit 1 — Workspace Rust suite

**Verdict: red.** `cargo test --workspace` on `bb84721` exited 101. One assertion failed. The two crates that Cargo never started, because the workspace stops at the first failing package, were then run on their own and passed.

Commit: `bb84721`. Date: 2026-10-01. Profile: debug, default features. Threads: 8. Host: Linux x86_64, rustc 1.95.0 (the CI pin).

## Commands

```bash
cargo test --workspace -- --test-threads=8
# exit 101 after the dreamd (CLI) package

cargo test -p dreamd-core -p dreamd-protocol -- --test-threads=8
# exit 0
```

Cargo's default is fail-fast across packages. The CLI package is `dreamd`, so it ran first, failed, and the workspace command never started `dreamd-core` or `dreamd-protocol` as test targets. Those crates were only compiled as dependencies of the CLI. The second command is what actually executed their tests.

## Results

| Target | Passed | Failed | Notes |
| --- | ---: | ---: | --- |
| `dreamd` lib (`crates/dreamd-cli`) | 332 | 0 | Includes `version_reports_vectors` and `enable_without_feature_exits_2_and_names_the_feature` |
| `dreamd` bins `main`, `generate_man` | 0 | 0 | Thin shims; expected empty |
| `dreamd` integration tests except `version_output` | 131 | 0 | See the file list below |
| `version_output` | 2 | 1 | `short_version_format_and_no_sentinel` |
| `dreamd` doc-tests | — | — | Not reached |
| `dreamd-core` lib | 557 | 0 | 11.33s |
| `dreamd-core` integration | 38 | 0 | Nine files; slowest is `weg12` at 11.03s |
| `dreamd-core` doc-tests | 0 | 0 | No doctests in the crate |
| `dreamd-protocol` lib | 10 | 0 | |
| `dreamd-protocol` `roundtrip_proptest` | 1 | 0 | 512 cases |
| `dreamd-protocol` doc-tests | 0 | 0 | |
| **Executed** | **1071** | **1** | |

CLI integration files that passed: `cli_dispatch` (12), `cli_help` (40), `cross_home_stop` (1), `doctor_repair` (4), `dream_doctor_discover` (4), `init_golden` (5), `mcp_coexistence` (1), `mcp_stdin_eof` (2), `reset_workspace` (2), `setup_cmd` (4), `setup_mcp_merge` (21), `setup_wizard` (9), `uninstall` (6), `uninstall_cmd` (10), `update_cmd` (10).

Core integration files that passed: `bm25_fastfield_integration` (3), `dream_cycle_snapshot` (3), `episodic_scan_proptest` (10), `salience_proptest` (5), `semantic_indexing` (12), `weg12_concurrency_torture` (2), `weg21_writer_process` (1), `weg268_sigterm_socket` (1), `weg60_wal_recovery_startup` (1).

Wall clock was about 90s for the workspace command (mostly compiling the CLI and its deps) and about 60s for the core and protocol command.

## The failure

`crates/dreamd-cli/tests/version_output.rs` lines 41–44 require the substring `schema:1.0)`. The panic is reported at line 41:

```41:44:crates/dreamd-cli/tests/version_output.rs
    assert!(
        stdout.contains(" schema:1.0)"),
        "missing schema:1.0): {stdout}"
    );
```

The binary printed:

```text
dreamd 1.0.0 (bb84721 build:2026-10-01 target:x86_64-unknown-linux-gnu schema:1.0 vectors:off)
```

`schema:1.0` is present. The next token is `vectors:off`, then the closing paren. The same assertion fails for a `--features vectors` build, which prints `vectors:on` in that slot. `short_version_v_flag_matches_long_flag` and `long_version_subcommand_format_and_no_sentinel` passed. The long form still has a line `  schema:  1.0` followed by `  vectors: off`, and the test looks for `schema:  1.0\n`, which is still true.

This assertion was last touched in `a2354bc` (2026-05-11), when the short line ended at `schema:1.0)`. `f786bff` (2026-09-30, `dreamd vectors enable`) inserted ` vectors:` and `on`/`off` into `VERSION_SHORT` in `commands/version.rs`. The help snapshot was updated with it. The integration test was not.

`crates/dreamd-cli/tests/snapshots/cli_help__version_short.snap` already locks the live shape:

```text
dreamd 1.0.0 ([sha] build:[date] target:[target] schema:1.0 vectors:[state])
```

`cli_help` (40 tests) passed against that snapshot. The unit test `version_reports_vectors` also passed inside the 332. The red test is a stale shell-out check, and the process under test is the 1.0.0 binary.

`crates/dreamd-cli/tests/README.md` still tells a reviewer to treat `dreamd 0.1.0-rc.2` as a locked literal. The snapshots no longer say that. The README is stale commentary; it is not what failed.

## What this pass did not run

- `cargo test --all-features --workspace`. That is the CI test command. It would compile the `vectors` feature (and fetch ONNX Runtime at build time) and it would still fail `version_output` for the reason above, so the rest of an all-features workspace run would not start under fail-fast.
- `cargo test -p dreamd --doc`, because the package failed before doc-tests.
- `cargo clippy`, `cargo fmt --check`, release-profile tests, coverage, and the nightly 1024-case proptest override.
- macOS and Windows.

Property tests in this pass used the in-file config of 512 cases (`salience_proptest`, `episodic_scan_proptest`, `roundtrip_proptest`). Nightly sets `PROPTEST_CASES=1024` on top of that. This pass did not.
