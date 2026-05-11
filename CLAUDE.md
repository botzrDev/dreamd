# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project status

Pre-release. `src/main.rs` is currently a `Hello, world!` stub and `Cargo.toml` is a single-crate placeholder at version `0.0.0`. The intended architecture (below) lives in `context/PRD.md` and `context/AGILE/plan1.md` — both gitignored and local-only. Treat those documents as the engineering ground truth; the code on disk has not yet caught up to them.

## What dreamd is

A local-first, single-binary daemon that provides a portable memory layer for AI coding agents. It exposes a standardized `.agent/` folder (`working/`, `episodic/`, `semantic/`, `personal/`) and a local HTTP API; an MCP server maps to that API so Claude Code, Cursor, OpenCode, etc. can share memory across harnesses. The file system is the source of truth — the daemon reads/writes plain files (markdown, JSONL) so users can edit them by hand.

## Commands

```
cargo build              # build the binary
cargo run                # run it
cargo test               # tests (none yet)
cargo clippy             # lint
cargo fmt                # format
```

The agile plan (DR-005) calls for a `Justfile` or `cargo xtask` with `dev/test/bench/release/lint` targets; not yet present. CI matrix (DR-003) is planned for Linux/macOS/Windows but not yet wired.

## Target architecture (when implementing toward v0.1)

The intended layout is a Cargo workspace, not a single crate (DR-002):

- `dreamd-protocol` — shared serde types only; deps limited to `serde` + `chrono`
- `dreamd-core` — modules: `api` (axum), `io` (FS/WAL), `index` (tantivy), `dream` (consolidation pipeline), `vcs` (git2)
- `dreamd-store`, `dreamd-server`, `dreamd-cli`

State management is an actor model: a single `MemoryCoordinator` task owns mutable state; API handlers and the file watcher send intents over `tokio::sync::mpsc`. Do not introduce parallel writers to the JSONL or index — every mutation goes through the coordinator.

## Load-bearing engineering decisions (do not change without re-reading the PRD)

These are the decisions whose violation would silently break the system. They came out of an explicit pressure-test pass:

1. **JSONL append durability.** All appends to `AGENT_LEARNINGS.jsonl` go through one `tokio::sync::Mutex<File>` owned by the coordinator. Each write: serialize → ensure trailing `\n` → single `write_all` → `sync_data`. The `POST /api/v1/learn` 201 response must not return until `sync_data` completes. Concurrent third-party writers to the JSONL are **not** supported in v0.1 despite PRD FR-1.2 — this is a deliberate scope cut.

2. **Tantivy salience scoring is computed at query time, not indexed.** Storing the score would force daily re-indexing as `age_days` drifts. Schema fields: `content` (TEXT), `timestamp_sec` (u64 fastfield), `pain` (f64 fastfield), `importance` (f64 fastfield), `recurrence` (u64 fastfield). Implement a custom `Collector` + `Scorer` that fetches FastFields and computes:

   ```
   salience = exp(-age_days / 14.0) * (pain / 10.0) * (importance / 10.0) * (1.0 + ln(1.0 + recurrence))
   final_score = bm25 * salience
   ```

   Tantivy 0.23+ removed index-time sorting; do not rely on it. Indexing is incremental (5-second commit cadence), never a nightly rebuild.

3. **Dream cycle WAL.** Before any destructive op (replacing `LESSONS.md`, pruning JSONL), write `dream_in_progress.wal` containing `WalIntent` entries (`ReplaceSemanticMemory`, `PruneEpisodicMemory`, `Commit`). On startup, if the WAL exists, run compensating cleanup before serving traffic. Tested by `kill -9` mid-cycle and asserting `.agent/` is either pre- or post-cycle, never in-between.

4. **LLM cost cap and prompt versioning.** Estimate tokens with `tiktoken-rs` before each dream-cycle call; abort and fall back to deterministic mode if the estimate exceeds `$0.10`. Prompts are `include_str!`-bundled with a version ID like `dream-cycle/v1.1@2026-MM-DD`, written into `LESSONS.md` frontmatter. A `--no-llm` mode must always work without network. The `personal/` layer is excluded from LLM calls unless `--share-personal`.

5. **Local API security is not optional.**
   - **Unix:** bind the axum server to a UDS at `~/.agent/dreamd.sock`, `0600` perms, with middleware that validates `SO_PEERCRED` (Linux) / `getpeereid` (macOS) on every request — connecting UID must match daemon owner.
   - **Windows:** bind `127.0.0.1` on an ephemeral port; require a bearer token written to `~/.agent/auth.json` with Windows ACLs.
   - Reject TCP binding to non-localhost without `--insecure`.

6. **MCP tool names.** The MCP server exposes `search_nodes` (→ `/api/v1/recall`) and `append_node` (→ `/api/v1/learn`). These names match the Anthropic reference memory server intentionally; do not rename. The MCP server is the **primary v0.1 distribution surface** — `npx dreamd-mcp` is the install path the README leads with, not `dreamd service install`.

7. **Schema versioning is mandatory.** Every persisted record carries `schema_version: "1.0"`. Add a `dreamd migrate` path before changing it.

## API contract

Endpoints (axum, JSON, all under `/api/v1`):

- `POST /learn` — append episodic event; returns 201 only after `fdatasync`
- `GET /recall?q=&k=` — BM25 × salience search
- `POST /dream` — manual cycle trigger (202, async)
- `POST /migrate` — schema migration

Schemas are specified in `context/PRD.md` §Tech Schemas. The `AgentLearning` struct (timestamp ISO 8601, `pain`/`importance` as `f32` 0–10, `recurrence` as `u32`, `skill_action` as the clustering key) is the canonical episodic record.

## Performance targets (NFRs from PRD)

- Idle RSS < 30 MB
- Stripped release binary < 15 MB
- Recall P50 < 1 ms / P99 < 5 ms warm at 10k entries (the agile plan softens the public claim to `<5ms P50 warm, <50ms P99 cold` until benchmarked)

When changing index, scoring, or hot-path code, run `cargo bench` (criterion benches, DR-208) and check the binary-size CI gate (DR-809).

## Repo conventions

- License: Apache-2.0 (ratified DR-009)
- Public-facing names and accounts already claimed: see `~/.claude/projects/-home-austingreen-Documents-botzr-projects-dreamd/memory/dreamd_surface_area.md` and `npm_account.md`
- The `context/` and `.claude/` directories are gitignored on purpose — local working notes, not artifacts
- Story IDs in commits/PRs follow `DR-XXX` (see `context/AGILE/plan1.md` for the backlog)

## Planning discipline (locked 2026-05-09 after grill round 6)

**Authoritative resolution stack:** PRD Part IV > PRD Part III > PRD Parts I-II > plan1.md grill-revision > plan1.md prior text. When editing either document, treat later layers as overrides; do not silently rewrite earlier text — add resolution sections with cross-references, mirroring the existing Part III pattern.

**Velocity floor:** Plan against **18-22 sustained pts/sprint** with a first-sprint spike of 28-32 that decays. The original `~30 pts/sprint` figure in `context/AGILE/plan1.md` Appendix B is a celebration number, not a planning number. Cumulative complexity (debugging unfamiliar stacks, ops setup, comms reactive work, dogfooding overhead at ~10% of weekly capacity) eats raw build hours faster than the original plan allowed. Sprint 1 is the first measurement; recalibrate Sprints 2-4 from real data, not vibes.

**Scope-discipline rule (applies from grill round 7 onward):** Any further grilling-round scope addition must trade 1:1.5 against existing scope — net +10 points of new work means -15 points of existing work. The grilling round must surface a candidate cut list as part of every "this should be locked" recommendation; the founder retains veto on what gets cut. Without this rule the meta-process generates work faster than the velocity floor can absorb regardless of whether each individual addition is correct. Six rounds added 46 points; round 7 cannot continue the pattern. **Round 7 (2026-05-09 CEO PRD review) was applied with proposed cuts (DR-906, DR-911, DR-410, DR-913 deferred/folded; DR-909 acceptance softened) totaling ~10 pts against ~7 pts of additions; founder vetoed nothing in the trade.**

**Grilling cadence rule (locked grill round 7 / 2026-05-09):** **No further grilling rounds before Sprint 1 ships.** Late-stage grilling rounds before measured velocity exists are the highest-risk cadence — they add scope to a plan whose realistic capacity is still hypothetical. Any "round 8" content the founder wants to surface defers to the post-Sprint-1 retrospective at minimum, where Sprint 2–4 plans can absorb additions through the velocity-gate cut sequence. The v0.1.1 scope freeze gets its own scheduled grilling round between week 9 (v0.1 ships) and week 10 (v0.1.1 freeze) — calendar event, not "if the founder feels like it."

**v0.1.1 scope freeze:** The cuts taken in Q6 (LLM dream cycle, OpenCode adapter, Windows lifecycle, semantic indexing pipeline DR-211) defer to v0.1.1, which gets its own scope freeze ONE WEEK after v0.1 ships. v0.1.1 is a real release with its own discipline — not an "everything that didn't make v0.1" graveyard. Kill criterion: if v0.1.1 is over capacity at end of Sprint 5, OpenCode adapter drops to v0.1.2.

**Launch target:** v0.1 ships **week 9** (was week 7; slipped two weeks after Q6). Pre-write the slip-announcement post in week 6.

**v0.1 wedge sentence (sacred):** *"AGENTS.md is what you wrote down. dreamd is what your agent learned."* Recommendations that weaken this are higher-bar; recommendations that strengthen it can be made aggressively. The post-Q6 v0.1 wedge framing is *"salience-scored cross-harness episodic recall + on-demand deterministic consolidation"* — LLM-assisted lessons land at v0.1.1, do NOT lead the README or HN draft with them.

## dreamd-cli package name is `dreamd`, not `dreamd-cli`

The `[package]` name in `crates/dreamd-cli/Cargo.toml` is `dreamd`.
The directory is `dreamd-cli`; the package name and binary name are both `dreamd`.

Correct invocations:
  cargo test -p dreamd
  cargo run -p dreamd -- <subcommand>
  cargo build -p dreamd

`-p dreamd-cli` will error with "package not found." Do not use it in bare
prompts, CI scripts, or verification commands.

Source: WEG-9 dev report, 2026-05-12.

## Error output belongs on stderr; verification pattern for CLI commands

Any `dreamd` subcommand that exits non-zero must write its error message to
stderr, not stdout. Success output goes to stdout only.

Verification pattern (paste into bare prompts and Step 5 checks):
  # stdout must be empty on error:
  cargo run -q -p dreamd -- <subcommand> 2>/dev/null
  # stderr must contain the error message:
  cargo run -q -p dreamd -- <subcommand> 2>&1 1>/dev/null

WEG-9 incident: "no project root" message was initially routed to stdout.
Caught in Step 5; fixed before commit. Exit code 2 for missing-root, 1 for
other I/O errors.

Source: WEG-9 dev report, 2026-05-12.

## vergen-gitcl `fail_on_error(false)` emits sentinels, not unset env vars

`vergen-gitcl` (and umbrella vergen 9+) with `fail_on_error(false)` does NOT
leave failed-instruction env vars unset. It emits the literal string
`VERGEN_IDEMPOTENT_OUTPUT` as the value. So `option_env!("VERGEN_GIT_SHA")`
returns `Some("VERGEN_IDEMPOTENT_OUTPUT")` on tarball-from-crates.io builds,
NOT `None`.

Any fallback pattern that relies on `match option_env!(...) { Some(s) => s,
None => "unknown" }` is broken — the `Some` arm fires with sentinel content.
Worst case: a 7-char-truncated SHA reading `VERGEN_` ships to crates.io users
and no test catches it because the with-`.git` build path looks fine.

Correct pattern: explicit sentinel-substitution after the `option_env!`:

```rust
const VERGEN_PLACEHOLDER: &str = "VERGEN_IDEMPOTENT_OUTPUT";

const fn str_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes(); let b = b.as_bytes();
    if a.len() != b.len() { return false; }
    let mut i = 0;
    while i < a.len() { if a[i] != b[i] { return false; } i += 1; }
    true
}

const fn or_unknown(s: &'static str) -> &'static str {
    if str_eq(s, VERGEN_PLACEHOLDER) { "unknown" } else { s }
}
```

The rename-`.git` simulation is the only thing that catches this pre-merge;
keep it in every vergen-touching ticket's verification block.

Also: vergen-gitcl 1.0.8 re-exports `Emitter` from `vergen-lib 0.1.6` but its
build/cargo/rustc feature flags route through `vergen`. vergen ≥9.1 brings in
`vergen-lib 9.1`, causing duplicate-`vergen-lib` trait mismatches at the
`add_instructions(...)` call sites. Pin `vergen = "=9.0.6"` as a build-dep to
keep the resolver from upgrading; revisit the pin in v0.1.1's dep-audit pass.

Source: WEG-18 dev report, 2026-05-13.

## clap auto-`--version` prepends the bin name

clap's `#[command(version = LITERAL)]` formats `--version` output as
`<bin_name> <LITERAL>`. If `LITERAL` already includes the bin name (e.g.,
`"dreamd 0.0.0 (...)"`), the output becomes `"dreamd dreamd 0.0.0 (...)"` —
double prefix, byte-exact spec contracts break.

Fix: `#[command(disable_version_flag = true)]` + manual `-V` / `--version`
handling in the dispatch. Side effect: `cli.command` becomes `Option<Command>`
because `--version` is valid with no subcommand; add an explicit exit-2-to-
stderr arm for the missing-subcommand case.

Source: WEG-18 dev report, 2026-05-13.

## Compile-time string assembly: `const_format` over `LazyLock`

For CLI strings that must be `&'static str` (clap's `version = ...` attribute,
embedded asset identifiers, etc.), the established dreamd-cli pattern is
`const_format` — `concatcp!` for assembly, `str_index!` for slicing — not
`std::sync::LazyLock<String>`. Reasons:

- Keeps the value usable in const position (clap attributes, match arms).
- `str_index!(..7)` on a 7-char `"unknown"` fallback string is a no-op; on a
  40-char vergen SHA it returns the first 7 — same code path, no panic.
- Adds one tiny pure-Rust dep (`const_format = "0.2"` → `konst`), no runtime
  init ceremony.

Use `LazyLock<String>` only if you genuinely need allocation-backed assembly
(format args that const_format can't express, runtime env reads).

Source: WEG-18 dev report, 2026-05-13.
