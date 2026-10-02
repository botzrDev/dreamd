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

**Where it fits (2026-09-26).** dreamd is the Linux-hosted proof of one pillar of **dreamOS**,
*the first computer you can hand your keys to*, whose kernel is momo (charter: momo
`docs/rfc/RFC-009`, amended by RFC-011, **accepted** 2026-09-26, Linear BZR-1075; local docs
issue BZR-1077): memory as plain files a person owns, with provenance — *the Memory*, in the
product's words. RFC-011 also names the rule the ledger grows toward — every effect traceable
to the data that caused it; no datum causes an effect above its own trust label — as a
design that needs its own spec before any code. It changes nothing shipped: no trust label
exists, `source_harness` is caller-asserted, `docs/provenance.md` is format only (BZR-154)
and records derivation, not trust. **Constraints that stand:** do not claim momo
`MemoryRegion` objects (the BZR-827 rule below); do not document a trust label, taint, or
"the Memory" as if it shipped; the momo repo is private, and while the product name
dreamOS and its one-sentence destination may appear in this repository's docs as a pointer
(ruled 2026-09-26 on BZR-1075, RFC-011 A0), announcing, previewing or marketing the product
before its v1 stays forbidden — a pointer, never a launch.

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

## Scope (1.0.0)

Shipped in v0.1.0: BM25 lexical recall, Linux + macOS, deterministic dream cycle, npm distribution.

Shipped in v0.1.1: LLM-assisted dream cycle (opt-in; deterministic fallback), `LESSONS.md` semantic indexing (Tantivy document layer, still BM25 × salience — **not** embeddings), Windows `watch` + `learn` over loopback TCP + bearer, `dreamd service` on Linux / macOS / Windows.

On `origin/main` at `0064355` the workspace version is `1.0.0`. Tag `v1.0.0` is `dacd95d` (GitHub Release published 2026-10-02, stable). Manifest shas were filled in `42e8e62`. There is no `v0.2.0-alpha.1` tag. npm `dreamd-mcp` is `1.0.0` on both `latest` and `next` (registry check 2026-10-02). Publishing again stays the human step in `RELEASING.md`.

Still out of scope: vector/embedding recall, hybrid retrieval, Windows `write_atomic` (dream cycle and index), Homebrew install, animated GIF, auto dream cycle.

The optional `vectors` feature downloads a model and does not rank. `rrf::fuse` is unused by recall. Forget, memory branches, and the in-RAM latency page are shipped. Do not describe vector recall or hybrid retrieval as shipped. `v1.0.0` is a published tag; `v0.2.0-alpha.1` was never tagged.

---

## License

Apache-2.0. All contributions require DCO sign-off (`git commit -s`).

---

## Project inventory — paired-dev-loop

**Last updated:** 2026-10-02

**Stack:** Rust 2021 edition (CI pin `1.95.0`), Axum 0.8, Tokio 1, Tantivy 0.26; no DB
**Manifest(s):** root `Cargo.toml` workspace; members `crates/dreamd-core`, `crates/dreamd-cli` (package name `dreamd`), `crates/dreamd-protocol`
**Static-check command:** `cargo check --workspace`
**Lint command:** `cargo clippy --workspace --all-targets --all-features -- -D warnings` (also `cargo fmt --all -- --check`)
**Test command:** `cargo test --workspace` / `cargo test --all-features --workspace` (CI)
**Test convention:** unit tests in `src/**`; integration tests in `crates/*/tests/` (often `wegNN_*.rs`); unix-only suites use `#![cfg(unix)]`; helper bins under `crates/dreamd-core/tests/bin/`
**Migration dir:** n/a (JSONL + `schema_version: "1.0.0"`; `dreamd migrate` stub via `dreamd-core::migrate` / WEG-133)
**Spec dir:** `assignments/`, naming `BZR-<n>.v2.md` for Botzr-Research / dreamd-eng tickets (legacy `AILAB-<n>.v2.md` and `WEG-<n>.v2.md` still valid if present; v1 often Linear-only)
**Spec v2 convention:** `assignments/AILAB-*.v2.md` (or `WEG-*.v2.md`) next to any local v1; leave v1 intact when a local file exists
**Memory location:** `AGENTS.md` (this file) — drift catalog section below, plus the build-order section
**Main branch:** `main`
**Linear:** team **Botzr-Research**. Shipped board is project **dreamd-eng** (`P-BZR-1`); it has no open issues. The unfinished spine is **Backlog** on project **R&D** (`P-BZR-79`). Order is the section below.

---

## Remaining build order — 2026-09-24

Locked 2026-09-24 for the finish race. Re-checked against `origin/main` (`2e34181`) and Linear on 2026-09-30. Items 1–27 are on `origin/main`. Linear marks the late ones Done on **dreamd-eng** (`P-BZR-1`): 158 was already Done; 151, 188, 181, 182, 177, and 157 moved from R&D Backlog to dreamd-eng Done on 2026-09-30. The dreamd issues still Backlog on **R&D** (`P-BZR-79`) are 149, 152, 207, and 210, and they are not claimable. One ticket at a time per agent. The user runs commits. The August 27 queue numbers are not the order. Live issue ids are `BZR-<n>` on team **Botzr-Research**.

**Do not start a ticket until every ticket above it in this list has landed.** One ticket at a time.

An empty backlog query on `dreamd-eng` is that move. Milestone progress there is not evidence a v0.2 ticket shipped. The dreamd-eng description body still says RFC-011 is proposed; the project summary, BZR-1075, and the "Where it fits" section above record it **accepted** (2026-09-26). BZR-1077 (the local docs pointer) is Done, commit `312983b`.

1. **BZR-187** — on `main` (`fe331bd`). `POST /api/v1/migrate` returns 501 only after the existing auth stack. Spec: `assignments/BZR-187.v2.md`. Unix missing peer UID is 403; Windows missing bearer is 401; missing `X-Agent-Root` is 400. Body schema token is episodic `RECORD_SCHEMA_VERSION` `"1.0.0"`, never daemon `"1.0"`. Documented in `docs/http-api.md`. Does not call `MigrationRegistry` and does not `.bak` the store.
2. **BZR-173** — on `main` (`adb7a40`). `MemoryStore` in `memory_store.rs`: in-process `send().await`, daemon proxy parses `LearnResponse`. HTTP recall stays on `with_index_handle` and shares `recall_to_json`. HTTP learn stays `try_send` → 503. Placeholder body is `LearnIngress::placeholder_learning` (`RECORD_SCHEMA_VERSION`, placeholder `EventId`). Direct dep `async-trait` (already in the lockfile). Off-Unix `DaemonStore::tcp` has not been compiled on this machine. That same commit also added `docs/provenance.md` (BZR-154).
3. **BZR-172** — on `main` (`979ddc3`, with BZR-168). `dream_cycle::run_guarded_cycle` is the only sequencer. The coordinator still runs only filesystem phases and reopens the append fd. `consolidation::DreamCycleError` is `LessonPhaseError`. CLI in-process now hits the 409 guard: a leftover `in_progress` blocks `dreamd dream` until `dreamd watch` runs `recover_on_startup` (that recovery keys off the WAL file). HTTP opens the index sender before dispatch.
4. **BZR-168** — on `main` (`979ddc3`, with BZR-172). `layout::home_dir` is the one resolver: empty `HOME` is unset, no password-database fallback, `USERPROFILE` only when `windows` is true. `init` takes `&DaemonHome`. Registry writes go through `registry::update_registry`. `find_project_root` lives next to `discover`. The Windows MCP arm (`mcp/mod.rs:649`) still calls `dirs::home_dir()`. Do not add a second liveness probe.
5. **BZR-170** — on `main` (`4bb3509`, with BZR-183). Freshness is `server/index_freshness.rs`. The indexer actor, including the semantic pass, prune, and recurrence sidecar, is `server/indexer_actor.rs`. `tantivy_handle.rs` keeps `open` / `flush` / `shutdown` / `close` and re-exports the moved public items. `IndexError::SchemaIncompatible` is the wipe gate. `SCHEMA_VERSION` stays `index/1.3`. Health stays the on-disk watermark.
6. **BZR-183** — on `main` (`4bb3509`, with BZR-170). `LettaStore` in `letta.rs` delegates to an inner `MemoryStore`. No MemFS layout and no MCP wiring. `lib.rs` adds `pub mod letta` in alphabetical order, just above `memory_store`.
7. **BZR-827** — on `main` (`187cc50`). `GrantedStore` / `ContextGrant` in `context_grant.rs` filter recall by `source` (`episodic` / `semantic`) and refuse an append when episodic is not granted. `ContextGrant::both()` returns the inner recall string unchanged. MCP is not wired. Spec: `assignments/BZR-827.v2.md`. Doc: `docs/context-grant.md`. Do not claim momo `MemoryRegion` objects. Do not reopen it.
8. **BZR-198** — on `main` (`f400d82`). `explain=1` on `GET /api/v1/recall` adds a `citations` array; every other caller, including MCP `search_nodes`, keeps the body with no `citations` key. Spec: `assignments/BZR-198.v2.md`. Doc: `docs/observability.md`. Do not reopen it.
9. **BZR-193** — on `main` (`63791b9`). `dreamd blame <query>` prints id, timestamp, skill_action, content, bm25, salience, total, layer. Default `-k` is 5. `--explain` reuses `explain_factors`. `--json` is one compact array. `dreamd recall` stays `-k` 10. Spec: `assignments/BZR-193.v2.md`. Do not start a second copy.
10. **BZR-194** — on `main` (`260f37d`). `dreamd recall --without` / `--without-cluster`, and repeatable `exclude=` on `GET /api/v1/recall`. The filter runs inside the collector before the top-k heap. MCP `search_nodes` and `dreamd blame` do not take it. Spec: `assignments/BZR-194.v2.md`.
11. **BZR-195** — on `main` (`806b764`). `GET /api/v1/observability/salience` and `dreamd salience-drift`. Score is `salience()` with no BM25. Drift reads only the snapshot dated exactly seven UTC days earlier. Spec: `assignments/BZR-195.v2.md`.
12. **BZR-159** — on `main` (`fe331bd`). Branch format spec. Spec: `assignments/BZR-159.v2.md`. Docs only (`docs/branching.md`). Objects go under `.dreamd/branches/`, never the decay archive `.dreamd/snapshots/<date>.jsonl`.
13. **BZR-160** — on `main` (`806b764`). `snapshot::create_autosnap` writes `branches/objects/<id>/` and a `snap-*` ref. It does not write `HEAD`. There is no `dreamd memory snapshot` command. Spec: `assignments/BZR-160.v2.md`.
14. **BZR-150** — on `main` (`702636a`). `dreamd memory branch|checkout|branches|delete`. Refs are two-line files under `branches/refs/`. `HEAD` is `branches/HEAD`. Checkout refuses while the daemon socket exists. Spec: `assignments/BZR-150.v2.md`.
15. **BZR-156** — on `main`. Library `memory_diff::diff_objects` in `702636a`. Command `dreamd memory diff` in `13ae1ca` (same commit as BZR-146). `--json` is one line and ignores `--unified`. The command reads objects and does not check out or test the daemon socket. Spec: `assignments/BZR-156.cli.v2.md`.
16. **BZR-153** — on `main` (`e3b7ecf`). `dreamd memory bisect`. Search range is `snap-*` refs by the timestamp line. No parent pointer. State is `branches/bisect`, beside `HEAD`. Spec: `assignments/BZR-153.v2.md`.
17. **BZR-146** — on `main` (`13ae1ca`, same commit as the diff command). `docs/branching-guide.md` and `scripts/branch-demo.sh`. No screencast and no `docs/competitor-comparison.md`. The guide does not document memory diff flags. The script resolves `dreamd` to an absolute path before `cd` and sets `HOME` to its temp directory. Spec: `assignments/BZR-146.v2.md`.
18. **BZR-154** — on `main` (`adb7a40`, same commit as BZR-173). Merkle ledger spec. Spec: `assignments/BZR-154.v2.md`. Docs only (`docs/provenance.md`). Ledger under `.dreamd/provenance/`, never `snapshots/` or `branches/`. No verifier program in this ticket.
19. **BZR-155** — on `main` (`260f37d`). Provenance recording. Leaf hash is SHA-256 of the event id's UTF-8 bytes. `index_doc` is appended from `commit_and_persist` and from `TantivyIndexHandle::open`. The learn path does not open the ledger. No `embedding` edge.
20. **BZR-148** — on `main`. `provenance::verify` is `e3b7ecf`. `dreamd doctor --provenance` is `ed134e1` (on `origin/main`). The flag prints that report and fails the run on orphans, missing edges, or corrupt lines. `skipped_unknown_kind` is printed and does not fail the check. `--repair` stays the Tantivy rebuild. Spec: `assignments/BZR-148.cli.v2.md` (local; `assignments/` is gitignored). Linear: Done on dreamd-eng.
21. **BZR-158** — on `origin/main` (`e656211`). `forget::cascade` removes one live event under the dream-cycle WAL, drops or unlinks the lesson that names it, and leaves old ledger lines in place. `MemoryCoordinatorMsg::Forget` reopens the append fd on every path except a clean missing id, then sends the existing index messages. Spec: `assignments/BZR-158.v2.md`. Linear: Done on dreamd-eng (2026-09-30).
22. **BZR-151** — on `origin/main` (`29ffdba`). `dreamd forget <id>` calls `forget::cascade`, refuses while the daemon probes live, and `--dry-run` writes nothing. `--proof` writes a `forget-receipt/1.0` file. Spec: `assignments/BZR-151.v2.md`. Linear: Done on dreamd-eng (2026-09-30).
23. **BZR-188** — on `origin/main` (`40ff05a`). Feature `vectors` is off by default (`fastembed` 5.17.4, Apache-2.0). `#[cfg(feature = "vectors")] pub mod vectors` names `BAAI/bge-small-en-v1.5` and does not call `TextEmbedding::try_new`. Spec: `assignments/BZR-188.v2.md`. Linear: Done on dreamd-eng (2026-09-30).
24. **BZR-181** — on `origin/main` (`f786bff`). `dreamd vectors enable` downloads the model only on a `--features vectors` build and exits 2 on the default binary. `dreamd version` / `--version` report `vectors:on` or `vectors:off`. Stripped default `dreamd` is 20,674,976 bytes. The feature build is 46,589,208 bytes and is informational (`size-report-vectors`). Spec: `assignments/BZR-181.v2.md`. Linear: Done on dreamd-eng (2026-09-30).
25. **BZR-182** — on `origin/main` (`edf7869`). `rrf::fuse` fuses two best-first id lists, `RRF_K = 60`, 1-based ranks. Recall does not call it. No ANN, no config table, no benchmark. Spec: `assignments/BZR-182.v2.md`. Linear: Done on dreamd-eng (2026-09-30).
26. **BZR-177** — on `origin/main` (`f48d8e0`). Sample-percentile printer and `docs/benchmarks.md`. Spec: `assignments/BZR-177.v2.md`. Linear: Done on dreamd-eng (2026-09-30).
27. **BZR-157** — on `origin/main` (`2e34181`). Workspace version `0.2.0-alpha.1`, changelog section, and `RELEASING.md` row 7 for the three path-dependency versions. Manifest shas are `PENDING_*`. There is no `v0.2.0-alpha.1` tag. Spec: `assignments/BZR-157.v2.md`. Linear: Done on dreamd-eng (2026-09-30) for the bump. The tag, the draft GitHub release, npm publish, and the MCP registry stay human steps.

**Not claimable**

- **BZR-147** is Done (2026-09-24). The error rename landed in BZR-172 (`LessonPhaseError`) and the learn/recall seam landed in BZR-173. The leftovers — deleting `server/http/types.rs`, merging `client.rs` with `daemon_client.rs`, collapsing the HTTP status mappers — were not done. Do not reopen 147 for them. Open a new ticket if those are still wanted.
- **BZR-171** is Done (2026-09-24). It was the parent epic. Do not implement it as its own change.
- **BZR-207** is not a gate. `rust-toolchain.toml` and `daemon_state::DaemonState` already exist. BZR-168 took `init(&DaemonHome)`. The `state_json()` rerun sentinel was left as-is: scaffold is one atomic rename. Do not reopen 207 to chase that sentinel.
- **BZR-210** stays held until a `watch` remap exists. Do not implement the Linear AC.
- **BZR-149** and **BZR-152** are research. Do not schedule them. The CRDT ticket fights the single-writer log.

**Parallel agents**

- Claim one ticket and its file list before editing. One writer per file.
- Through **BZR-168** the spine is on `main`: 187 (`fe331bd`), 173 (`adb7a40`), 172 and 168 (`979ddc3`). Do not reopen them. **BZR-159** (`docs/branching.md`) landed in `fe331bd`. **BZR-154** (`docs/provenance.md`) landed in `adb7a40`. Their code tickets landed later: 160 in `806b764`, 155 in `260f37d`.
- **BZR-170** and **BZR-183** are on `main` (`4bb3509`). Do not reopen them. 170 owns the Tantivy split (`server/index_freshness.rs`, `server/indexer_actor.rs`, `server/tantivy_handle.rs`, `server/mod.rs`, `server/index_map.rs`, `collector.rs`, `handlers/health.rs`, the cadence test in `http/tests.rs`). 183 owns `letta.rs`, the `pub mod letta` line in `lib.rs`, and `adapters/letta/README.md`.
- **BZR-827** is on `main` (`187cc50`). Do not reopen it. **BZR-198** is on `main` (`f400d82`). Do not reopen it. **BZR-193** is on `main` (`63791b9`). **BZR-194** is on `main` (`260f37d`). **BZR-195** and **BZR-160** are on `main` (`806b764`). **BZR-155** is on `main` (`260f37d`). **BZR-150** and the **BZR-156** library are on `main` (`702636a`). **BZR-153** and **BZR-148**'s `provenance::verify` are on `main` (`e3b7ecf`). **BZR-156**'s command and **BZR-146** are on `main` (`13ae1ca`). `dreamd doctor --provenance` is `ed134e1` (BZR-148). The finish list through BZR-157 is on `origin/main`. Linear marks 151, 188, 181, 182, 177, and 157 Done on dreamd-eng. Do not reopen them. The 1.0.0 bump is `bb84721`, tagged `v1.0.0` at `dacd95d`. Manifest shas are filled (`42e8e62`). The docs pass is `41e4ee5` and `0064355` (`origin/main`). There is no next implement ticket. Do not cut another tag or publish npm from an agent session. Behaviour bugs, founder decisions, and doc gaps left after that docs pass are listed in `context/audits/v1.0.0-docs-audit-remaining-2026-10-02.md` (gitignored; none of those items have tickets). Do not file them unless asked.

Windows atomic writes is on `ROADMAP.md` and has no ticket in this list. Do not invent that work inside one of these tickets.

---

## Paired-dev-loop drift catalog

### privacy-disclosure-tracks-the-shipped-network-story

- **Rule:** `DR413_DISCLOSURE` in `privacy.rs` is the first-run banner printed by `dreamd init` and first MCP spawn. `tests/fixtures/init.golden.txt` is byte-locked to it. When whether the daemon can make network calls changes, rewrite **both** in the same commit. Never leave "planned for v0.X.Y" in the banner after that version is tagged.
- **Why:** The v0.1.1 honesty pass updated README / ROADMAP / SECURITY but missed this string. Users who ran init from the then-published npm package still saw "LLM-assisted dream cycles … planned for v0.1.1" after that release had shipped the LLM path.
- **How to apply:** Edit the const and the golden together; `cargo test -p dreamd --test init_golden` is the gate. Do not rewrite historical CHANGELOG `[0.1.0]` bullets that also said `--dry` was planned (`changelog-historical-entries-stay-put`).
- **Cross-refs:** `changelog-historical-entries-stay-put`

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
- **How to apply:** Identify the bullet by heading + wording, not by AC line number. Leave rc.* / released entries byte-identical unless the user explicitly authorizes a historical rewrite. An anti-pattern grep for a retired path (`docs/security.md`) will also hit those historical bullets — pair with `git diff` added lines, do not rewrite the old entry to satisfy the grep (`changelog-historical-entries-stay-put`).
- **Cross-refs:** `rustdoc-trips-word-grep-that-meant-call-sites`

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

### spec-grep-status-word-hits-interpolation

- **Rule:** An anti-pattern grep `systemctl.*status` will match a `Display` impl that interpolates `{status}` (the child's exit status), not the banned `systemctl status` verb. Pair with `git diff` added lines and a tighter pattern (`"systemctl".*"status"|systemctl --user status`).
- **Why:** AILAB-178 §5 grep 2 was red on a pre-existing byte-identical line: `"systemctl --user {args} failed ({status}): {}`". No added line in the 178 diff matched. Same family as `rustdoc-trips-word-grep-that-meant-call-sites`.
- **How to apply:** Ban the argv (`status dreamd.service`, `systemctl --user status`), not the English word `status`. Confirm with `git diff -- path | grep '^+' | grep -E 'banned'` so a HEAD-identical Display string cannot fail a new ticket.
- **Cross-refs:** `rustdoc-trips-word-grep-that-meant-call-sites`, `spec-grep-file-wide-hits-the-tickets-own-fixtures`, `service-status-is-not-dreamd-status`

### v2-beats-linear-ac

- **Rule:** For dreamd-eng tickets that have `assignments/AILAB-*.v2.md`, implement the v2, not Linear AC as written.
- **Why:** AILAB-204 Linear still said `genai`, `[dream.llm]`, and env-only after the v2 spec and the ship. The implementing tree followed the v2 file. Next tickets (201, 196, 200) inherit those reconciliations.
- **How to apply:** If `assignments/AILAB-NNN.v2.md` exists, that file is the contract. Treat Linear bullets as historical. Do not re-introduce `[dream.llm]`, `tiktoken-rs` on 201, or env-only credentials.
- **Cross-refs:** `config-llm-keys-are-flat`, `linear-201-ac-is-pre-llm-module`

### linear-folded-ac-may-already-be-in-the-blocker

- **Rule:** When Linear folds a second story into a later ticket, grep the blocker before re-implementing the fold. The remaining ticket is only the unfinished half.
- **Why:** AILAB-199 Linear still asks for `~/.config/dreamd/secrets.toml` mode `0600` (founder B9, folded 2026-05-10). AILAB-204 already shipped `resolve_llm_credentials`, env-then-secrets, and `NO_API_KEY_FALLBACK`. Re-doing that half would churn `llm.rs` and fight the 204 CHANGELOG bullet.
- **How to apply:** Remaining 199 is `--share-personal` / `x-dreamd-share-personal` consent for `personal/` on the compose prompt. Do not add a second secrets reader. Same shape as `linear-todo-can-already-be-on-main`.
- **Cross-refs:** `linear-todo-can-already-be-on-main`, `v2-beats-linear-ac`

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
- **How to apply:** For a module-only gate use `cargo test -p dreamd-core --lib -- llm::`. Read the “filtered” count. Do not treat a 35-pass `--lib llm` paste as “35 tests in llm.rs.” The filter is a contiguous substring of the full path, so `cli::parses_blame` matches nothing: those tests are `cli::tests::parses_blame_*` (BZR-193). Use `cli::tests::parses_blame`.
- **Cross-refs:** `cargo-run-dreamd-needs-bin`

### linear-search-nodes-layer-filter-does-not-exist

- **Rule:** MCP `search_nodes` takes `query` + `k` only (`SearchNodesParams`). HTTP `GET /api/v1/recall` takes `q` + `k`. Both call `recall(..., None)`. Hits already carry `source` (`"episodic"` | `"semantic"` via `Layer::as_str`). There is no `layer` / `layer=` argument on either surface.
- **Why:** AILAB-191 Linear AC says “`layer=` filter narrows.” Documenting that would teach agents to pass a param rmcp will reject. Adding the param is a new feature, not “just better descriptions.”
- **How to apply:** Rewrite descriptions to tell agents to read `source`. Do not add `SearchNodesParams.layer`. Do not unify MCP `query` with HTTP `q`. `explain=1` on `GET /api/v1/recall` is the BZR-198 citation gate; it is not a layer filter, and `search_nodes` still does not take it. `index.rs` rustdoc still says every v0.1 doc is `Layer::Episodic` — stale after AILAB-205; do not “fix” it on a copy ticket.
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

### linear-project-is-dreamd-eng-on-botzr-research

- **Rule:** dreamd issue ids are `BZR-<n>` on team **Botzr-Research** (same numbers as the retired `AILAB-<n>` ids). Shipped work stays on project **dreamd-eng** (`P-BZR-1`). The unfinished spine is project **R&D** (`P-BZR-79`). There is no team named `Botzr-AI-Labs` or `dreamd-eng`. `list_issues(team: "dreamd-eng")`, `list_issues(team: "Botzr-AI-Labs")`, and `list_issues(project: "dreamd-eng", state: "backlog")` return empty.
- **Why:** 2026-08-27 look-ahead: querying team `dreamd-eng` found zero issues while the project still held them. 2026-09-24: the team list no longer contains `Botzr-AI-Labs`; the project moved to `Botzr-Research`. 2026-09-26: R&D was created and the open spine (827 through 157, plus 207, 210, 149, 152) was moved onto it. dreamd-eng then has only Done, Canceled, and Duplicate, and its milestone bars read 100% because those issues left. The project description body still says RFC-011 is proposed; BZR-1075 accepted it the same day.
- **How to apply:** For the next ticket, filter `project: R&D` (id `362958e6-8f75-4493-830a-21f3f8a1ba79` — the name `R&D` matches more than one project). For history, filter `project: dreamd-eng`. Read `BZR-<n>` as the live id. An empty dreamd-eng backlog is the move, not a finished product. Build order is `remaining-build-order-2026-09-24`, not the August queue numbers, and not dreamd-eng milestone progress.
- **Cross-refs:** `linear-todo-can-already-be-on-main`, `remaining-build-order-2026-09-24`

### ailab-210-ac-is-pre-watch-architecture

- **Rule:** AILAB-210 Linear AC (MCP-client refcount, 30s/90s application heartbeats, PID file, idle-exit of a shared writer) does not describe the live process model. Do not implement it as written.
- **Why:** Live `dreamd mcp` is stdio (`Backend::Remote` HTTP-over-UDS or `Backend::Local` in-process). `dreamd watch` is the UDS HTTP daemon and exits only on SIGINT/SIGTERM (`AILAB-162` / `AILAB-748` `DRAIN_TIMEOUT` 30s). No PID file, no client registry, no heartbeat. Map idle 30 min is per-project index/supervisor eviction (`project_resource_map.rs`), not MCP clients. Cited `context/PRD.md` is gone. Linear itself demoted 210 off the v0.1 short list (2026-07-20) and again to Backlog (2026-08-22).
- **How to apply:** A v2 must remap onto UDS connection tracking on `watch` (scope change — ask) or skip until dogfood shows zombie writers. Do not add a PID file or MCP ping protocol to satisfy the May AC. Do not conflate 210 with `DRAIN_TIMEOUT`.
- **Cross-refs:** `ailab-162-drain-not-sigterm`, `linear-todo-can-already-be-on-main`

### ailab-163-excise-skips-not-suffix

- **Rule:** AILAB-163 quarantines **only** mid-file blank/malformed `\n`-terminated lines into `.corrupt-YYYY-MM-DD.jsonl` and rewrites the live JSONL to keep every well-formed record. Do not quarantine the suffix from the first anomaly through EOF. Torn no-`\n` tails still truncate in place. Implement `assignments/AILAB-163.v2.md`, not Linear’s silent-truncate story.
- **Why:** By `9ddd4b5`, WEG-378/`episodic::scan` already skips mid-file corruption and `recover` only `set_len`s torn tails (`recover_leaves_midfile_corruption_in_place`). Linear AC and the 2026-08-27 queue Q01 blurb still claim silent truncate. Suffix quarantine would regress “neither good lost.”
- **How to apply:** Change `recover` to take the JSONL path; in-place fd rewrite only (no `rewrite_atomic`/`rename` while the coordinator holds the fd). Sidecar is a sibling `.corrupt-<date>.jsonl`, same-day append, `warn!` on quarantine. Call site: `MemoryCoordinator::open_at`.
- **Cross-refs:** `weg378-skip-midfile-halt-torn-tail`, `jsonl-torn-tail-validation`, `v2-beats-linear-ac`, `linear-todo-can-already-be-on-main`

### wal-guarded-replace-owns-tmp-path

- **Rule:** Dream-cycle destructive replaces go through `wal::guarded_replace`. Callers never construct `WalIntent.temp_file_path`. The tmp path is the `&Path` `write_atomic_with_hook` passes into its hook after fsync, not a second `with_extension("tmp")` at the call site.
- **Why:** AILAB-164 Linear still cited `consolidation.rs:226` and “three state.json getters.” Live pin/decay thread a closure through `rewrite_atomic` and recompute the tmp path; `write_selected_lesson` appends `ReplaceSemanticMemory` *before* `write_lessons_file` (intent can land with no tmp). AILAB-167 already owns the typed `state.json` writer; only `read_last_cycle_status` / `read_cycle_started_at` remain as `Value` parsers.
- **How to apply:** Implement `assignments/AILAB-164.v2.md`. `apply_pin_unpin` / `run_decay_pruner` use `episodic::rewrite_guarded` (Prune). `write_selected_lesson` uses `guarded_replace` + `render_lessons_file` (Replace), not `write_lessons_file`. `archive.rs` keeps hook-free `rewrite_atomic`. `recover` must not call this seam (AILAB-163 open fd). Readers wrap `read_daemon_state`; do not add a third writer. Recovery tests drive `guarded_replace_crash_after_intent`, not hand-written WAL JSON.
- **Cross-refs:** `daemon-state-module-avoids-wal-autobiography-cycle`, `init-is-a-create-only-statejson-writer`, `ailab-163-excise-skips-not-suffix`, `v2-beats-linear-ac`, `linear-folded-ac-may-already-be-in-the-blocker`

### tracelayer-default-span-is-debug

- **Rule:** `TraceLayer::new_for_http()` without a custom MakeSpan emits DEBUG spans. `DREAMD_LOG` defaults to `info` (`observability.rs`), so those spans never appear. HTTP request spans must be `info_span!` (AILAB-189).
- **Why:** Linear AC says `TraceLayer::new_for_http()` as if that were the whole job. The lockfile `tower-http 0.6.11` from reqwest does not even enable the `trace` feature. Peer UID lives on `Extension(PeerUid)` injected in `uds_server.rs`, not in `build_router`. Last `.layer()` is outermost — `SetRequestIdLayer` must sit outside TraceLayer or MakeSpan sees no `x-request-id`.
- **How to apply:** Implement `assignments/AILAB-189.v2.md`. Direct `tower-http` with `default-features = false, features = ["trace", "request-id"]`. Do not change `init_tracing` or coordinator log levels. Measure stripped `dreamd` after `build_router` actually calls TraceLayer (`fat-lto-dead-strips-unreferenced-tokenizer`, `nfr-2-stripped-binary-is-20mb`). Stop if over 20 MB. An `info_span!` is not a log line — see `info-span-alone-logs-nothing-fmtspan-none`. `SetRequestIdLayer` must sit outside both `PropagateRequestIdLayer` and TraceLayer (`set-request-id-must-be-outside-propagate`).
- **Cross-refs:** `nfr-2-stripped-binary-is-20mb`, `fat-lto-dead-strips-unreferenced-tokenizer`, `v2-beats-linear-ac`, `ailab-210-ac-is-pre-watch-architecture`, `info-span-alone-logs-nothing-fmtspan-none`, `set-request-id-must-be-outside-propagate`

### info-span-alone-logs-nothing-fmtspan-none

- **Rule:** `observability::init_tracing` uses `fmt::layer()` with the default `FmtSpan::NONE`. A span, even INFO, writes nothing until an *event* fires inside it. HTTP tracing therefore records onto the span *and* emits one `info!(parent: span, "http request")`. Do not treat "do not emit a second INFO event" as "do not emit any INFO event."
- **Why:** AILAB-189 v2 §3 banned a second INFO line to stop `DefaultOnResponse` at INFO duplicating status/latency under different names. Literal reading would ship a correct INFO span that is silent under the shipping subscriber. Shipped (`968e67b`): one event into the span, fields on the span, not `DefaultOnResponse`.
- **How to apply:** Do not flip `with_span_events` to `NEW|CLOSE` (two lines per request). Do not promote `on_request` to INFO (arrival + completion = two INFO lines). Leave `DefaultOnFailure` at ERROR for 5xx.
- **Cross-refs:** `tracelayer-default-span-is-debug`

### set-request-id-must-be-outside-propagate

- **Rule:** Axum last `.layer()` is outermost. `SetRequestIdLayer` must sit **outside** both `PropagateRequestIdLayer` and `TraceLayer`. `PropagateRequestId::call` stashes `x-request-id` off the request it is handed; if it runs before the generator, the response has no header and MakeSpan records `"-"`.
- **Why:** AILAB-189 v2 §3 snippet ordered Trace → SetRequestId → Propagate (Propagate outermost). That compiles and looks layered, but oneshot would see no `x-request-id`. Live (`968e67b`): Trace → Propagate → SetRequestId → Unix `peer_uid`.
- **How to apply:** Inbound on Unix is `peer_uid` → `SetRequestId` → `Propagate` → `Trace` → `agent_root`. Do not "fix" it to match `ServiceBuilder` inner-first examples without flipping for axum.
- **Cross-refs:** `tracelayer-default-span-is-debug`

### macos-rss-is-ps-rss-not-phys-footprint

- **Rule:** AILAB-176 measures macOS idle daemon memory with `ps -o rss=` (KiB). That is not `phys_footprint`. The CI job is informational: no macOS limit, not in `notify-failure`. Linux `idle-rss-gate` stays VmRSS < 30 MB.
- **Why:** Linear AILAB-176 title said “< 30 MB on macOS” and the preamble equated `phys_footprint` with `ps -o rss=`. Those are different Mach metrics; a 30 MB fail on `macos-latest` would be an invented gate. Founder lock at queue time: informational job, `ps -o rss=` only, threshold later.
- **How to apply:** Darwin branch in `scripts/idle-rss.sh` (reuse spawn/socket/cleanup; never `/proc`, never `LIMIT_MB` compare). New job `idle-rss-report-macos` copies `size-report-macos`. `PERF.md` macOS measured cell is pending first CI run — do not guess a number. Do not sample `task_info` / `memory_pressure` to “be more accurate.”
- **Cross-refs:** `nfr-2-stripped-binary-is-20mb`, `v2-beats-linear-ac`

### systemd-user-unit-is-foreground-watch

- **Rule:** `dreamd service install` writes a systemd *user* unit that `ExecStart`s foreground `dreamd watch` (`Type=simple`). Do not call `detach_double_fork`. `WorkingDirectory=` is the `AgentRoot` project root discovered from cwd at install time.
- **Why:** ARCHITECTURE.md §8.1 said the double-fork helper was “reserved for service install.” Under systemd that helper reparents the daemon so the unit tracks the wrong PID. `run_watch` also requires `AgentRoot::discover(cwd)` (`watch.rs:67–70`); a unit without WorkingDirectory exits 2. Founder lock at AILAB-190 queue: install-time project root, not `$HOME`, not first `registry.toml` entry.
- **How to apply:** Nested clap like `reset` (`ServiceArgs` / `ServiceCommand::{Install,Start,Restart,Status,Uninstall}`). Probe `/run/systemd/system`; refuse with “run `dreamd watch`” otherwise. No `Environment=HOME=` (AILAB-584). `wants_daemon_log` stays Watch-only. LaunchAgent is AILAB-169 (same module, injected `launchctl`, same WorkingDirectory lock). `service status` is AILAB-178; `service restart` is AILAB-185 (bounce-only); `service uninstall` is AILAB-202 (unit/plist only; `--purge` is daemon home, not per-project stores). Tests must not require a live `systemctl --user`.
- **Cross-refs:** `no-hoisted-stdio-lock-across-tantivy`, `cargo-run-dreamd-needs-bin`, `v2-beats-linear-ac`, `ailab-210-ac-is-pre-watch-architecture`, `launchd-supervises-foreground-watch`, `service-status-is-not-dreamd-status`, `service-restart-is-not-update-restart`, `service-uninstall-is-not-dreamd-uninstall`

### service-status-is-not-dreamd-status

- **Rule:** `dreamd service status` is the OS supervisor (systemd `--user` / launchd). `dreamd status` is UDS daemon liveness + project + last dream cycle. Do not merge them. Do not call `systemctl status` (pager); query `systemctl show --no-pager -p …`.
- **Why:** Linear AILAB-178 AC said `systemctl --user status` and “last 10 log lines” as if `dreamd status` did not exist. WEG-103 already tails 5 lines of `~/.agent/dreamd.log` without truncating it. The two commands can disagree (foreground `watch` live, no unit; unit running, socket dead).
- **How to apply:** Nested `ServiceCommand::Status`. Log tail via `read_log_tail_n(path, 10)`; leave `LOG_TAIL_LINES = 5`. Four states: running / stopped / failed / not-installed. Exit 0 on a successful query including stopped. Inject a stdout-returning runner; never spawn live `systemctl` / `launchctl` in tests.
- **Cross-refs:** `systemd-user-unit-is-foreground-watch`, `launchd-supervises-foreground-watch`, `plist-must-not-redirect-dreamd-log`

### service-restart-is-not-update-restart

- **Rule:** `dreamd service restart` bounces the OS-supervised unit (`systemctl --user restart` / Darwin `kickstart -k`). It does not rewrite `ExecStart`. It is not `dreamd update --restart` (AILAB-552). Every `dreamd update` already SIGTERMs same-HOME `mcp`/`watch` (`--restart` only announces that stop); `Restart=on-failure` / `KeepAlive.SuccessfulExit=false` do not bring a clean SIGTERM back. Darwin `service start` already is `kickstart -k`; restart reuses that argv and prints `restarted`.
- **Why:** Linear AILAB-185 AC named the Darwin kickstart line as if it were new, and the upgrade story is easy to collapse into `update --restart`. A first draft of 185 docs claimed update “leaves the supervised unit alone”; false — the unit inherits `HOME` (AILAB-584 forbids `Environment=HOME=`), so the stop pass matches it. An npx cache-path change still needs `service install` first.
- **How to apply:** Nested `ServiceCommand::Restart`. Injected runners like `start`. Missing unit = same as `start` (supervisor fail, exit 1). `NoSystemd` stays exit 2. Docs: README one-liner plus `docs/install.md`; `update` stops, `service restart` brings the unit back.
- **Cross-refs:** `systemd-user-unit-is-foreground-watch`, `launchd-supervises-foreground-watch`, `service-status-is-not-dreamd-status`, `npm-shim-must-list-new-subcommands`

### service-uninstall-is-not-dreamd-uninstall

- **Rule:** `dreamd service uninstall` removes the systemd user unit / LaunchAgent only. `dreamd uninstall` (AILAB-226) stops processes and clears caches. `--purge` deletes daemon home `~/.agent/` after `--yes` or typed `y` (`reset workspace` idiom), never per-project `.agent/`.
- **Why:** Linear AILAB-202 AC named `--purge` + typed-Y and cited gone PRD/plan1.md. A merge into `Command::Uninstall` would pkill and cache-wipe. Store wipe of project `.agent/` is still the manual troubleshooting “Full fresh store”.
- **How to apply:** Nested `ServiceCommand::Uninstall { purge, yes }`. Injected runners. Idempotent if the unit/plist is already gone. Confirm `--purge` before any mutation. Do not call `commands::uninstall::run` or `lifecycle_cleanup`. Daemon home is `home.join(".agent")`, never `resolve_daemon_home()` — that helper's HOME-unset fallback is a relative `.agent` under cwd (a project store). Confirmation copy lives in `ServiceError` Display; `service_exit` prints it once — no local `eprintln!` in `confirm_purge`. Darwin `bootout` is a mutation: a `--purge` test must pass `purge: true` on `run_launchd_uninstall_with` or confirm-before-bootout is unpinned.
- **Cross-refs:** `systemd-user-unit-is-foreground-watch`, `service-restart-is-not-update-restart`, `npm-shim-must-list-new-subcommands`

### windows-compile-is-not-the-port

- **Rule:** AILAB-174 makes the crate compile on `windows-latest`. It does not ungate `pub mod server` / `client` / `daemon_client`, does not implement `write_atomic` on Windows, and does not drop `continue-on-error` on the Windows test job. AILAB-203 is the Windows schtasks logon task plus the `auth.json` token, not the port; **AILAB-192** (TCP + localhost + bearer) is the daemon/API port.
- **Why:** Linear AILAB-174 AC offered Option A (compile) or Option B (intentional red). Option B already shipped (`continue-on-error` on `windows-latest`). The remaining holes are ungated `crate::server` / `daemon_client` imports in `mcp/mod.rs` plus CLI `watch` / `archive` / `cli.rs` `client::resolve_daemon_socket` sites — not “mcp only,” and not a reason to compile `Supervisor` on Windows. A half-written `#[cfg(not(unix))] Supervisor::start` branch never typechecks.
- **How to apply:** `#[cfg(unix)]` those imports. Split `run_mcp_server`; Windows returns `McpRunError::Unsupported` (exit 2). Copy `run_doctor`'s socket idiom (`cli.rs` unix `resolve_daemon_socket` / not-unix `None`). If a test fails on Windows because it calls a unix-only API, `#[cfg(unix)]` the test. That advice is right for `tests/*.rs` (rustc synthesizes `main`) and **wrong** for `tests/bin/*.rs`: `cargo test --workspace` still builds every `[[bin]]`, and a crate-level `#![cfg(unix)]` strips `fn main` (`E0601`). Gate items individually and give `main` a `not(unix)` twin. Do not add `windows-sys`.
- **Cross-refs:** `linear-todo-can-already-be-on-main`, `nfr-2-stripped-binary-is-20mb`, `v2-beats-linear-ac`

### launchd-supervises-foreground-watch

- **Rule:** macOS `dreamd service install` writes `~/Library/LaunchAgents/dev.dreamd.dreamd.plist` that `ProgramArguments`s foreground `dreamd watch`. Do not call `detach_double_fork`. Do not set `AbandonProcessGroup`. `WorkingDirectory` is the install-time `AgentRoot` project root (same founder lock as the systemd unit).
- **Why:** Linear AILAB-169 AC assumed a new command and `docs/install.md` (file was missing). Live clap is already `service install`/`start` from AILAB-190. Launchd tracking a double-forked grandchild is the same PID lie as systemd `Type=simple` plus `detach_double_fork`.
- **How to apply:** Inject `launchctl` like `systemctl`; inject `uid` (production `id -u`, no `unsafe` / no CLI `nix` dep). `bootstrap gui/<uid> <plist>` on install; `--force` is the overwrite confirmation (no stdin) and does `bootout` then `bootstrap`; `start` is `kickstart -k gui/<uid>/dev.dreamd.dreamd`. Linux `NoSystemd` copy must still not contain `LaunchAgent`. Darwin-path tests must run on Linux CI via `run_*_with`, not only `cfg(macos)`.
- **Cross-refs:** `systemd-user-unit-is-foreground-watch`, `plist-must-not-redirect-dreamd-log`, `npm-shim-must-list-new-subcommands`

### plist-must-not-redirect-dreamd-log

- **Rule:** The LaunchAgent plist omits `StandardOutPath` / `StandardErrorPath`. `~/.agent/dreamd.log` is owned by `watch`’s tracing file layer (`wants_daemon_log` is Watch-only; `init_tracing` truncates).
- **Why:** Linear AILAB-169 said “logs to `~/.agent/dreamd.log`.” That is already true. Pointing launchd at the same file gives two writers and a truncate race with AILAB-184.
- **How to apply:** `KeepAlive` / `SuccessfulExit=false` analogue of `Restart=on-failure`. Never `EnvironmentVariables` / empty `HOME`. One-shots stay console-only.
- **Cross-refs:** `launchd-supervises-foreground-watch`, `systemd-user-unit-is-foreground-watch`

### npm-shim-must-list-new-subcommands

- **Rule:** Every new `dreamd` clap subcommand that users might type after `npx -y dreamd-mcp` must be added to `DREAMD_SUBCOMMANDS` in `packages/dreamd-mcp/bin/dreamd-mcp.js`. Unlisted tokens become `dreamd mcp <token>`.
- **Why:** AILAB-190 shipped `service` on the binary and left the shim set unchanged, so `npx -y dreamd-mcp service install` silently starts MCP. The file comment already says keep in sync with `Command`.
- **How to apply:** Add the token + a `packages/dreamd-mcp/test/route.test.js` case. Docs that show the npx form are a lie until the set includes it.
- **Cross-refs:** `cargo-run-dreamd-needs-bin`, `launchd-supervises-foreground-watch`

### windows-service-is-schtasks-foreground-watch

- **Rule:** Windows `dreamd service install` writes a logon-triggered scheduled task whose action is foreground `dreamd watch` (`WorkingDirectory` = install-time `AgentRoot`) and mints `~/.agent/auth.json` (256-bit token, owner-only ACL via `icacls`). It does not `CreateProcess` detach, does not call `detach_double_fork`, does not write a PID file, and does not ungate `pub mod server`. Token rotation on `--force` / reinstall; AILAB-192 re-reads the file per request and never rotates at watch start. `watch` boots on Windows as of AILAB-192 (loopback TCP + bearer) and exits 1 when `auth.json` is missing.
- **Why:** Linear AILAB-203 AC named `DETACHED_PROCESS`, heartbeat/refcount (AILAB-210’s model), PID stale-lock, CI-gating, 15 MB / 30 MB Windows gates. Live Unix is systemd/launchd supervising foreground `watch`. `write_atomic` is still `Unsupported` on Windows, so the daemon cannot run.
- **How to apply:** `ServiceBackend::Schtasks`; XML + `schtasks /Create /XML`; inject `schtasks` / `icacls` / SID like `launchctl` / `id -u`. Tests on Linux via `run_schtasks_*_with`. Do not add `windows-sys`. Do not `/Run` at install. Do not implement TCP/bearer here (AILAB-192).
- **Cross-refs:** `windows-compile-is-not-the-port`, `launchd-supervises-foreground-watch`, `systemd-user-unit-is-foreground-watch`, `ailab-210-ac-is-pre-watch-architecture`, `nfr-2-stripped-binary-is-20mb`, `v2-beats-linear-ac`

### windows-api-is-tcp-localhost-bearer

- **Rule:** Windows `dreamd watch` binds `127.0.0.1:0`, writes `~/.agent/server.json`, and authenticates with `Authorization: Bearer` from `~/.agent/auth.json`. Unix stays UDS + `SO_PEERCRED`. `--bind` / `--insecure` (AILAB-197) gate this listener only; see `watch-insecure-is-bind-only`. `write_atomic` stays `Unsupported`.
- **Why:** Linear AILAB-192 folded DR-409 and a gating Windows smoke, and assumed a writer-process 174/203 still refused. Token mint is AILAB-203 install, not watch start.
- **How to apply:** Bearer outermost on the Windows router only. Skip `TantivyIndexHandle::open` on Windows. MCP Remote over TCP when `server.json` is live; no in-process Local. Tests bind `127.0.0.1` on Linux. Do not add `windows-sys` / `subtle`.
- **Cross-refs:** `windows-compile-is-not-the-port`, `windows-service-is-schtasks-foreground-watch`, `nfr-2-stripped-binary-is-20mb`, `v2-beats-linear-ac`

### watch-insecure-is-bind-only

- **Rule:** `dreamd watch --insecure` only lifts the non-loopback **bind** refuse. Unix UDS is unchanged (those flags exit 2). `read_server_json` still requires a loopback host. Bearer / `auth.json` still required. No DNS.
- **Why:** Linear AILAB-197 AC named `--bind` including UDS paths, DNS “resolves”, and `docs/security.md`. Live 192 is Windows `127.0.0.1:0` + a client that must not dial a forged routable `server.json`.
- **How to apply:** Gate on the requested `SocketAddr` before bind. Publish loopback in `server.json` even when listening on `0.0.0.0`. Streamable HTTP MCP (AILAB-206) is `dreamd mcp --bind`, not `watch`; see `mcp-streamable-http-is-not-watch-rest`.
- **Cross-refs:** `windows-api-is-tcp-localhost-bearer`, `v2-beats-linear-ac`, `changelog-historical-entries-stay-put`, `mcp-streamable-http-is-not-watch-rest`

### mcp-streamable-http-is-not-watch-rest

- **Rule:** Streamable HTTP MCP lives on `dreamd mcp --bind`, path `/mcp`. `dreamd watch` REST (`/api/v1`) stays a different server. Unix `watch --bind` still exits 2. No bearer on `/mcp`. No `server.json` for the MCP port. Default `dreamd mcp` is still stdio. The transport is behind the non-default `mcp-http` cargo feature (NFR-2: +~1.5 MB stripped); without it `--bind` exits 2 naming the feature.
- **Why:** Linear AILAB-206 read as a watch `--insecure` transport and a HITL harness smoke. Live MCP is stdio; live `--insecure` is the 197 TCP bind gate.
- **How to apply:** Reuse `bind_listen`. `--insecure` without `--bind` on mcp is exit 2. STOP if stripped binary > 20 MB. Do not add `mcp-http` to `default` features or to a release build without an explicit NFR-2 decision. Test HTTP with `--features mcp-http` (CI runs `--all-features`); a default-feature `mcp::http::` filter runs 0 tests.
- **Cross-refs:** `watch-insecure-is-bind-only`, `nfr-2-stripped-binary-is-20mb`, `windows-api-is-tcp-localhost-bearer`, `v2-beats-linear-ac`

### episodic-1-0-is-not-state-1-0

- **Rule:** Linear “no `schema_version` `1.0` anywhere” is a **won't-do**. Only episodic `AgentLearning` / JSONL events use `RECORD_SCHEMA_VERSION` `"1.0.0"`. Daemon `STATE_SCHEMA_VERSION`, WAL, and the recurrence sidecar stay `"1.0"`. `render_lessons_file` already matches live SPEC.md (YAML frontmatter + HTML-comment blocks); do not add `TODO(WEG-61)` or revert to `## <skill_action>` / `*Sources:*`.
- **Why:** AILAB-209 Linear AC (May 2026) treated `"1.0"` as a misspelled episodic token and assumed the dream-cycle LESSONS.md writer had not shipped. WEG-61 / AILAB-201 / AILAB-200 shipped. 209 stamped leftover episodic literals (`http/tests.rs` AgentLearning + raw JSONL seed + `dream-cycle-snapshot`) and added a rustdoc pointer.
- **How to apply:** Gate AgentLearning **and** raw JSONL seeds (`"schema_version":"1.0"` in `b"…"` / `r#"` fixtures) — an `AgentLearning {` grep misses `fs::write` of a JSONL line (`raw-jsonl-seed-is-not-agentlearning-struct`). Keep the `SkillAction` “legacy dotted keys serde” sentence. Do not register a migrate `"1.0"`→`"1.0.0"`. Skip 210. 208 is in-repo `docs/spec/` (`spec-page-is-in-repo-not-dreamd-dev`), not a `dreamd.dev` deploy.
- **Cross-refs:** `migrate-from-to-is-record-schema`, `v2-beats-linear-ac`, `linear-todo-can-already-be-on-main`, `raw-jsonl-seed-is-not-agentlearning-struct`

### raw-jsonl-seed-is-not-agentlearning-struct

- **Rule:** An `AgentLearning {` grep does not see `fs::write` of a JSONL byte string. Search `"schema_version":"1.0"` in `b"…"` / `r#"` / `.jsonl` fixtures too.
- **Why:** AILAB-209 §1.2 listed two episodic `"1.0"` sites. A third was `http/tests.rs:1397` (`dream_happy_path_returns_200`). Grep 1 stayed green with that seed still on `"1.0"`.
- **How to apply:** Pair the struct grep with `rg '"schema_version":"1.0"' --glob '*.rs' --glob '*.jsonl'` and classify each hit as episodic vs state/WAL/sidecar before changing it.
- **Cross-refs:** `episodic-1-0-is-not-state-1-0`, `spec-grep-file-wide-hits-the-tickets-own-fixtures`

### spec-grep-rg-e-is-encoding

- **Rule:** ripgrep’s `-E` is `--encoding`, not grep’s extended-regex. `rg -viE "$FILT"` exits 2; a leading `!` turns that into a pass.
- **Why:** AILAB-209 §5 grep 7. The same line also matches `/` in `///` markers and `crates/…` paths, so even `grep -viE` over full `rg -n` lines is not a skill_action check.
- **How to apply:** Use `grep -viE` or `rg -vi`. For rustdoc, strip the `///` prefix and path before looking for `/`. Never `|| true` on an anti-pattern grep (`! cmd || true` cannot fail).
- **Cross-refs:** `rustdoc-trips-word-grep-that-meant-call-sites`, `guards-vs-mandates-same-line`

### spec-page-is-in-repo-not-dreamd-dev

- **Rule:** AILAB-208 publishes two digest pages under `docs/spec/`. Root `SPEC.md` stays the canonical on-disk contract. Do not register `dreamd.dev` (AILAB-272 is Canceled). Do not wait on ANTH-143 (the issue id is gone). Do not move `SPEC.md`.
- **Why:** Linear AC named `dreamd.dev/spec` and a best-citizen spike blocker. The 2026-07-20 status on the same issue already offered in-repo `docs/spec/` + a README link as the v0.1 cheap signal. Remaining-50 still queued it as Q17.
- **How to apply:** Implement `assignments/AILAB-208.v2.md`. Page 1 = folder layout + per-project `.agent/` vs per-user `~/.agent/` daemon home. Page 2 = JSONL / LESSONS.md frontmatter (MAY `citations`) / promotion / redaction. `skills/` and `protocols/` are dreamd extras, not SPEC-required. Skip 210. Q18 is AILAB-180. 208 shipped on `main` at `7cc9418` while Linear is still Backlog — do not re-queue it.
- **Cross-refs:** `v2-beats-linear-ac`, `doc-first-append-via-uds-learn`, `episodic-1-0-is-not-state-1-0`, `npm-dreamd-mcp-unscoped`, `linear-todo-can-already-be-on-main`

### salience-14-is-efolding-not-half-life

- **Rule:** The recency term is `exp(-age_days / 14)`, a 14-day **e-folding** time constant (`1/e` at 14 days). It is not a half-life. The half-life of that curve is `14 * ln(2) ≈ 9.7` days.
- **Why:** AILAB-180 Linear AC (May 2026) asked for “exponential decay over 14-day half-life.” `docs/architecture.md` Decay already names this a factual error. Teaching “half-life” in `docs/salience.md` would contradict the architecture notes and `--explain` (`exp(-age_days/14)`).
- **How to apply:**
  - Restate the formula from `salience.rs:87-93` / ARCHITECTURE.md decision #2.
  - Say “e-folding” (or “time constant”) and explicitly “not a half-life.”
  - Do not change the `14.0` literal; it is a product lock, not a fitted parameter and not Anderson’s `d`.
- **Cross-refs:** `actr-park-are-lineage-not-the-formula`, `v2-beats-linear-ac`

### actr-park-are-lineage-not-the-formula

- **Rule:** Anderson 2007 (ACT-R) and Park et al. 2023 (Generative Agents) are **lineage** for the salience *shape*. They are not the formula. ACT-R base-level activation is a power law (`t^{-d}`); Park retrieval is a weighted **sum** of recency + importance + relevance. dreamd is a **product** `BM25 × exp(-age/14) × (pain/10) × (importance/10) × (1 + ln(1 + recurrence))`.
- **Why:** SPEC.md footnote ¹ already cites both. AILAB-180’s “defensible answer for engineers reading critically” fails if the page claims isomorphism. Linear also wrote `ln(1+recurrence)` without the leading `1 +`, which would zero a first occurrence.
- **How to apply:**
  - Cite Anderson, *How Can the Human Mind Occur in the Physical Universe?* (2007) and Park et al., arXiv `2304.03442`.
  - Contrast the arithmetic in the same sections that cite them.
  - Keep `1 + ln(1 + recurrence)` so recurrence `0` → factor `1.0`.
- **Cross-refs:** `salience-14-is-efolding-not-half-life`, `layer-semantic-is-not-embeddings`

### releasing-npm-is-human-passkey

- **Rule:** `npm publish` for `dreamd-mcp` is human-only (`dataprime1` passkey / `npm login --auth-type=web`). CI must not publish, hold `NPM_TOKEN`, request `id-token`, or run `--provenance` / `dist-tag`. AILAB-179 is the **manifest write-back PR** only.
- **Why:** Linear AILAB-179 asked to automate npm publish. `RELEASING.md` (post-0.1.1) forbids it: passkey 2FA cannot run in Actions. MCP Registry publish is the same posture. Founder remap 2026-09-18: keep step 6 human; have `release.yml` open a `chore/mcp-manifest-v*` PR after regenerating `manifest.json`.
- **How to apply:**
  - Generator stays `scripts/update-mcp-manifest.sh` (extracted-binary sha, three Unix shim platforms).
  - Write-back is `gh pr create` to `main` with `GITHUB_TOKEN`; never `git push origin main`.
  - Bot commit DCO trailer must be column 0 (`ci.yml` `^Signed-off-by:`). YAML-indented heredocs fail that. Prefer `git commit -m "subject" -m "Signed-off-by: …"`.
  - `GITHUB_TOKEN` PRs do not trigger workflows. Do not invent `RELEASE_PR_TOKEN` without a founder ask.
- **Cross-refs:** `npm-dreamd-mcp-unscoped`, `v2-beats-linear-ac`, `changelog-historical-entries-stay-put`

### remaining-build-order-2026-09-24

- **Rule:** The finish order is the section `Remaining build order — 2026-09-24` in this file. Do not implement from the August 27 remaining-50 queue numbers, and do not treat a Linear **Backlog** status as proof the ticket is unshipped.
- **Why:** On 2026-09-24 the queue doc still named Q09 (`service restart`) as next, while `main` at `7d98716` had already landed Q09–Q19. Q20 **BZR-187** was the first ticket with no route in the tree. Parallel Claude and DeepSeek sessions will otherwise each pick a different "next" ticket and edit the same hot path.
- **How to apply:** Claim one ticket from that section. One writer per file. 187 through 146, including the BZR-156 command, are on `main` at or before `13ae1ca`. `provenance::verify` is on `main` at `e3b7ecf`. `dreamd doctor --provenance` is `ed134e1` (BZR-148). The finish list through BZR-157 is on `origin/main`. The stripped default binary on `f786bff` is 20,674,976 bytes. Tag `v1.0.0` is `dacd95d`; `origin/main` is `0064355`. There is no next implement ticket. Do not cut another tag or publish npm from an agent session. 207, 210, 149, and 152 are not claimable implement tickets. dreamd-eng milestone bars at 100% do not mean those tickets shipped. Leftovers after the 1.0.0 docs pass have no tickets; the list is `context/audits/v1.0.0-docs-audit-remaining-2026-10-02.md`.
- **Cross-refs:** `linear-todo-can-already-be-on-main`, `linear-project-is-dreamd-eng-on-botzr-research`, `nfr-2-stripped-binary-is-20mb`, `vectors-feature-stays-out-of-the-default-binary`, `ailab-210-ac-is-pre-watch-architecture`

### vectors-feature-stays-out-of-the-default-binary

- **Rule:** The `vectors` feature stays off the default build. Stripped `dreamd` on `29ffdba` / `40ff05a` is 20,670,912 bytes. After BZR-181 (`f786bff`), the default stripped binary is 20,674,976 bytes. The hard gate is 20,971,520. A default-build caller of `fastembed` fails that gate.
- **Why:** BZR-188's Linear AC asks for `dreamd vectors enable`, a model download, and a JSONL index. CI on `ed134e1` measured 20,615,600 bytes. `rustc 1.95.0` on `29ffdba` measured 20,670,912 after `strip`. Headroom is 300,608 bytes before BZR-181 and 296,544 after it. The seam (`fastembed` 5.17.4) compiles only with `--features vectors`. `ort-sys` fetches a prebuilt ONNX Runtime while that feature builds, so `--all-features` CI needs network at compile time. The size-gate job builds default features and does not fetch it. Fat LTO drops an unreferenced crate, so a strings count of zero is proof only together with a default-features build (no `fastembed` / `ort-sys` fingerprints under `target/release`). BZR-181's help text names `BAAI/bge-small-en-v1.5`, so a `bge-small-en` strings probe matches the default binary even when the embedder is not linked. The feature-build strip measured 46,589,208 bytes and 2028 `onnxruntime` hits.
- **How to apply:** The seam is `assignments/BZR-188.v2.md`, on local `main` at `40ff05a`. `fastembed` stays optional. `TextEmbedding::try_new` is `download_model` in `vectors.rs`, compiled only with `--features vectors`, and no test calls it. The linkage probe on a default release binary is `fastembed|onnxruntime`, not `bge-small-en`. A spec grep of a directory without `-r` exits 2, and a leading `!` turns that into a pass. Re-run those greps with `-r`. An insta filter `vectors:\s*\S+` also eats the closing `)` on `VERSION_SHORT`; use `vectors:\s*(?:on|off)`.
- **Cross-refs:** `nfr-2-stripped-binary-is-20mb`, `fat-lto-dead-strips-unreferenced-tokenizer`, `layer-semantic-is-not-embeddings`, `remaining-build-order-2026-09-24`

### in-process-dream-now-honors-the-409-guard

- **Rule:** `dreamd dream` with no daemon goes through `run_guarded_cycle`, which calls `ensure_not_in_progress` before `begin_cycle`. A `state.json` left at `in_progress` refuses the CLI cycle. `dreamd watch` clears that only when `dream_in_progress.wal` is still on disk (`recover_if_needed` returns `Clean` when the WAL is absent and does not rewrite the status).
- **Why:** Before BZR-172 the in-process path never checked the guard, so `begin_cycle` overwrote a stale `in_progress`. The sequencer made the HTTP 409 rule apply to the CLI too.
- **How to apply:** Do not remove the guard from `run_in_process` to "restore" the overwrite. A stuck status with no WAL is an operator cleanup (`state.json`), not a watch boot.
- **Cross-refs:** `remaining-build-order-2026-09-24`

### home-dir-does-not-consult-the-password-database

- **Rule:** Daemon home comes from `layout::home_dir` / `resolve_home_from_vars`. Empty `HOME` is unset. Unix does not read `USERPROFILE`. There is no `dirs::home_dir()` fallback in `daemon_client.rs` or `server/watch.rs`. The Windows MCP arm in `mcp/mod.rs` still calls `dirs::home_dir()`.
- **Why:** BZR-168 unified on the CLI rule. `dirs::home_dir()` on Unix falls through to the passwd entry when `HOME` is unset, so watch and MCP can disagree under Git-Bash.
- **How to apply:** Do not put `dirs::home_dir()` back on the watch or daemon-client path. The Windows MCP call is still `mcp/mod.rs:649`. BZR-170 and BZR-183 left that file alone. A later ticket may switch it once `mcp/mod.rs` has a single writer.
- **Cross-refs:** `remaining-build-order-2026-09-24`

### replay-open-is-an-index-doc-writer

- **Rule:** `index_doc` edges are written in two places, both after `writer.commit()` and before `write_progress`: `commit_and_persist` for a live `Append` batch, and `TantivyIndexHandle::open` for the episodic ids in `to_index`. Startup replay does not call `commit_and_persist`.
- **Why:** BZR-155 spec item 7 said a failed ledger append is healed because the next startup replays the batch. `open` committed that replay and moved the watermark with no `append_edges` call, so those ids never got an edge. The in-process retry (keep the batch when `append_edges` fails) does not survive a process restart.
- **How to apply:** Any new commit path that updates `index_progress.json` for episodic ids appends `index_doc` edges first. Semantic `lsn_` docs are not edges. The append is idempotent on `(kind, from, to)`. `run_indexer` takes the watermark path and the ledger path as one `IndexerPersist` argument; a separate `PathBuf` pushes the signature to 8 and `cargo clippy -- -D warnings` fails at 7.
- **Cross-refs:** `indexer-shed-is-not-replay-healed`

### bisect-searches-snap-refs-not-a-parent-graph

- **Rule:** `dreamd memory bisect` searches `branches/refs/snap-*` by the timestamp on line 2 of the ref. There is no parent pointer on an object or a ref. A named branch is one point on that timeline, not a chain.
- **Why:** BZR-153's Linear text says "binary search across branch history" in the git sense. `write_ref` stores an object id and a UTC timestamp (`snapshot.rs`). Two refs with the same timestamp are not ordered by ancestry. Same-second autosnaps overwrite the ref name and leave the earlier object unnamed.
- **How to apply:** Resolve endpoints through `refs/<name>` or a 64-hex id that some ref already names. Require the good timestamp string to be strictly less than the bad one. Do not invent a parent field. State lives at `branches/bisect`, beside `HEAD`, so `list_branches` does not show it. `dreamd memory diff` is a separate command (`13ae1ca`); do not fold it into a bisect change.
- **Cross-refs:** `remaining-build-order-2026-09-24`

### provenance-verify-does-not-rewrite-the-ledger

- **Rule:** `provenance::verify` only reads. There is no stored Merkle root to compare against, and no `--repair`. An unknown `kind` is skipped. `embedding` is corrupt, not skipped. A JSONL id with no `index_doc` edge is not a missing edge.
- **Why:** BZR-148's Linear AC asks for hash mismatches against a root and for `--repair`. BZR-155 never writes the root. The format forbids rewriting ledger lines. The index watermark is not a contiguous prefix, so "every event has an index_doc" would flag healthy logs.
- **How to apply:** Recompute the root from the good lines and return it. Report orphans (`from` absent from the live JSONL), missing `lesson_citation` / `recurrence` edges, and corrupt line numbers. The doctor flag is its own ticket now that `cli.rs` is free: print this report, and do not rebuild the ledger under `--repair` or any other flag name.
- **Cross-refs:** `replay-open-is-an-index-doc-writer`, `remaining-build-order-2026-09-24`, `doctor-repair-is-the-index-rebuild`

### memory-diff-reads-and-does-not-check-out

- **Rule:** `dreamd memory diff` resolves refs and calls `diff_objects`. It does not call `checkout` and it does not look for the daemon socket. `--json` prints one line and returns before any `--unified` dump.
- **Why:** BZR-156. Checkout and bisect refuse while the socket exists because they replace the live log. Diff only reads objects. A lessons dump on the JSON path would break the one-object contract.
- **How to apply:** Resolve `name:<64-hex>` first (ref line 1 must equal that id; the error names both ids), then an existing ref, then a bare 64-hex id. `serde_json::to_string` plus one newline, then return. `FileChange` serializes `snake_case`. `diff_objects` itself stays free of `Serialize` logic beyond the derives.
- **Cross-refs:** `bisect-searches-snap-refs-not-a-parent-graph`, `remaining-build-order-2026-09-24`

### branch-demo-uses-an-absolute-binary-and-a-temp-home

- **Rule:** `scripts/branch-demo.sh` resolves `dreamd` on `PATH` to an absolute path before `cd`, and exports `HOME` to the temp directory for the script process.
- **Why:** `PATH="target/debug:$PATH"` is relative. After the script changes into its temp project, that entry no longer finds the binary. `dreamd init` writes `~/.agent/registry.toml`, and checkout refuses when `~/.agent/dreamd.sock` exists, so a demo that keeps the caller's `HOME` writes into the real registry and can be blocked by a running `dreamd watch`.
- **How to apply:** `command -v dreamd`, prefix `$PWD` when the path is relative, then call that binary. Set `HOME` to the temp home before any `dreamd` invocation. The exit trap removes the temp directory, including that home. Two `memory branch` calls with unchanged live files share one object id.
- **Cross-refs:** `remaining-build-order-2026-09-24`, `memory-diff-reads-and-does-not-check-out`

### doctor-repair-is-the-index-rebuild

- **Rule:** `dreamd doctor --repair` rebuilds the Tantivy cache and unlinks an orphaned socket. It does not rewrite `provenance/ledger.jsonl`. Ledger repair is not a flag.
- **Why:** BZR-148's Linear AC asks `--repair` to rebuild the ledger from current state. The flag already shipped as the index rebuild (`run_repair` in `commands/doctor.rs`). Pointing it at the ledger would rewrite lines the format forbids, on a command operators already run.
- **How to apply:** `dreamd doctor --provenance` (`assignments/BZR-148.cli.v2.md`) prints `provenance::verify` and fails the run on orphans, missing edges, and corrupt lines. `skipped_unknown_kind` is printed and does not fail the check. Do not call `append_edges` from doctor. Do not branch `run_repair` into the ledger. There is no stored Merkle root to mismatch against.
- **Cross-refs:** `provenance-verify-does-not-rewrite-the-ledger`, `remaining-build-order-2026-09-24`, `forget-cascade-does-not-run-the-dream-cycle`

### forget-cascade-does-not-run-the-dream-cycle

- **Rule:** `forget::cascade` removes one event from the live JSONL under the open dream-cycle WAL. It does not call `filesystem_phases`, `select_lesson_or_retire`, or `run_decay_pruner`.
- **Why:** BZR-158's Linear AC says to re-run the deterministic dream cycle and to hard-delete embeddings. The dream cycle also decays every other aged row, and `embedding` is not a kind this tree writes. A `kill -9` test is the wrong crash probe: `recover_if_needed` only deletes uncommitted temps.
- **How to apply:** Spec `assignments/BZR-158.v2.md`. Drop a non-exemplar citation, or unlink `LESSONS.md` when the forgotten id is `Lesson.id`. Recount with `run_cluster_engine`. Leave old ledger lines in place. The coordinator reopens the append fd on every path except a clean missing id, including a cascade error after the JSONL rename, then sends the existing `PruneDecayedEvents`, `ApplyRecurrenceSidecar`, and `IndexSemanticLessons` messages. `dreamd forget` is BZR-151 (`assignments/BZR-151.v2.md`); the proof file it writes is the receipt in `forget-proof-is-a-receipt-not-a-signed-membership-proof`.
- **Cross-refs:** `coordinator-not-mutex-file`, `delete-term-must-target-a-unique-field`, `provenance-verify-does-not-rewrite-the-ledger`, `select-lesson-or-retire-unlinks`, `remaining-build-order-2026-09-24`, `forget-proof-is-a-receipt-not-a-signed-membership-proof`

### forget-proof-is-a-receipt-not-a-signed-membership-proof

- **Rule:** `dreamd forget --proof` writes a `forget-receipt/1.0` file: event id, lesson effect, the root from `provenance::verify`, and the orphan edges whose `from` is that id. It does not write the signed membership proof in `docs/provenance.md`.
- **Why:** BZR-151's Linear AC asks for a Merkle path proving derivatives were removed, plus ed25519, a verifier crate, embeddings, and a second dream cycle. The format's proof shows an id is a leaf under the root (`docs/provenance.md` Proof object). After `forget::cascade` the id is still a leaf, because ledger lines are append-only and `verify` reports the old edges as orphans. No crate depends on ed25519, and the format leaves key storage unspecified. A membership path would show the opposite of removal.
- **How to apply:** Spec `assignments/BZR-151.v2.md`. Call `forget::cascade`. Refuse a live daemon the way `dreamd archive` does. `--dry-run` writes nothing and does not probe. Refresh an existing index with the coordinator's three messages; do not create `.agent/.dreamd/index` when it is absent. Do not add a `signature` or `path` key to the receipt. Do not add an HTTP route. An id that is already absent prints `removed=false` and exits 0.
- **Cross-refs:** `forget-cascade-does-not-run-the-dream-cycle`, `provenance-verify-does-not-rewrite-the-ledger`, `nfr-2-stripped-binary-is-20mb`, `remaining-build-order-2026-09-24`

### public-benchmark-is-the-in-ram-recall-bench

- **Rule:** BZR-177's public harness is the existing `cargo bench -p dreamd-core --bench recall` (`crates/dreamd-core/benches/recall.rs`, `Index::create_in_ram`). `scripts/benchmark/recall_summary.py` prints sample P50/P99 from `target/criterion/recall/n/{1000,10000,100000}/new/sample.json`. Divide `times[i]` by `iters[i]`. Do not read `estimates.json`. Do not add `benches/public`, a CI bench job, a LongMemEval score, a vectors install, or a dreamd.dev page. The no-argument printer always reads `<repo>/target/criterion`. Criterion 0.5.1 writes `$CRITERION_HOME` if set, otherwise `$CARGO_TARGET_DIR/criterion`, otherwise the target dir from `cargo metadata`, otherwise `./target/criterion`.
- **Why:** Linear BZR-177 asks for `benches/public/`, LongMemEval mini scores (BZR-270), a vectors install, and CI publish to dreamd.dev. DR-208 already shipped the bench. The index is in RAM, so a disk-speed note is false. Criterion 0.5 stores iteration totals in `sample.json`, and its `median.point_estimate` is a bootstrap, not the sample percentile. Python `round` is banker's rounding; the index is `int((p / 100) * (n - 1) + 0.5)`. dreamd.dev is canceled. WasTrue (`scripts/benchmark/wastrue_bench.py`) is a different eval.
- **How to apply:** Spec `assignments/BZR-177.v2.md`. Leave `benches/recall.rs` alone. Leave the PERF.md measured cells and the 2026-07-28 / `e8e27fd` stamp. Do not rewrite the µs table in `docs/architecture/tantivy-migration.md`. The full 100k bench is not a report-back gate; `cargo check -p dreamd-core --benches` is. A grep that bans the token `LongMemEval` also bans a sentence saying the page omits that eval. The shipped `docs/benchmarks.md` names vector recall and WasTrue and omits the token. The printer parses `sys.argv` (`json`, `pathlib`, `subprocess`, `sys`). Do not send either back.
- **Cross-refs:** `remaining-build-order-2026-09-24`, `vectors-feature-stays-out-of-the-default-binary`, `spec-page-is-in-repo-not-dreamd-dev`, `layer-semantic-is-not-embeddings`, `changelog-historical-entries-stay-put`

### alpha-tag-is-the-version-bump-not-the-publish

- **Rule:** BZR-157 prepares `0.2.0-alpha.1` on the six `RELEASING.md` surfaces plus a changelog section. The tag, the draft GitHub release, npm publish, and the MCP registry stay the human steps in `RELEASING.md`. Published binaries stay the default build (`cargo build --release`). `dreamd vectors enable` exits 2 on them. Recall stays BM25 × salience. `rrf::fuse` stays uncalled. The bump commit belongs on `release/v0.2.0-alpha.1`, because `PENDING_*` shas fail `manifest.test.js` on `main`.
- **Why:** Linear BZR-157 asks for a vectors-feature release asset, hybrid retrieval enabled, a one-paste installer check, and the BZR-271 blog post in the same tag. The feature build is 46,589,208 bytes. `release.yml` fails NFR-2 above 20,971,520 and builds the default binary only. A hyphenated tag is already a GitHub prerelease. npm publish is the `dataprime1` passkey. Hybrid recall is not in the tree.
- **How to apply:** The 0.2.0-alpha.1 bump is `2e34181` and was never tagged. Manifest shas on `main` are the v1.0.0 release shas (`42e8e62`). Do not `git tag`, `git push`, or `npm publish` from an agent session. A bare path-dep `version = "0.1.0-rc.2"` is `^0.1.0-rc.2` and rejects `0.2.0-alpha.1`. The three fields in `crates/dreamd-core/Cargo.toml` and `crates/dreamd-cli/Cargo.toml` move with the workspace version. `RELEASING.md` step 1 row 7 names them.
- **Cross-refs:** `remaining-build-order-2026-09-24`, `releasing-npm-is-human-passkey`, `nfr-2-stripped-binary-is-20mb`, `vectors-feature-stays-out-of-the-default-binary`, `changelog-historical-entries-stay-put`, `v1-is-the-version-bump-of-the-current-tree`

### v1-is-the-version-bump-of-the-current-tree

- **Rule:** Product 1.0.0 is the version bump in `assignments/v1.0.0.v2.md`. `SPEC.md` stays v0.1. Episodic `schema_version` stays `"1.0.0"`. Recall stays BM25 × salience. The tag, the draft GitHub release, npm publish, and the MCP registry stay the human steps in `RELEASING.md`.
- **Why:** On 2026-10-01 the finish list was empty and `context/planning/future-directions.md` still described identity, hypergraph, CRDT, and cryptographic deletion as later phases. The user chose to ship 1.0 from the tree on `main` (`2e34181`) rather than finish that document. There is no `v0.2.0-alpha.1` tag. `2e34181` has no Actions run. `release.yml` marks a tag as a prerelease only when the name contains `-`, so `v1.0.0` publishes as stable. The release notes are only the `## [1.0.0]` block.
- **How to apply:** The bump is `bb84721`. Tag `v1.0.0` is `dacd95d` (published 2026-10-02, stable). Manifest shas are filled (`42e8e62`). npm `dreamd-mcp` 1.0.0 is on `latest` and `next`. Do not queue the bump again. Do not implement Windows `write_atomic`, wire `rrf::fuse` into recall, or bump `SPEC.md`. Do not `git tag`, `git push`, or `npm publish` from an agent session.
- **Cross-refs:** `alpha-tag-is-the-version-bump-not-the-publish`, `changelog-historical-entries-stay-put`, `vectors-feature-stays-out-of-the-default-binary`, `remaining-build-order-2026-09-24`
