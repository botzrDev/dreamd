# `dreamd-cli` integration tests

## Test files

- `init_golden.rs` — byte-exact stdout fixtures for `dreamd init` (first run, re-run, `--quiet` first run and re-run, no-project-root).
- `setup_cmd.rs` — subprocess coverage for `dreamd setup` (AILAB-549): non-interactive scaffold, rerun idempotency, `--dry-run` writes nothing, missing project root exits 2.
- `setup_mcp_merge.rs` — `dreamd setup` MCP config merge writer (AILAB-550), driven through the real binary.
- `setup_wizard.rs` — `dreamd setup` TTY gate and success card (AILAB-551).
- `version_output.rs` — subprocess regression checks for `--version` / `version` (vergen sentinel leak guard).
- `cli_dispatch.rs` — subprocess tests for dispatch paths that exit quickly: usage errors, feature guards, `mcp` / `watch` error-to-exit-code mapping.
- `dream_doctor_discover.rs` — `dreamd dream` and `dreamd doctor` find the store by walking up from a subdirectory, and exit 2 when there is none (WEG-281).
- `doctor_repair.rs` — `dreamd doctor` orphaned-socket detection and `--repair` (AILAB-223), with a kill timeout that guards the stdio-lock deadlock.
- `reset_workspace.rs` — `dreamd reset workspace` (WEG-15).
- `uninstall.rs` — `dreamd init --uninstall-project` registry removal and idempotency.
- `uninstall_cmd.rs` — `dreamd uninstall` / `dreamd update` (AILAB-226), in-process with a fake process stopper.
- `update_cmd.rs` — `dreamd update` restart contract (AILAB-552), in-process with a fake process stopper.
- `cross_home_stop.rs` — `dreamd update` stops only processes under the same `$HOME` (AILAB-584).
- `mcp_stdin_eof.rs` — `dreamd mcp` exits promptly when the client closes stdin.
- `mcp_coexistence.rs` — `dreamd mcp` next to Anthropic's reference memory server (AILAB-221).
- `cli_help.rs` — WEG-20 (DR-803) **in-process** snapshot tests for every published CLI surface: `--help` for the top level, for every subcommand (`archive`, `blame`, `doctor`, `dream`, `forget`, `init`, `mcp`, `memory`, `migrate`, `recall`, `reset`, `salience-drift`, `score`, `service`, `setup`, `status`, `uninstall`, `update`, `vectors`, `version`, `watch`), and for the nested `reset workspace`, `memory branch|checkout|branches|delete|diff|bisect`, `memory bisect start|good|bad|run`, and `service install|start|restart|status|uninstall`; plus the `--version` and `version` format contract from WEG-18. One `.snap` file per surface, 40 in total.

## Snapshot workflow (`cli_help.rs`)

Snapshots live in `crates/dreamd-cli/tests/snapshots/`. They are compared on every `cargo test` run and a mismatch panics the test.

When clap help text or the WEG-18 version format intentionally changes:

```
cargo insta review       # interactive accept/reject per pending snapshot
cargo insta accept       # accept all pending
cargo insta reject       # discard all pending (.snap.new files)
```

A pending change writes a `*.snap.new` file next to the existing `*.snap`. `cargo insta review` walks them one at a time; accepted ones overwrite the baseline. Always read the diff carefully — a snapshot mismatch is the canary for unintended drift in the documented surface.

## What the redaction filters mask

The `*_help.snap` snapshots capture clap-generated text. It's fully deterministic across machines and builds; no filters are applied.

The two version snapshots (`version_short.snap` for the `VERSION_SHORT` const, `version_long.snap` for `render_long()`) carry compile-time metadata that drifts per build:

| Field    | Real value example         | Redacted to    |
| -------- | -------------------------- | -------------- |
| commit   | `54dc788…` (7-char SHA)    | `[sha]`        |
| build    | `2026-05-13`               | `[date]`       |
| target   | `x86_64-unknown-linux-gnu` | `[target]`     |
| vectors  | `on` / `off` (the `vectors` cargo feature) | `[state]` |

Filters are applied via `insta::with_settings!({ filters => vec![...] }, { assert_snapshot!(...) })` so the snapshot file stores the redacted form. The `\S+` patterns for commit, build date, and target also capture the `"unknown"` sentinel that tarball builds (no `.git/`) produce — so source-tarball builds pass the same snapshots, no special handling needed.

**Locked literals — review carefully on mismatch:** the `dreamd <workspace version>` prefix (currently `dreamd 1.0.0`; a version bump must update both version snapshots), `schema:1.0` / `schema:  1.0`, every field label (`commit:`, `built:`, `target:`, `schema:`, `build:`, `vectors:`), and the multi-line column alignment. Drift in any of these is a real change to the WEG-18 install-debug contract, not snapshot noise.

## In-process bind, not subprocess

`cli_help.rs` binds directly to `dreamd::cli::Cli` (via `clap::CommandFactory::command()`) and to `dreamd::commands::version::{VERSION_SHORT, render_long}`. The integration test never spawns the `dreamd` binary. Rationale (WEG-20 AC): subprocess capture introduces build-product staleness, target-triple drift, and stdout-encoding flakiness for zero coverage gain over the in-process bind.

Color is forced off via `Cli::command().color(clap::ColorChoice::Never)` so developer machines (TTY-attached) generate the same snapshots as CI (no TTY).

## Verification commands

```
cargo test -p dreamd --test cli_help     # snapshots only
cargo test --workspace                   # all crates, all tests
```
