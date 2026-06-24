# `dreamd-cli` integration tests

## Test files

- `init_golden.rs` — byte-exact stdout fixtures for `dreamd init` (first run + re-run + no-project-root).
- `version_output.rs` — subprocess regression checks for `--version` / `version` (vergen sentinel leak guard).
- `cli_help.rs` — WEG-20 (DR-803) **in-process** snapshot tests for every published CLI surface (`--help` for top-level and each subcommand: `init`, `dream`, `mcp`, `doctor`, `watch`, `reset`, `reset workspace`, `version`, plus the `--version` and `version` byte-exact format contract from WEG-18).

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

Snapshots 1–9 (the `--help` snapshots) capture clap-generated text. It's fully deterministic across machines and builds; no filters are applied.

Snapshots 10–11 (`VERSION_SHORT` const, `render_long()` fn) carry compile-time vergen metadata that drifts per build:

| Field    | Real value example         | Redacted to    |
| -------- | -------------------------- | -------------- |
| commit   | `54dc788…` (7-char SHA)    | `[sha]`        |
| build    | `2026-05-13`               | `[date]`       |
| target   | `x86_64-unknown-linux-gnu` | `[target]`     |

Filters are applied via `insta::with_settings!({ filters => vec![...] }, { assert_snapshot!(...) })` so the snapshot file stores the redacted form. The `\S+` patterns also capture the `"unknown"` sentinel that tarball builds (no `.git/`) produce — so source-tarball builds pass the same snapshots, no special handling needed.

**Locked literals — review carefully on mismatch:** `dreamd 0.1.0-rc.2`, `schema:1.0` / `schema:  1.0`, every field label (`commit:`, `built:`, `target:`, `schema:`, `build:`), and the multi-line column alignment. Drift in any of these is a real change to the WEG-18 install-debug contract, not snapshot noise.

## In-process bind, not subprocess

`cli_help.rs` binds directly to `dreamd::cli::Cli` (via `clap::CommandFactory::command()`) and to `dreamd::commands::version::{VERSION_SHORT, render_long}`. The integration test never spawns the `dreamd` binary. Rationale (WEG-20 AC): subprocess capture introduces build-product staleness, target-triple drift, and stdout-encoding flakiness for zero coverage gain over the in-process bind.

Color is forced off via `Cli::command().color(clap::ColorChoice::Never)` so developer machines (TTY-attached) generate the same snapshots as CI (no TTY).

## Verification commands

```
cargo test -p dreamd --test cli_help     # snapshots only
cargo test --workspace                   # all crates, all tests
```
