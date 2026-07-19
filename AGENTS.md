# AGENTS.md — dreamd

This file is read by Claude Code, Cursor, Cline, Codex, and other MCP-aware coding
agents when working in this repository. It describes what this project builds, how to
navigate and work in it, and the conventions that govern the codebase.

---

## What this repo builds

dreamd is a local-first, single-binary memory layer for AI coding agents. Claude Code,
Cursor, Cline, and other MCP-aware harnesses read and write to a shared `.agent/` folder
via MCP. What one agent learns, the next already knows. The file system is the source
of truth — plain JSONL and Markdown you can cat, grep, git diff, and hand-edit.

**The one job dreamd is hired for:** memory continuity across tools. Not a second brain,
not an ambient capture product, not a Python framework SDK.

---

## Repository layout

  crates/dreamd-core/        Core memory engine: Tantivy BM25 index, salience
                             collector, episodic store, dream cycle WAL.
  crates/dreamd-cli/         CLI binary: init, mcp, watch, dream, doctor, version.
  crates/dreamd-protocol/    Shared types: AgentLearning, EventId, HTTP schemas.
  packages/dreamd-mcp/       Node.js npx shim (dreamd-mcp). Downloads
                             prebuilt binary — no Rust required at runtime.
  adapters/claude-code/      .mcp.json.example for Claude Code users.
  adapters/cursor/           .mcp.json.example + .cursor/rules/dreamd-recall.mdc.
  SPEC.md                    Implementation-agnostic spec for the .agent/ convention.
  CONTRIBUTING.md            Dev setup, PR workflow, DCO sign-off requirement.
  SECURITY.md                Threat model and disclosure policy.
  ARCHITECTURE.md            Load-bearing engineering decisions for contributors.

---

## Build and test

Requires Rust stable.

```bash
cargo build                              # build all crates
cargo test --workspace                   # full test suite
cargo clippy --workspace -- -D warnings  # lint (CI enforces zero warnings)
cargo bench -p dreamd-core               # recall latency benchmarks
scripts/coverage.sh                      # HTML + lcov coverage report
```

All commits require DCO sign-off: `git commit -s`.

---

## Architectural conventions — read before making changes

**Single-writer, append-only episodic log.**
`episodic/AGENT_LEARNINGS.jsonl` is append-only. The coordinator actor holds the write
lock. Do not write to it from outside the coordinator.

**EventId is daemon-minted.**
`AgentLearning.id` is an `evt_`-prefixed Crockford base32 ULID minted by the coordinator.
Any inbound `id` field is overwritten. Clients never supply IDs.

**MCP tool names are locked.**
The MCP server exposes `search_nodes` and `append_node`. These names match the Anthropic
reference memory server intentionally. Do not rename them.

**Salience is computed at query time.**
`BM25 × exp(-age_days/14) × (pain/10) × (importance/10) × (1 + ln(1 + recurrence))`
This is computed from Tantivy fastfields on recall. It is not stored or indexed.

**Dream cycle is WAL-protected.**
Before any destructive op (replacing LESSONS.md, pruning JSONL), write
`dream_in_progress.wal`. On startup, if WAL exists, run compensating cleanup.
`.agent/` must be either pre- or post-cycle, never mid-cycle.

**`unsafe` policy.**
`unsafe_code = "forbid"` at workspace level. `dreamd-core` has a scoped `deny` override
for `detach_double_fork` only, with an explicit SAFETY contract. Do not widen this.

**Unix domain socket only.**
HTTP API binds to `~/.agent/dreamd.sock` with `SO_PEERCRED` UID-match enforcement on
every request. Do not bind to TCP without `--insecure`.

---

## Schema

Every persisted record carries `schema_version: "1.0.0"`. Before changing the schema,
register a real transform in `dreamd-core::migrate` (WEG-133 shipped the stub:
`dreamd migrate --from 1.0.0 --to 1.0.0` only). `skill_action` is the dream-cycle clustering key — segments match `[a-z0-9_]`, joined by `::`, language-first (e.g. `rust::error_handling::axum_rejection`).

---

## v0.1 scope

In scope: BM25 lexical recall, Linux + macOS, deterministic dream cycle, npm distribution.
Out of scope until v0.1.1: Windows, semantic/embedding recall, LLM-assisted dream cycle,
Homebrew install, animated GIF.

Do not implement or document v0.1.1 features in v0.1 code.

---

## License

Apache-2.0. All contributions require DCO sign-off (`git commit -s`).

---

## Project inventory — paired-dev-loop

**Last updated:** 2026-07-17

**Stack:** Rust 2021 edition (CI pin `1.95.0`), Axum 0.8, Tokio 1, Tantivy 0.26; no DB
**Manifest(s):** root `Cargo.toml` workspace; members `crates/dreamd-core`, `crates/dreamd-cli` (package name `dreamd`), `crates/dreamd-protocol`
**Static-check command:** `cargo check --workspace`
**Lint command:** `cargo clippy --workspace --all-targets --all-features -- -D warnings` (also `cargo fmt --all -- --check`)
**Test command:** `cargo test --workspace` / `cargo test --all-features --workspace` (CI)
**Test convention:** unit tests in `src/**`; integration tests in `crates/*/tests/` (often `wegNN_*.rs`); unix-only suites use `#![cfg(unix)]`; helper bins under `crates/dreamd-core/tests/bin/`
**Migration dir:** n/a (JSONL + `schema_version: "1.0.0"`; `dreamd migrate` stub via `dreamd-core::migrate` / WEG-133)
**Spec dir:** `assignments/`, naming `WEG-<n>.v2.md` (v1 often Linear-only; leave v1 intact when a local file exists)
**Spec v2 convention:** `assignments/WEG-*.v2.md` next to any local v1; established by existing specs
**Memory location:** `AGENTS.md` (this file) — drift catalog section below
**Main branch:** `main`

---

## Paired-dev-loop drift catalog

### coordinator-not-mutex-file

- **Rule:** All JSONL mutations go through `MemoryCoordinator` actor messages (`AppendLearning` / `RunDreamCycle` / `Shutdown`). There is no `Mutex<File>` writer.
- **Why:** Linear/older docs still say “Mutex&lt;File&gt; + fdatasync”; live code serializes via `&mut self` on the actor run loop (`coordinator.rs`, ARCHITECTURE.md §1).
- **How to apply:**
  - Spawn via `MemoryCoordinator::open` / `open_at` + `tokio::spawn(coordinator.run())`, or `Supervisor::start`.
  - Send `MemoryCoordinatorMsg::AppendLearning { learning, client_dedup_key, response_tx }`.
  - Prefer `tx.send(...).await` in torture tests; HTTP handlers use `Supervisor::try_send` (100 ms timeout → 503).
- **Cross-refs:** `weg12-direct-channel-not-http-hammer`

### weg12-direct-channel-not-http-hammer

- **Rule:** WEG-12 / DR-110 concurrency torture uses the **direct coordinator channel**, not real UDS HTTP / `POST /api/v1/learn`.
- **Why:** Default coordinator capacity is 256 and HTTP `try_send` times out at 100 ms (`lifecycle.rs`); a naïve 1000-way HTTP hammer returns many 503s and cannot assert “exactly 1000 lines” without retries/capacity gymnastics. Linear AC explicitly allows the direct channel for v0.1.
- **How to apply:**
  - Integration test under `crates/dreamd-core/tests/` with `#![cfg(unix)]`.
  - Fan out 1000 `AppendLearning` with `client_dedup_key: None` (or unique keys).
  - Validate with raw bytes + `episodic::assess_log_health` / parse-as-`AgentLearning`.
- **Cross-refs:** `coordinator-not-mutex-file`, `agentlearning-placeholder-id`

### agentlearning-placeholder-id

- **Rule:** Callers construct `AgentLearning` with a valid placeholder `EventId` (`evt_` + 26 Crockford chars); the coordinator overwrites `id`, `schema_version`, and `timestamp` on durable write.
- **Why:** `EventId` rejects invalid strings at parse/serde time; tests historically use `evt_01ARZ3NDEKTSV4RRFFQ69G5FAV` or `evt_00000000000000000000000000`.
- **How to apply:**
  - See `LearnIngress::build_agent_learning` and coordinator unit tests.
  - Direct-channel tests may use any `skill_action` string (no ingress gate). Prefer `::` form for consistency.
- **Cross-refs:** none

### jsonl-torn-tail-validation

- **Rule:** “No torn lines” means the file ends with `\n`, `assess_log_health(...).torn_tail_bytes == 0`, and every `\n`-terminated non-empty line deserializes as `AgentLearning`.
- **Why:** `episodic::scan` skips mid-file corrupt `\n`-terminated lines but treats a final no-`\n` fragment as a torn tail (WEG-378).
- **How to apply:** Prefer `dreamd_core::episodic::{read_all, assess_log_health}` over ad-hoc `lines()` alone; also assert unique `id`s.
- **Cross-refs:** none

### npm-dreamd-mcp-unscoped

- **Rule:** The npx package is unscoped `dreamd-mcp`, never `@dataprime1/dreamd-mcp`. Prefer floating `npx dreamd-mcp` in new docs; adapter `.mcp.json.example` pins may lag `packages/dreamd-mcp/package.json` — bump pins only in an explicit pin-sweep ticket.
- **Why:** Linear AC for WEG-91 still cited `@dataprime1/…`; live `package.json` is `"name": "dreamd-mcp"` with `mcpName: "io.github.botzrDev/dreamd"`. WEG-91 left examples on `rc.2` while the package was already `rc.3`; the 2026-07-19 pin-sweep (`assignments/adapter-pin-sweep.v2.md`) brought consumer-facing pins to `@0.1.0-rc.3` without bumping the Rust binary train (`Cargo.toml` still `0.1.0-rc.2`).
- **How to apply:** Grep for `@dataprime1/dreamd-mcp` before shipping adapter/docs copy. Check version via `packages/dreamd-mcp/package.json` or `npm view dreamd-mcp version`. Do not conflate npm pin with workspace crate version.
- **Cross-refs:** none

### migrate-from-to-is-record-schema

- **Rule:** `dreamd migrate --from` / `--to` take **episodic** `RECORD_SCHEMA_VERSION` (`"1.0.0"`), never daemon `STATE_SCHEMA_VERSION` (`"1.0"` / `dreamd version` display), never `index::SCHEMA_VERSION` (`"index/1.3"`). Index self-heals; migrate does not bak or rewrite it.
- **Why:** Linear WEG-133 AC said `"1.0"`; three independent streams exist on disk. Registering `"1.0"→"1.0"` would teach the wrong token.
- **How to apply:**
  - Registry identity via `dreamd_protocol::RECORD_SCHEMA_VERSION` (see `dreamd-core::migrate`).
  - CLI `.bak` only `episodic_jsonl()` + `state_json()`; report index read-only.
  - Docs: `docs/migrate.md`.
- **Cross-refs:** none

### doc-first-append-via-uds-learn

- **Rule:** Non-MCP / documentation-pattern adapters teach durable append via UDS `POST /api/v1/learn` (placeholder `EventId` / `timestamp` / `schema_version`), never hand-edit or `echo >>` of `AGENT_LEARNINGS.jsonl`. There is no `dreamd learn` CLI in v0.1.
- **Why:** Linear WEG-128 AC said “append … JSONL directly”; that fights `coordinator-not-mutex-file` and `agentlearning-placeholder-id`. Shipped fix: `adapters/aider/CONVENTIONS.md.template` + README.
- **How to apply:**
  - Copy curl shape from `docs/http-api.md` `POST /api/v1/learn`; set reserved `source_harness` (e.g. `"aider"`).
  - Recall = `/read` (or equivalent) of `LESSONS.md` / JSONL; append requires a live daemon (`dreamd watch` preferred default).
  - Anti-pattern: any new adapter doc that instructs editing the JSONL file by hand.
- **Cross-refs:** `coordinator-not-mutex-file`, `agentlearning-placeholder-id`
