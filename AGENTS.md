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

**Last updated:** 2026-08-24

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
  - Exemplar lookup is the live JSONL only; missing → skip + WARN. Do **not** add a snapshot fallback — cited exemplars are pinned and never decay (see `pin-is-the-decay-exemption`), so the recovery path it would buy is unreachable on the normal route.
  - Never index with defaulted scores when the exemplar is unfindable — skip the document instead.
  - Do not "fix" this by changing the formula: ARCHITECTURE.md decision #2 and `salience.rs:9-11` lock the arithmetic shape verbatim.
- **Cross-refs:** `layer-semantic-is-not-embeddings`, `delete-term-must-target-a-unique-field`, `recency-of-a-derived-doc-is-its-derivation-time`

### pin-is-the-decay-exemption

- **Rule:** `pinned` is the decay-exemption mechanism, and the dream cycle applies it automatically. `apply_pin_unpin` sets `event.pinned |= cited_ids.contains(...)` (`consolidation.rs:245`) on every exemplar a lesson cites, `should_decay` returns `false` immediately for pinned records (`decay.rs:33-35`), and consolidation runs before the pruner inside one WAL envelope (`dream_cycle.rs:106-107`) — so the pin always lands first. **A cited exemplar does not age out at 90 days.** Do not build a second exemption.
- **Why:** AILAB-205 rev 2 specced a snapshot-fallback lookup on the premise that every exemplar decays at ~90 days, giving lessons a hard lifetime. The premise was false and the step was cut in rev 3. Cost: a scope expansion approved on bad evidence, plus in-flight dev work. The reasoning error was reading `should_decay`'s age and salience gates while skipping the `pinned` short-circuit three lines above them.
- **How to apply:**
  - Before claiming a record decays, check `pinned` first — it is the outermost gate in `should_decay`, ahead of both age and salience.
  - `dreamd archive --force-unpin` (`cli.rs:87,132`) is the supported way to clear a pin, so "unpinned then pruned" is reachable — but it is an explicit operator action, not the default lifecycle.
  - The real lifecycle gap runs the other way: nothing retires a stale lesson (AILAB-699). When reasoning about a derived artifact's lifetime, check the *never-removed* direction too.
- **Cross-refs:** `semantic-docs-need-inherited-pain-importance`, `recency-of-a-derived-doc-is-its-derivation-time`

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

### linear-todo-can-already-be-on-main

- **Rule:** Before queuing the only Linear Todo, grep the live tree for the feature. A Todo is not proof the work is unshipped.
- **Why:** 2026-08-15 paired-dev pull: AILAB-205 was the sole dreamd-eng Todo, but `IndexerMsg::IndexSemanticLessons`, `lsn_`, and `tests/semantic_indexing.rs` (cases 01–10) were already on `main` (`787e957`, `5e66918`). Recurred 2026-08-23: AILAB-748 still **In Progress** on Linear while `DRAIN_TIMEOUT` / `with_drain_timeout` around `drain_daemon` are on `main` (`5afb741`). Recurred 2026-08-24: AILAB-201 / 200 / 191 still **Backlog** and AILAB-748 still **In Progress** after `af256c1` (versioned prompt), `7e3c4b4` (citations), `2b12ae2` (MCP descriptions), `75f5a3f` (`--dry`), `5afb741` (drain timeout).
- **How to apply:** Search for the ticket id, the new type/msg variant, and the named test file. If present and the spec's test cases exist, close Linear and pick the next Backlog ticket instead of writing a second implement prompt. Status **Backlog** is not proof of unshipped either — check `main` before treating P0 Backlog as the next implement.
- **Cross-refs:** none

### changelog-historical-entries-stay-put

- **Rule:** Keep a Changelog historical version sections are immutable. Correct a factual error from a shipped tag under `## [Unreleased]`, never by rewriting the old bullet.
- **Why:** AILAB-698 Linear AC cited `CHANGELOG.md:113`; live line is `:119` under `[0.1.0-rc.1]`, not `[0.1.0]`. Line numbers on CHANGELOG rot; the section heading is the identity.
- **How to apply:** Identify the bullet by heading + wording, not by AC line number. Leave rc.* / released entries byte-identical unless the user explicitly authorizes a historical rewrite.
- **Cross-refs:** none

### daemon-state-module-avoids-wal-autobiography-cycle

- **Rule:** Shared `state.json` types (`DaemonState`, `AutobiographySkip`) live in a third module both `wal` and `autobiography` depend on. Do not put `DaemonState` in `wal.rs` and `AutobiographySkip` in `autobiography.rs`.
- **Why:** AILAB-167 pre-flight: the typed writer must be callable from both modules, and `DaemonState` needs `Option<AutobiographySkip>`. Splitting those two types across `wal.rs` / `autobiography.rs` is a compile-time cycle.
- **How to apply:**
  - New `crates/dreamd-core/src/daemon_state.rs`; `pub mod daemon_state` in `lib.rs`.
  - Move `STATE_SCHEMA_VERSION` there and `pub use` it from `wal.rs` so `crate::wal::STATE_SCHEMA_VERSION` citations keep compiling (`migrate-from-to-is-record-schema`).
  - Re-export `AutobiographySkip` from `autobiography` so `doctor.rs` / `cli.rs` / `setup.rs` do not churn.
- **Cross-refs:** `migrate-from-to-is-record-schema`

### weg378-skip-midfile-halt-torn-tail

- **Rule:** Episodic JSONL policy is SPEC §88 / WEG-378: skip `\n`-terminated blank or unparseable mid-file lines; halt only a no-`\n` torn tail (`episodic::scan` / `read_all` / `recover`). Halt-at-blank is a regression.
- **Why:** AILAB-166 Linear AC still said unify to halt-at-blank, but `read_all` + consolidation/decay/indexer already share that skip policy. Re-queuing 166 would have undone `read_all_skips_blank_midfile_line`. Closed as shipped 2026-08-18.
- **How to apply:** Before queuing a JSONL-reader ticket, grep `fn read_all` and the skip tests. Quarantine-to-sidecar is AILAB-163 (still open), not a reader-unification ticket.
- **Cross-refs:** `jsonl-torn-tail-validation`, `linear-todo-can-already-be-on-main`

### ailab-162-drain-not-sigterm

- **Rule:** SIGTERM + socket unlink shipped in AILAB-301 / WEG-268. The remaining AILAB-162 hole was the post-`select!` coordinator/indexer drain (the comment that named it WEG-283). That drain is now in the working tree: unlink first, then `drain_daemon` (coordinator `Shutdown`, then index `try_unwrap`→`shutdown` or `flush`). Do not re-queue "add SIGTERM".
- **Why:** 2026-08-18 look-ahead almost re-queued SIGTERM over `weg268_sigterm_socket`. The drain itself shipped unbounded (Shutdown can sit behind `RunDreamCycle`). Bounding that drain is **AILAB-748**, not AILAB-210 — 210 is Mode-X client refcount / idle-exit (Backlog). Do not conflate the two grace windows.
- **How to apply:** Clone `Arc<Supervisor>` / primary handle *before* `build_router`. `shutdown(self)` cannot run from `Drop`. Do not async-shutdown `supervisor_map`. Idle `systemctl stop` is the `try_unwrap` success arm (`select!` drops `serve_uds` + router + `AppState`); `flush` is the busy-daemon fallback because `uds_server` `tokio::spawn`s connection tasks that drop of the serve future does not cancel. Coordinator `Shutdown` is `rx.close()` then receive-and-discard: queued *ahead* lands, queued *behind* is dropped.
- **Cross-refs:** `linear-todo-can-already-be-on-main`, `no-hoisted-stdio-lock-across-tantivy`, `indexer-shed-is-not-replay-healed`, AILAB-748

### indexerror-lives-in-index-map

- **Rule:** `IndexError` is defined in `server/index_map.rs`, not `tantivy_handle.rs`. Linear AILAB-161 still names the handle file as the type's home.
- **Why:** The tuple struct is the map's error type; `tantivy_handle` / `dream_cycle` / `http/state` only construct it. Queuing "change IndexError in tantivy_handle.rs" misses the definition and the other two construction files.
- **How to apply:** Grep `pub struct IndexError` / `pub enum IndexError` first. AILAB-170 owns the typed `SchemaIncompatible` wipe-gate and the module split; a 161-sized enum is ChannelClosed vs terminal, not that split.
- **Cross-refs:** `linear-todo-can-already-be-on-main`, `indexer-shed-is-not-replay-healed`

### replay-two-pass-already-filters

- **Rule:** `replay_two_pass` already does one `episodic::read_all` and `events.into_iter().filter(|ev| id > watermark)`. Do not re-queue "remove the double-parse."
- **Why:** AILAB-161 item 2 (June audit) shipped with the WEG-378 shared scan. Re-doing it is a no-op; a second `read_jsonl_events` would be a regression.
- **How to apply:** Read `fn replay_two_pass` before queuing any replay/index-error cleanup. The remaining 161 work is the enum + registry lockfile.
- **Cross-refs:** `linear-todo-can-already-be-on-main`, `weg378-skip-midfile-halt-torn-tail`

### indexer-shed-is-not-replay-healed

- **Rule:** Startup replay does not heal a middle gap. `replay_two_pass` keeps `ev.id.as_str() > last_indexed_id` (newest committed id, not a contiguous prefix). Live appends no longer shed: `handle_append` awaits `tx.send(IndexerMsg::Append)`, so a full 1024-slot indexer channel parks the coordinator instead of dropping. A `Closed` tail past the watermark is still replay-healed; a gap followed by any later indexed event is still skipped forever.
- **Why:** The pre-fix `try_send` + Full drop logged "recoverable via next-startup replay". False for any shed-then-continue sequence. The ticket closed the live hole by awaiting send; it did not change the watermark.
- **How to apply:** Do not document channel backpressure as if replay closes a shed gap — there is no shed on Full anymore. Do not "fix" remaining crash holes by rewriting `replay_two_pass` into a contiguous watermark unless that is its own ticket. HTTP 503 is **not** "coordinator parked": in-flight learns wait in the 256-slot inbox (`resp_rx.await` has no timeout); only overflow of that inbox trips `Supervisor::try_send`'s 100 ms timeout, and only on HTTP. In-process `dreamd mcp` sends on a raw coordinator clone with no timeout and waits. `Supervisor::drain`'s deadline-free `Shutdown` can now park behind the indexer and consume `DRAIN_TIMEOUT`, skipping the index flush (lost last commit → rebuild on next open, not JSONL loss). The indexer task never sends back to the coordinator, so coordinator→indexer is acyclic.
- **Cross-refs:** `ailab-162-drain-not-sigterm`, `replay-two-pass-already-filters`, `spec-deadlock-fixture-names-must-match-the-tree`

### spec-deadlock-fixture-names-must-match-the-tree

- **Rule:** A §4 "fixture must be gone" grep that searches names the spec invented will pass green with the real fixtures intact. Pair the spec names with a live-tree grep of the functions those line numbers actually name.
- **Why:** indexer-shed.v2 §1.6 named `coordinator_still_succeeds_when_indexer_channel_full` / `channel_full_leaves_recall_stale_until_restart_replay`. The tree had `coordinator_continues_when_indexer_channel_full` and `channel_saturation_stale_recall_until_restart_replay` at the cited lines. §4 grepped only the invented names.
- **How to apply:** Before locking a negative test-name grep, `rg 'async fn'` the file around the cited lines. Grep both spellings if the spec and tree disagree.
- **Cross-refs:** `indexer-shed-is-not-replay-healed`

### gated-consumer-is-what-makes-a-full-channel-test-real

- **Rule:** A capacity-1 pre-fill test is vacuous if an ungated forwarder/consumer drains the filler before the coordinator sends. Gate the consumer; assert delivery count or recall, not just "append Ok".
- **Why:** indexer-shed first green pass: `full_indexer_channel_still_reaches_recall_after_flush` drained the pre-fill, never met a full channel, passed on `try_send`+drop. Mutation-injecting `try_send` + silent Full made the gated rewrite fail (`1 != 3`; recall assertion).
- **How to apply:** Mutation-inject `try_send`+silent Full and require the test to fail. Current-thread runtime comments are not the proof; the count/recall assertion is.
- **Cross-refs:** `indexer-shed-is-not-replay-healed`

### git-diff-name-only-misses-untracked

- **Rule:** An anti-pattern allowlist that is `git diff --name-only | grep -vE '^(allowed files)$'` cannot see untracked files. A new module the spec *requires* will not appear, and a new file the spec *forbids* will not fail the gate.
- **Why:** AILAB-167 report-back: `daemon_state.rs` was `??` and the allowlist line was empty of it. The only red hit was PM-side `AGENTS.md`. The spec's own file-scope grep was structurally blind to the ticket's main deliverable.
- **How to apply:** Pair `git diff --name-only` with `git ls-files --others --exclude-standard` (or `git status --porcelain`). Do not treat an empty `git diff --name-only` allowlist as proof that only the listed paths changed.
- **Cross-refs:** none

### init-is-a-create-only-statejson-writer

- **Rule:** `dreamd init` has its own private `State` in `commands/init.rs` that scaffolds `state.json`. It is a third writer, but create-only — not a rebuild of an existing file. AILAB-167's "single writer" AC is `wal` + `autobiography` only.
- **Why:** Linear AILAB-167 names those two files. Folding `init.rs` in would also retouch `init_golden.rs` (651 B stdout lock) without changing the eat-on-cycle bug.
- **How to apply:** Do not queue `init.rs` onto a state.json RMW ticket unless the user expands scope. `fs::write` at scaffold time is acceptable; cycle mutations go through the typed RMW writer.
- **Cross-refs:** `daemon-state-module-avoids-wal-autobiography-cycle`

### registry-flock-gates-on-registry-not-home-exists

- **Rule:** `uninstall_project` may skip the registry flock only when `registry.toml` is absent (true no-op). Any RMW takes `lock_exclusive` first and re-checks existence under the lock. Never gate the flock on `daemon_home.exists()` alone.
- **Why:** AILAB-161 report-back: `if daemon_home.exists() { Some(lock) } else { None }` left a TOCTOU against a concurrent first `dreamd init` that created the home+registry after uninstall skipped the lock, then ran the RMW unlocked.
- **How to apply:** Probe `registry_toml()` for the early return; lock then re-probe before read/retain/write/chmod. Do not `create_dir_all` the daemon home from uninstall just to take the lock. Regression: `concurrent_first_init_and_uninstall_keeps_registered_root` in `commands/init.rs`.
- **Cross-refs:** `indexerror-lives-in-index-map`

### guards-vs-mandates-same-line

- **Rule:** When a v2 §3 snippet shows a multi-line call and a §5 anti-pattern grep requires two tokens on the same physical line (e.g. `with_drain_timeout` and `drain_daemon(drain_supervisor`), the grep wins — write the call on one line (or rewrite the grep before queueing). Never ship a snippet that fails its own guard.
- **Why:** AILAB-748 report-back (third instance logged by the implementing agent): copying the four-line §3 Step 2 form failed §5 grep #1; the single-line call site is semantically identical and rustfmt-stable.
- **How to apply:** Before handing a bare prompt, dry-run every §5 line against the §3 snippets as if they were already in the tree. Prefer greps that match across `\n` (`rg -U`) only when intentional; otherwise keep call-site examples single-line when a same-line guard exists.
- **Cross-refs:** none

### config-llm-keys-are-flat

- **Rule:** LLM settings on `Config` are flat (`provider`, `model`, `endpoint`, `cost_cap_usd`), not a nested `[dream.llm]` table. Linear AILAB-204 originally said `[dream.llm]`; the issue description was reconciled to flat keys when 204 moved to Done (2026-08-23).
- **Why:** WEG-14 / DR-112 shipped flat keys and a commented template; a nested table would break `CONFIG_TEMPLATE` parse tests and every existing `config.toml`.
- **How to apply:** Read `crates/dreamd-core/src/config.rs`. Do not add `[dream.llm]`. AILAB-196 has shipped: the dream cycle reads and enforces the flat `cost_cap_usd`; it did not introduce `max_cost_usd` or a nested table.
- **Cross-refs:** `migrate-from-to-is-record-schema`

### doctor-cluster-health-flag-already-exists

- **Rule:** `dreamd doctor --cluster-health` already diffs `semantic/recurrence_counts.json` against `compute_promoted_clusters`. Tickets that "add cluster-health" must extend that flag's output, not invent a parallel flag.
- **Why:** Linear AILAB-196 AC (May 2026) asked to show estimated next-cycle cost on `dreamd doctor --cluster-health` as if the flag were new. The flag and sidecar-drift checks shipped earlier; 196 is additive `next_cycle_est_usd=` output. A new flag would split the doctor surface and miss existing tests.
- **How to apply:** Call a sibling reporter from the existing `if flags.cluster_health` arm. Do not fold cost into `check_cluster_health`'s `bool`. Do not call `select_lesson_or_retire` / `run_cluster_engine` from doctor (`select-lesson-or-retire-unlinks`).
- **Cross-refs:** `select-lesson-or-retire-unlinks`, `linear-todo-can-already-be-on-main`

### no-llm-must-not-skip-daemon-proxy

- **Rule:** `--no-llm` threads through `POST /api/v1/dream` as `x-dreamd-no-llm: 1`. It does not skip the daemon proxy. Only `--no-commit` skips the proxy (existing CI path).
- **Why:** Skipping the proxy while `dreamd watch` owns the JSONL races the coordinator (WEG-271). Linear AILAB-204 asked for a CLI flag without an HTTP channel; the live proxy POSTs an empty body.
- **How to apply:** `client::proxy_dream_cycle(..., no_llm)` sets the header; `RunDreamCycle { no_llm, ... }` on the coordinator. See `assignments/AILAB-204.v2.md`.
- **Cross-refs:** `coordinator-not-mutex-file`

### nfr-2-stripped-binary-is-20mb

- **Rule:** CI `size-gate` fails if stripped `target/release/dreamd` is **> 20 MB** (`MAX_BYTES=$((20 * 1024 * 1024))` in `.github/workflows/ci.yml`; soft warn at 16 MB). There is **no headroom** on current `main`. Do not queue a crate that grows the binary until a measurement on this commit is under the gate.
- **Why:** AILAB-204 report-back cited **16.77 MB** and the founder raised 15→20 MB on 2026-08-23. That 16.77 figure was never the CI number. AILAB-204's own commit (`580cc44`) already fails size-gate at **20.42 MB**; `217f15e` (current main) is **20.47 MB** on ubuntu-latest ([run 32768136453](https://github.com/botzrDev/dreamd/actions/runs/32768136453)) and **20.23 MB** stripped locally. Nightly stays green because that workflow does not run size-gate, which hid the red. Standalone/non-LTO tiktoken probes at ~+3.7 MB; fat LTO + a *reachable* `cl100k_base` is **+1.82 MB** (19.07 MB local / ~19.31 MB projected CI). The 3.7 MB number is the wrong planning input once the profile block ships.
- **How to apply:** Strip and `stat` `target/release/dreamd` on the commit you are about to grow, or read the latest `size-gate` job log — do not reuse the 16.77 MB CHANGELOG/AGENTS number. Raising the gate again is a founder call. Soft warn is 16 MB and fires on every current main build. AILAB-196 ships `[profile.release] lto = "fat"` + `codegen-units = 1` (measured 17.25 MB without tiktoken, 19.07 MB with a *reachable* tokenizer locally). Shrink or cut, do not silently raise `MAX_BYTES`. A size number from a build that never calls the new code is a false pass (`fat-lto-dead-strips-unreferenced-tokenizer`).
- **Cross-refs:** `linear-todo-can-already-be-on-main`, `config-llm-keys-are-flat`, `fat-lto-dead-strips-unreferenced-tokenizer`

### fat-lto-dead-strips-unreferenced-tokenizer

- **Rule:** Fat LTO will drop `tiktoken-rs` (and `cl100k_base`) from the stripped binary if nothing in a bin crate reaches the estimator. A size measurement taken after adding the dep but before `compose_lesson_body` calls `estimate_prompt_cost` is a false pass.
- **Why:** AILAB-196 STOP: local fat-LTO + tiktoken was byte-identical to LTO-only (18,086,912) while `strings dreamd | grep -ciE 'cl100k|endoftext|tiktoken'` was 0. A `black_box(estimate_prompt_cost(…))` probe in `main.rs` made the assets link (20,000,192; +1.82 MB). The production reachability path is the compose gate, not a probe left in the CLI.
- **How to apply:** Report NFR-2 from a build where the production caller exists. Confirm tokenizer markers with `strings`. Do not commit a `black_box` in `crates/dreamd-cli/src/main.rs`.
- **Cross-refs:** `nfr-2-stripped-binary-is-20mb`

### rustdoc-trips-word-grep-that-meant-call-sites

- **Rule:** An anti-pattern grep for a banned *API* (`count_tokens`, a URL path) will also match rustdoc that explains why that API is unused, unless comment lines are filtered. The `do not|don't|never` negation filter does not catch "we do not call X" if X is on a different sentence-shape.
- **Why:** AILAB-196 §4 originally grepped `count_tokens|count-tokens|messages/count` in `llm.rs`. Phase 1 rustdoc explaining the rejected remote token-count endpoint tripped it. Same family as `guards-vs-mandates-same-line`.
- **How to apply:** Grep call syntax / URLs (`\.count_tokens\(`, `messages/count`) and drop `///` / `//!` / `//` lines. Do not ban the English words in docs.
- **Cross-refs:** `guards-vs-mandates-same-line`

### spec-grep-file-wide-hits-the-tickets-own-fixtures

- **Rule:** An anti-pattern grep over a whole `.rs` file will hit `#[cfg(test)]` fixtures that *must* call the banned production function. Narrow to production code (above `#[cfg(test)]`) or to `git diff` added lines — do not fail a ticket because pre-existing tests seed state with the helper the production path must not use.
- **Why:** AILAB-196 §4 grep 3 (`select_lesson_or_retire|run_cluster_engine` in `commands/doctor.rs`) was already red at `d2d5fa9` on three drift-test lines (comment + two `run_cluster_engine` calls). Those tests need a real sidecar on disk. `head -n` of the `#[cfg(test)]` line (784) was empty; `git diff` added lines naming the symbols all carried `never` / `rather than`. Weakening the fixtures to satisfy the grep would delete the only proof the sidecar-diff arm works.
- **How to apply:** Pair a file-wide grep with (1) a production-only slice through the first `#[cfg(test)]` and (2) `git diff -- path | grep -E '^\+.*(banned)'`. Same family as `spec-grep-count-guard-contradicts-ac-test-code` / `guards-vs-mandates-same-line`.
- **Cross-refs:** `select-lesson-or-retire-unlinks`, `rustdoc-trips-word-grep-that-meant-call-sites`, `guards-vs-mandates-same-line`

### v2-beats-linear-ac

- **Rule:** For dreamd-eng tickets that have `assignments/AILAB-*.v2.md`, implement the v2, not Linear AC as written.
- **Why:** AILAB-204 Linear still said `genai`, `[dream.llm]`, and env-only after the v2 spec and the ship. The implementing tree followed the v2 file. Next tickets (201, 196, 200) inherit those reconciliations.
- **How to apply:** If `assignments/AILAB-NNN.v2.md` exists, that file is the contract. Treat Linear bullets as historical. Do not re-introduce `[dream.llm]`, `tiktoken-rs` on 201, or env-only credentials.
- **Cross-refs:** `config-llm-keys-are-flat`, `linear-201-ac-is-pre-llm-module`

### linear-201-ac-is-pre-llm-module

- **Rule:** AILAB-201 Linear AC names `crates/dreamd-core/src/dream/prompts/v1.1.txt`, per-lesson `LESSONS.md` frontmatter, and `prompt_version`. Live (shipped 2026-08-23): crate `dreamd-core`, `LESSON_PROMPT = include_str!("prompts/v1.1.txt")`, `VERSIONED_PROMPT_ID = "dream-cycle/v1.1@2026-08-23"`, artifact `.agent/semantic/LESSONS.md` with **file-level** `prompt_version`. `UNVERSIONED_PROMPT_ID` / `UNVERSIONED_LESSON_PROMPT` / `"llm-unversioned"` are gone from `crates/dreamd-core/src/`.
- **Why:** May 2026 AC predates AILAB-204's `llm.rs` module and the shipped LESSONS.md schema. Implementing Linear as written would add a phantom `src/dream/` module and a second frontmatter shape.
- **How to apply:** Do not re-introduce the throwaway constants. Citation validation and a `citations:` frontmatter key are AILAB-200. A prompt-byte edit must move `VERSIONED_PROMPT_ID` in the same commit (reviewer, not insta).
- **Cross-refs:** `v2-beats-linear-ac`, `posix-trailing-nl-widens-prompt-seam`

### posix-trailing-nl-widens-prompt-seam

- **Rule:** Moving a Rust `"` `\`-string into a `.txt` file adds a POSIX trailing newline the literal did not have. If the assembler still concatenates `"\n\nCluster: "`, the seam gains a blank line. insta's `trim_end` snapshot is blind to that.
- **Why:** AILAB-201: HEAD literal 594 B → file 595 B (`+0x0a`). `build_lesson_prompt` kept `"\n\nCluster: "`, so `prose\n\nCluster:` became `prose\n\n\nCluster:`. Spec allowed the newline; a dedicated `ends_with("...\n") && !ends_with("\n\n")` test pinned the seam.
- **How to apply:** When extracting prompt/config bytes into a file, assert the assembler seam, not only an insta snapshot. Do not `.trim_end()` in `build_lesson_prompt` to hide it.
- **Cross-refs:** `linear-201-ac-is-pre-llm-module`

### required-struct-field-forces-out-of-allowlist-literal-sites

- **Rule:** Adding a required field to a public struct forces a literal at every existing struct-literal construction site. An allow-list that names only the product files will miss those compile-fix sites; `git diff --name-only` plus `git ls-files --others --exclude-standard` is the gate, not an empty allow-list.
- **Why:** AILAB-200 added `LessonsFile.citations: Vec<String>`. Product work stayed in the allow-list, but `server/tantivy_handle.rs` and `tests/semantic_indexing.rs` each gained a one-line `citations: Vec::new()` so test helpers still compiled. That is not scope creep; omitting the field does not compile.
- **How to apply:** When a v2 adds a required field, expect helper/test literals outside the product allow-list. Prefer `Default` only if the crate already uses it for that type. Do not fail 200-style tickets for those one-line fills; do fail new product logic in those files.
- **Cross-refs:** `git-diff-name-only-misses-untracked`

### cargo-test-lib-filter-is-substring

- **Rule:** `cargo test -p dreamd-core --lib llm` is a **substring** filter on test names, not “the `llm` module.” It also runs `dream_cycle` / HTTP tests whose names contain `llm`.
- **Why:** AILAB-200 report-back: `--lib llm` printed 35 passed (311 filtered) because `llm_success_writes_model_text…` and friends matched. That extra coverage is real; it is not proof the command was scoped to `llm.rs` alone.
- **How to apply:** For a module-only gate use `cargo test -p dreamd-core --lib -- llm::`. Read the “filtered” count. Do not treat a 35-pass `--lib llm` paste as “35 tests in llm.rs.”
- **Cross-refs:** `cargo-run-dreamd-needs-bin`

### linear-search-nodes-layer-filter-does-not-exist

- **Rule:** MCP `search_nodes` takes `query` + `k` only (`SearchNodesParams`). HTTP `GET /api/v1/recall` takes `q` + `k`. Both call `recall(..., None)`. Hits already carry `source` (`"episodic"` | `"semantic"` via `Layer::as_str`). There is no `layer` / `layer=` argument on either surface.
- **Why:** AILAB-191 Linear AC says “`layer=` filter narrows.” Documenting that would teach agents to pass a param rmcp will reject. Adding the param is a new feature, not “just better descriptions.”
- **How to apply:** Rewrite descriptions to tell agents to read `source`. Do not add `SearchNodesParams.layer`. Do not unify MCP `query` with HTTP `q`. `index.rs` rustdoc still says every v0.1 doc is `Layer::Episodic` — stale after AILAB-205; do not “fix” it on a copy ticket.
- **Cross-refs:** `v2-beats-linear-ac`, `layer-semantic-is-not-embeddings`

### rmcp-tool-description-requires-string-literal

- **Rule:** rmcp 1.7 parses `#[tool(description = ...)]` as a darling `Option<String>`, so the attribute takes a string **literal**, not a const. Keep the copy in a const for tests and duplicate the exact bytes into each `#[tool]`; assert `MemoryMcpServer::search_nodes_tool_attr().description` equals the const. `#[allow(dead_code)]` on the const is required because only `#[cfg(test)]` reads it — same shape as the existing `tool_router` allow at `mcp/mod.rs:151`.
- **Why:** AILAB-191 v2 asked to use consts in all three attributes. That does not compile on rmcp 1.7. Duplicating without a round-trip test would let unix vs `not(unix)` `search_nodes` drift.
- **How to apply:** Do not fight the macro. Do not `#[cfg(test)]` the const (hides rustdoc / `not(unix)` links). Do not add a layer param to “avoid” the duplication.
- **Cross-refs:** `linear-search-nodes-layer-filter-does-not-exist`

### dry-skips-proxy-unlike-no-llm

- **Rule:** `dreamd dream --dry` must skip the daemon proxy. `--no-llm` must not. `--no-commit` is the only write-path flag that skips the proxy today.
- **Why:** `POST /api/v1/dream` runs the real cycle (WAL, LESSONS.md, JSONL pins, sidecar, index). A dry preview that proxies would write. `--no-llm` skipping the proxy while `dreamd watch` owns the JSONL races the coordinator (WEG-271).
- **How to apply:** Dry is a separate CLI arm that calls a read-only preview and never `try_proxy_to_daemon`. The write path still sends `x-dreamd-no-llm: 1` through the proxy. `--dry --no-commit` is just `--dry` (nothing to commit). `--dry --auto` stays invalid.
- **Cross-refs:** `no-llm-must-not-skip-daemon-proxy`, `coordinator-not-mutex-file`

### select-lesson-or-retire-unlinks

- **Rule:** `select_lesson_or_retire` is not a read. On empty promotion it `remove_file`s `LESSONS.md` (AILAB-699). `run_cluster_engine` always writes `recurrence_counts.json`.
- **Why:** A preview that reused the write-path select/cluster functions would mutate the store — the opposite of `--dry`.
- **How to apply:** Dry uses `compute_promoted_clusters` (already pub, no disk) plus a new `preview_select_lesson` that does not unlink and does not call `run_cluster_engine`. Do not call `wal::begin_cycle`, `write_selected_lesson`, decay, index, or autobiography on the dry path.
- **Cross-refs:** `dry-skips-proxy-unlike-no-llm`

### fixture-now-sec-is-not-the-corpus-clock

- **Rule:** A “advance past the recurrence window” test must add the window to the fixture’s newest event timestamp, not to the test’s `NOW_SEC` constant.
- **Why:** AILAB-341: `tests/fixtures/dream-cycle-snapshot/` is dated 2026-05, a year ahead of `NOW_SEC = 1_747_137_600` (2025-05-13). `NOW_SEC + 30d` still sits inside the window, so a naive retire-preview test keeps promoting.
- **How to apply:** `newest_event_ts + WINDOW_30_DAYS_SEC + 1`. See `preview_without_promotion_reports_none_and_leaves_lessons_md` in `dream_cycle.rs`.
- **Cross-refs:** `select-lesson-or-retire-unlinks`
