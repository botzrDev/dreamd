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

**Last updated:** 2026-07-27

**Stack:** Rust 2021 edition (CI pin `1.95.0`), Axum 0.8, Tokio 1, Tantivy 0.26; no DB
**Manifest(s):** root `Cargo.toml` workspace; members `crates/dreamd-core`, `crates/dreamd-cli` (package name `dreamd`), `crates/dreamd-protocol`
**Static-check command:** `cargo check --workspace`
**Lint command:** `cargo clippy --workspace --all-targets --all-features -- -D warnings` (also `cargo fmt --all -- --check`)
**Test command:** `cargo test --workspace` / `cargo test --all-features --workspace` (CI)
**Test convention:** unit tests in `src/**`; integration tests in `crates/*/tests/` (often `wegNN_*.rs`); unix-only suites use `#![cfg(unix)]`; helper bins under `crates/dreamd-core/tests/bin/`
**Migration dir:** n/a (JSONL + `schema_version: "1.0.0"`; `dreamd migrate` stub via `dreamd-core::migrate` / WEG-133)
**Spec dir:** `assignments/`, naming `AILAB-<n>.v2.md` for Botzr-AI-Labs / dreamd-eng tickets (legacy `WEG-<n>.v2.md` still valid if present; v1 often Linear-only)
**Spec v2 convention:** `assignments/AILAB-*.v2.md` (or `WEG-*.v2.md`) next to any local v1; leave v1 intact when a local file exists
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

- **Rule:** The npx package is unscoped `dreamd-mcp`, never `@dataprime1/dreamd-mcp`. Use the **floating** form `npx -y dreamd-mcp` (and `["-y", "dreamd-mcp"]` in MCP configs) in ALL user-facing docs and `.mcp.json.example` files — npx re-resolves the `latest` dist-tag on each fresh spawn (verified npm 10.9.4: `GET registry.npmjs.org/dreamd-mcp … cache revalidated`), so a floating config never goes stale. Do **not** hard-pin a version in copy-paste examples (a hard pin is the one form that never tracks new releases); pin only to reproduce a specific build.
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

### cargo-run-dreamd-needs-bin

- **Rule:** Always `cargo run -p dreamd --bin dreamd -- …` (and the same `--bin dreamd` for any help/CLI smoke). Never bare `cargo run -p dreamd -- …`.
- **Why:** Package `dreamd` (`crates/dreamd-cli`) ships two bins — `dreamd` and `generate_man`. Bare `cargo run -p dreamd` errors with “could not determine which binary to run” and prints nothing on stdout; piping to `grep -c` then yields `0` as a **false pass** (caught in AILAB-495 report-back).
- **How to apply:** Put `--bin dreamd` in every bare-prompt / v2 verify block that runs the CLI via cargo. Prefer `cargo test -p dreamd --test cli_help` for snapshot gates (no bin ambiguity). Unit tests for CLI modules live under **`--lib`**, not `--bin dreamd` — `main.rs` is a thin shim, so `cargo test -p dreamd --bin dreamd` reports `0 passed` and exits 0 (silent false pass; caught in AILAB-550). Use `cargo test -p dreamd --lib <module>` (e.g. `setup_mcp`) or `--test <integration>`.
- **Cross-refs:** none

### no-hoisted-stdio-lock-across-tantivy

- **Rule:** Never hold `stdout.lock()` / `stderr.lock()` across `TantivyIndexHandle::open`, `writer.commit()`, or any path that can block on Tantivy’s `segment_updater` while tracing’s console layer may write to stderr.
- **Why:** AILAB-575 / AILAB-561 — `run_doctor` hoisted StdoutLock/StderrLock for the whole run; `open`’s commit blocked the main thread waiting on `segment_updater`, which logged via tracing → stderr and deadlocked on the same process-wide lock. In-process unit tests (Vec&lt;u8&gt; writers) stayed green; only the real CLI / `tests/doctor_repair.rs` hung. The earlier `handle.shutdown()` “fix” never reached.
- **How to apply:**
  - Pass unlocked `std::io::stdout()` / `stderr()` (per-`writeln!` lock/release) for any CLI arm that can open a writer-spawning index — see `cli.rs` `run_doctor` + comment on `run_repair`.
  - Regression guard: `crates/dreamd-cli/tests/doctor_repair.rs` spawn + 30s kill timeout with `DREAMD_LOG=info` pinned (off would disarm the trigger).
  - Other `run_*` arms may still hoist locks today if they never wait on a logging Tantivy writer thread; do not cargo-cult unlock everywhere without a trigger, but do not reintroduce a hoisted lock on doctor.
  - Every arm that still hoists carries an inline `// lock-ok (AILAB-583):` rationale naming why it never opens an index, and any new or changed arm that can open one must use unlocked handles instead.
- **Cross-refs:** none

### layer-semantic-is-not-embeddings

- **Rule:** `Layer::Semantic` / `layer=semantic` names the **LESSONS.md document layer** in the Tantivy index. It is not vector/embedding semantics. Anything indexed under it is still scored by lexical BM25 × salience.
- **Why:** AILAB-205 (DR-211) is a pure-Tantivy ticket with no new dependencies, but `AGENTS.md` §"v0.1 scope" lists "semantic/embedding recall" as one deferred item, and the Linear backlog files DR-211 next to the genuine vector tickets (AILAB-188 `fastembed-rs`, AILAB-182 RRF fusion, AILAB-181 vector opt-in). A dev or PM reading "semantic indexing pipeline" plausibly reaches for embeddings and pulls in a dependency the ticket never wanted.
- **How to apply:**
  - `index.rs:56-67` — the `Layer` enum is `Episodic | Semantic`, two document layers in one index.
  - Embedding work is AILAB-188/182/181 and is *not* on the v0.2 Consolidation row of `context/planning/future-directions.md`.
  - Anti-pattern grep for any DR-211-adjacent ticket: `! grep -rniE 'fastembed|embedding|vector|cosine' crates/dreamd-core/src/ --include='*.rs'`.
- **Cross-refs:** `semantic-docs-need-inherited-pain-importance`

### semantic-docs-need-inherited-pain-importance

- **Rule:** Any document indexed into Tantivy MUST carry non-zero `pain` and `importance`, or it is unrankable. `collector.rs:114-115` reads both with `.unwrap_or(0.0)` and `salience()` multiplies by `(pain/10) × (importance/10)`, so a missing fastfield collapses the whole score to `0.0`.
- **Why:** AILAB-205 — the AC asked for "dual-document scoring: episodic and semantic docs ranked together" without saying where a lesson's pain/importance comes from. A lesson has neither natively. The failure is silent: the document indexes fine, commits fine, matches the BM25 query fine, and then scores 0.0 and never appears in results. `test_zero_pain_produces_zero_score` (`collector.rs:400-413`) already pins the behavior.
- **How to apply:**
  - Lessons inherit **pain and importance** from the exemplar event named by `Lesson.id` (`lessons.rs:24`); `recurrence` = promoted-cluster size.
  - Exemplar lookup is live JSONL → `.agent/.dreamd/snapshots/*.jsonl` → skip+WARN. Decayed records are archived, not deleted (`decay.rs:163-179`), and the effective decay gate is `!pinned && age > 90d`, so without the snapshot leg every lesson dies at 90 days.
  - Never index with defaulted scores when the exemplar is unfindable — skip the document instead.
  - Do not "fix" this by changing the formula: ARCHITECTURE.md decision #2 and `salience.rs:9-11` lock the arithmetic shape verbatim.
- **Cross-refs:** `layer-semantic-is-not-embeddings`, `delete-term-must-target-a-unique-field`, `recency-of-a-derived-doc-is-its-derivation-time`

### recency-of-a-derived-doc-is-its-derivation-time

- **Rule:** A derived document's `timestamp_sec` is **when it was derived**, not the timestamp of the source record it was derived from. For a LESSONS.md lesson that is `LessonsFile.last_updated`, never the exemplar event's timestamp.
- **Why:** AILAB-205 — I specced pain, importance, and timestamp as one "inherit from the exemplar" bundle. Wrong on the third. Salience decays as `exp(-age_days/14)`, which asks *how stale is this claim*; a lesson consolidated today from a 90-day-old exemplar is not a 90-day-old claim. Stamping the exemplar's timestamp yields `exp(-6.43) ≈ 0.0016`, so the document indexes, commits, matches the query, and then ranks two orders of magnitude below any fresh event — the same silent-burial failure as `semantic-docs-need-inherited-pain-importance`, just slower and harder to spot in a test that only asserts presence.
- **How to apply:**
  - Assert *rank*, not presence, when testing a derived document. A test that only checks the doc comes back passes under both the right and wrong timestamp.
  - Do not add decay or expiry logic to derived docs. Retirement is structural: the dream cycle rewrites LESSONS.md wholesale, so a cluster that stops recurring drops out and its lesson vanishes at the next `delete_term(layer="semantic")`.
  - Re-stamping a derived doc on every rebuild is correct, not a bug — the rewrite is evidence the pattern still recurs.
- **Cross-refs:** `semantic-docs-need-inherited-pain-importance`

### delete-term-must-target-a-unique-field

- **Rule:** `writer.delete_term(...)` in the Tantivy path targets `fields.event_id` (unique per document) or `fields.layer` (whole-layer wholesale replace). **Never** `skill_action` / cluster key — episodic and semantic documents share it, so a cluster-keyed delete takes every event in the cluster with it.
- **Why:** AILAB-205's Linear AC said `delete_term(cluster_key)` then `add_document`. Episodic docs carry `skill_action` (`index.rs:52`, written at `index.rs:149`), so that would silently delete the cluster's entire event history on every dream cycle. Both shipped delete sites use `event_id` deliberately — see the WEG-45 rationale at `index.rs:47-49` ("resolves to exactly one document"), the decay pruner at `tantivy_handle.rs:477`, and the recurrence sidecar at `tantivy_handle.rs:588`.
- **How to apply:**
  - Per-document delete → `Term::from_field_text(fields.event_id, id)`.
  - Whole-layer replace (LESSONS.md is rewritten wholesale each cycle) → `Term::from_field_text(fields.layer, Layer::Semantic.as_str())`; `layer` is `STRING` so the match is exact and cannot touch episodic docs.
  - Give non-event documents a distinct `event_id` namespace (e.g. `lsn_<lesson id>`), or the decay pruner and recurrence sidecar will delete them as collateral — the sidecar then re-adds only the episodic doc, dropping the other silently.
  - Any ticket adding a delete path needs a regression test asserting the *other* layer survives.
- **Cross-refs:** `semantic-docs-need-inherited-pain-importance`, `shared-helper-dual-consumer-blast-radius`

### display-name-grep-excludes-external-slugs

- **Rule:** Zero-hit “retired display name” greps exclude external identifiers (`linear.app/` document slugs, registry URLs, etc.). Keep the href byte-identical unless product explicitly retitles the remote doc and mints a new slug.
- **Why:** AILAB-593 — AC wanted zero hits on the retired benchmark display name *and* an unchanged Linear bake-off doc URL whose slug still embeds that retired token. Mutually unsatisfiable without excluding the URL from the display-name gate.
- **How to apply:** Filter `| grep -viE 'linear\.app/'` (or equivalent) on rename ACs; do not invent a new Linear URL; do not pad lines with “no longer” to cheat anti-pattern filters.
- **Cross-refs:** none
