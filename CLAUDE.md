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
