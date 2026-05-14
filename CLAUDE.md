# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project status

Pre-release. Sprint 1 of 6, near-end. Eight of nine originally-queued Sprint-1 tickets shipped (WEG-5, 9, 10, 17, 26, 28, 18, 20 as of 2026-05-11), plus WEG-7 (DR-103 JSONL append durability) shipped 2026-05-14; WEG-21 (UDS writer-process lifecycle) remains. Workspace version stays at `0.0.0` through v0.1 release-bump.

**2026-05-12 strategic additions (founder override).** Competitor-research synthesis drove 23 new Linear tickets (WEG-174–WEG-196, ~97 pts) plus description updates to WEG-89 + WEG-107. Public v0.1 wedge becomes Framing A (cross-harness memory portability); post-Q6 engineering wedge stays internal only. New differentiator angles slotted across v0.1.1 (memory observability), v0.2 backlog (branchable memory, cryptographic forgetting), and research backlog (MCP-for-memory spec, CRDT, compression-unified, hyperagent meta-memory). See "## 2026-05-12 strategic additions" section below; full ticket batch in `context/linear-batch-2026-05-12.md`; wedge text in `context/framing-a-rewrite.md`; comparison FAQ in `context/competitor-comparison.md`. Founder explicitly overrode the Q6/Q7 1:1.5 scope-discipline rule for this batch.

The intended end-state architecture (below) lives in `context/PRD.md` and `context/AGILE/plan1.md` — both gitignored and local-only. Treat those documents as the engineering ground truth; what's on disk is partial.

### What's currently shipped (read these to understand the running code)

- **Cargo workspace** with two crates so far: `crates/dreamd-core` (`layout` + `privacy` modules) and `crates/dreamd-cli` (binary name `dreamd`). Both depend on each other via path; do not invent alternate dep idioms.
- **`dreamd init`** — per-project `.agent/` scaffold with locked stdout (16 lines / 651 bytes first-run, 1 line / 63 bytes re-run), idempotent on `.agent/` directory existence, first-run privacy disclosure baked in. Golden tests at `crates/dreamd-cli/tests/init_golden.rs` (3 tests, byte-exact fixtures at `tests/fixtures/init.{golden,rerun.golden}.txt`).
- **`dreamd --version` and `dreamd version`** — structured 5-field version output (semver, 7-char SHA, build date, target triple, schema `1.0`) baked at compile time via `vergen-gitcl`. Byte-exact format locked (see WEG-18 AC). Output factored as `pub(crate) const VERSION_SHORT` + `pub(crate) fn render_long()` in `crates/dreamd-cli/src/commands/version.rs` so WEG-20 can snapshot-test directly without subprocess.
- **`docs/security.md`** — threat model + privacy disclosure with `#privacy-disclosure` anchor. Single canonical doc; absorbed the former `docs/privacy.md` per round-7 fold.
- **`docs/demo-corpus.md`** — DR-920 corpus-selection decision rule (≥30 events / ≥2 cluster-prefixes by end of Sprint 2 → dogfood corpus; else hand-authored per WEG-57), reserved provenance section, Sprint 2 retro check.
- **`crates/dreamd-core::privacy`** — `pub const DR413_DISCLOSURE` + `pub const PRIVACY_DISCLOSURE_LINK` with inline `#[cfg(test)]` invariant test (`disclosure_contains_link`).
- **`dreamd-protocol::EventId`** (WEG-7) — newtype around `String` with a private inner field; `parse(&str) -> Result<EventId, EventIdParseError>` validates `evt_` prefix + exactly 26 chars + uppercase Crockford base32 (excludes I/L/O/U). Custom `Serialize`/`Deserialize` re-run validation on the wire and on disk. `AgentLearning.id` is now `EventId`, not `String`. `dreamd-protocol` deps stay locked to `serde + chrono + serde_json` — ULID minting lives in `dreamd-core`, not protocol.
- **`MemoryCoordinator` full WEG-7 protocol** (`crates/dreamd-core/src/coordinator.rs`) — `open(&AgentRoot, rx)` / `open_at(jsonl_path, agent_root, rx)` constructors run malformed-tail-skip recovery, then seek to EOF for appends. `AppendLearning` message carries `client_dedup_key: Option<String>` and returns `Result<EventId, CoordinatorError>`. Write order: LRU lookup → mint ULID + overwrite `learning.id` → serialize → ensure `\n` → 4 KiB check → `write_all` → `sync_data` → LRU `put` (insert-after-sync, no cache poisoning). `pub const MAX_LEARNING_LINE_BYTES: usize = 4096`; oversized lines return `CoordinatorError::PayloadTooLarge { size, max }` (HTTP 413 mapping ready). Idempotency LRU is `LruCache<(PathBuf, String), EventId>` cap 1024, keyed by canonicalized AgentRoot path × dedup key, in-memory only (cleared on restart). `truncate_malformed_tail` walks forward, tracks last-good offset, `set_len + sync_data` on torn tails. 4 new tests in `coordinator::tests`; total core test count 23.
- **`docs/architecture/durability.md`** (WEG-7) — durability protocol document; gitignored on disk like the other `docs/` files.

### What's not yet built (the rest of v0.1)

- HTTP API surfaces (`POST /learn`, `GET /recall`, `POST /dream`, `POST /migrate`) and the axum server. `POST /learn` will wire `client_dedup_key` from `Idempotency-Key` (or similar) header into the coordinator message and map `CoordinatorError::PayloadTooLarge` to HTTP 413 (WEG-68).
- Tantivy indexing + custom salience `Collector`/`Scorer`.
- Dream-cycle pipeline (WAL, consolidation, LLM gate, `--no-llm` mode).
- MCP server / `npx dreamd-mcp` distribution (WEG-81 blocked by WEG-17, now unblocked).
- UDS socket binding + `SO_PEERCRED` middleware (DR-118, WEG-21).
- `dreamd doctor --cluster-health` (DR-107, WEG-50).

## What dreamd is

A local-first, single-binary daemon that provides a portable memory layer for AI coding agents. It exposes a standardized `.agent/` folder (`working/`, `episodic/`, `semantic/`, `personal/`) and a local HTTP API; an MCP server maps to that API so Claude Code, Cursor, OpenCode, etc. can share memory across harnesses. The file system is the source of truth — the daemon reads/writes plain files (markdown, JSONL) so users can edit them by hand.

## Commands

```
cargo build --workspace                  # build all crates
cargo run -p dreamd -- init              # scaffold .agent/ in cwd
cargo run -p dreamd -- version           # print version block
cargo run -p dreamd -- --version         # print version single-line

cargo test -p dreamd-core                # 1 test (privacy invariant)
cargo test -p dreamd --test init_golden  # 3 tests (init stdout golden fixtures)
cargo check --workspace                  # static check
cargo clippy --workspace
cargo fmt --all
```

Note: package name in the cli crate is `dreamd`, not `dreamd-cli` (see drift catalog). `cargo test -p dreamd-cli` errors with "package not found."

The agile plan (DR-005) calls for a `Justfile` or `cargo xtask` with `dev/test/bench/release/lint` targets; not yet present. CI matrix (DR-003) is planned for Linux/macOS/Windows but not yet wired.

## Target architecture (end-state)

Cargo workspace per DR-002:

- `dreamd-protocol` — shared serde types only; deps limited to `serde` + `chrono` (not yet created).
- `dreamd-core` — modules: `api` (axum), `io` (FS/WAL), `index` (tantivy), `dream` (consolidation pipeline), `vcs` (git2). Currently has `layout` + `privacy` only; rest is queued.
- `dreamd-store`, `dreamd-server`, `dreamd-cli` — `dreamd-cli` exists; the others may collapse into `dreamd-core` depending on how DR-114 lands. Defer the split decision until the actor topology ships.

State management is an actor model: a single `MemoryCoordinator` task owns mutable state; API handlers and the file watcher send intents over `tokio::sync::mpsc`. Do not introduce parallel writers to the JSONL or index — every mutation goes through the coordinator. (Not yet built; WEG-16.)

## Load-bearing engineering decisions (do not change without re-reading the PRD)

These are the decisions whose violation would silently break the system. They came out of an explicit pressure-test pass. Most are aspirational — they govern code that isn't written yet — but they're binding when that code lands.

1. **JSONL append durability.** All appends to `AGENT_LEARNINGS.jsonl` flow through one `MemoryCoordinator` actor; `&mut self` on the run loop is the exclusivity guarantee, not a `Mutex<File>` (there is no second handle to the file). Each write: idempotency-LRU lookup → mint `EventId` (`evt_` + 26-char Crockford ULID via the `ulid` crate in `dreamd-core` — `dreamd-protocol` stays free of the `ulid` dep and only owns the parse/validate boundary) → overwrite inbound `learning.id` → serialize → ensure trailing `\n` → 4 KiB hard reject (`MAX_LEARNING_LINE_BYTES = 4096`, returns `PayloadTooLarge`, maps to HTTP 413) → single `write_all` → `sync_data` → LRU `put` only on Ok (insert-after-sync, never before — pre-sync insert would poison the cache on write failure). The `POST /api/v1/learn` 201 response must not return until `sync_data` completes. Idempotency LRU is in-memory only, capacity 1024, keyed by `(canonicalized AgentRoot path, client_dedup_key)`; restart clears it (durable replay-protection is not v0.1 scope). On startup, the coordinator runs `truncate_malformed_tail`: walks forward, retains lines up to the last cleanly-parseable `\n`-terminated record, and `set_len + sync_data`s torn tails. **Writers must never emit blank lines** — `\n\n` is treated as a torn-write signal and halts recovery there. Sidecar storage for >4 KiB payloads is deferred to v0.1.1. Concurrent third-party writers to the JSONL are **not** supported in v0.1 despite PRD FR-1.2 — this is a deliberate scope cut. (DR-103, WEG-7.)

2. **Tantivy salience scoring is computed at query time, not indexed.** Storing the score would force daily re-indexing as `age_days` drifts. Schema fields: `content` (TEXT), `timestamp_sec` (u64 fastfield), `pain` (f64 fastfield), `importance` (f64 fastfield), `recurrence` (u64 fastfield). Implement a custom `Collector` + `Scorer` that fetches FastFields and computes:

   ```
   salience = exp(-age_days / 14.0) * (pain / 10.0) * (importance / 10.0) * (1.0 + ln(1.0 + recurrence))
   final_score = bm25 * salience
   ```

   Tantivy 0.23+ removed index-time sorting; do not rely on it. Indexing is incremental (5-second commit cadence), never a nightly rebuild.

3. **Dream cycle WAL.** Before any destructive op (replacing `LESSONS.md`, pruning JSONL), write `dream_in_progress.wal` containing `WalIntent` entries (`ReplaceSemanticMemory`, `PruneEpisodicMemory`, `Commit`). On startup, if the WAL exists, run compensating cleanup before serving traffic. Tested by `kill -9` mid-cycle and asserting `.agent/` is either pre- or post-cycle, never in-between.

4. **LLM cost cap and prompt versioning.** Estimate tokens with `tiktoken-rs` before each dream-cycle call; abort and fall back to deterministic mode if the estimate exceeds `$0.10` (DR-307, WEG-140, deferred to v0.1.1). Prompts are `include_str!`-bundled with a version ID like `dream-cycle/v1.1@2026-MM-DD`, written into `LESSONS.md` frontmatter. A `--no-llm` mode must always work without network (DR-308, WEG-61, the deterministic-exemplar path that ships at v0.1). The `personal/` layer is excluded from LLM calls unless `--share-personal`.

5. **Local API security is not optional.**
   - **Unix:** bind the axum server to a UDS at `~/.agent/dreamd.sock`, `0600` perms, with middleware that validates `SO_PEERCRED` (Linux) / `getpeereid` (macOS) on every request — connecting UID must match daemon owner.
   - **Windows:** bind `127.0.0.1` on an ephemeral port; require a bearer token written to `~/.agent/auth.json` with Windows ACLs.
   - Reject TCP binding to non-localhost without `--insecure`.

6. **MCP tool names.** The MCP server exposes `search_nodes` (→ `/api/v1/recall`) and `append_node` (→ `/api/v1/learn`). These names match the Anthropic reference memory server intentionally; do not rename. The MCP server is the **primary v0.1 distribution surface** — `npx dreamd-mcp` is the install path the README leads with, not `dreamd service install` (WEG-81).

7. **Schema versioning is mandatory.** Every persisted record carries `schema_version: "1.0"`. The current version output also exposes this field. Add a `dreamd migrate` path before changing it.

## API contract (when built)

Endpoints (axum, JSON, all under `/api/v1`):

- `POST /learn` — append episodic event; returns 201 only after `fdatasync`.
- `GET /recall?q=&k=` — BM25 × salience search.
- `POST /dream` — manual cycle trigger (202, async).
- `POST /migrate` — schema migration.

Schemas are specified in `context/PRD.md` §Tech Schemas. The `AgentLearning` struct (timestamp ISO 8601, `pain`/`importance` as `f32` 0–10, `recurrence` as `u32`, `skill_action` as the clustering key) is the canonical episodic record. The `AgentLearning` Rust type lands in `dreamd-protocol` (WEG-6).

## Performance targets (NFRs from PRD)

- Idle RSS < 30 MB.
- Stripped release binary < 15 MB. **Current state:** 839 KB stripped at end of Sprint 1 (`dreamd init` + `dreamd version` only). 17× under budget, with the daemon, tantivy, and HTTP server still to land.
- Recall P50 < 1 ms / P99 < 5 ms warm at 10k entries. The agile plan softens the public claim to `<5ms P50 warm, <50ms P99 cold` until benchmarked.

When changing index, scoring, or hot-path code, run `cargo bench` (criterion benches, DR-208) and check the binary-size CI gate (DR-809).

## Repo conventions

- License: Apache-2.0 (DR-009).
- Public-facing names and accounts already claimed: see `~/.claude/projects/-home-austingreen-Documents-botzr-projects-dreamd/memory/dreamd_surface_area.md` and `npm_account.md`.
- The `context/`, `.claude/`, and `assignments/` directories are gitignored on purpose — local working notes and PM-side spec docs (`WEG-X.v2.md`), not repo artifacts.
- Story IDs in commits/PRs follow `DR-XXX` (see `context/AGILE/plan1.md` for the backlog). WEG-IDs are Linear tracking surface; they appear in branch names (`dataprimecan/weg-NN-...`) but not in commit messages.
- Commits go through Austin's hand, not the PM session. PM never runs `git stash/add/commit/push`.

## Planning discipline (locked 2026-05-09 after grill round 6)

**Authoritative resolution stack:** PRD Part IV > PRD Part III > PRD Parts I-II > plan1.md grill-revision > plan1.md prior text. When editing either document, treat later layers as overrides; do not silently rewrite earlier text — add resolution sections with cross-references, mirroring the existing Part III pattern.

**Velocity floor:** Plan against **18-22 sustained pts/sprint** with a first-sprint spike of 28-32 that decays. The original `~30 pts/sprint` figure in `context/AGILE/plan1.md` Appendix B is a celebration number, not a planning number. Cumulative complexity (debugging unfamiliar stacks, ops setup, comms reactive work, dogfooding overhead at ~10% of weekly capacity) eats raw build hours faster than the original plan allowed. Sprint 1 is the first measurement; recalibrate Sprints 2-4 from real data, not vibes.

**Scope-discipline rule (applies from grill round 7 onward):** Any further grilling-round scope addition must trade 1:1.5 against existing scope — net +10 points of new work means -15 points of existing work. The grilling round must surface a candidate cut list as part of every "this should be locked" recommendation; the founder retains veto on what gets cut. Without this rule the meta-process generates work faster than the velocity floor can absorb regardless of whether each individual addition is correct. Six rounds added 46 points; round 7 cannot continue the pattern. **Round 7 (2026-05-09 CEO PRD review) was applied with proposed cuts (DR-906, DR-911, DR-410, DR-913 deferred/folded; DR-909 acceptance softened) totaling ~10 pts against ~7 pts of additions; founder vetoed nothing in the trade.**

**Grilling cadence rule (locked grill round 7 / 2026-05-09):** **No further grilling rounds before Sprint 1 ships.** Late-stage grilling rounds before measured velocity exists are the highest-risk cadence — they add scope to a plan whose realistic capacity is still hypothetical. Any "round 8" content the founder wants to surface defers to the post-Sprint-1 retrospective at minimum, where Sprint 2–4 plans can absorb additions through the velocity-gate cut sequence. The v0.1.1 scope freeze gets its own scheduled grilling round between week 9 (v0.1 ships) and week 10 (v0.1.1 freeze) — calendar event, not "if the founder feels like it."

**v0.1.1 scope freeze:** The cuts taken in Q6 (LLM dream cycle, OpenCode adapter, Windows lifecycle, semantic indexing pipeline DR-211) defer to v0.1.1, which gets its own scope freeze ONE WEEK after v0.1 ships. v0.1.1 is a real release with its own discipline — not an "everything that didn't make v0.1" graveyard. Kill criterion: if v0.1.1 is over capacity at end of Sprint 5, OpenCode adapter drops to v0.1.2. v0.1.1 freeze-notes file holds explicit deferrals (e.g., `dreamd init --quiet` per WEG-17).

**Launch target:** v0.1 ships **week 9** (was week 7; slipped two weeks after Q6). Pre-write the slip-announcement post in week 6.

**v0.1 wedge sentence (sacred):** *"AGENTS.md is what you wrote down. dreamd is what your agent learned."* Recommendations that weaken this are higher-bar; recommendations that strengthen it can be made aggressively. The post-Q6 v0.1 wedge framing is *"salience-scored cross-harness episodic recall + on-demand deterministic consolidation"* — LLM-assisted lessons land at v0.1.1, do NOT lead the README or HN draft with them.

**2026-05-12 Framing A adoption (founder override, WEG-183 / DR-925):** Public v0.1 lead becomes *"dreamd makes Claude Code, Cursor, and Cline remember the same things. Drop a `.agent/` folder in your repo. Every coding agent you use reads and writes to it."* Sacred sentence stays, with optional `, across every tool` tail. The post-Q6 engineering wedge stays in `context/PRD.md` Part IV §6 and `context/AGILE/plan1.md` round-7 revision section — **internal only, NOT in README / X bio / HN draft / spec / any public artifact**. Adoption cascade tracked by WEG-107 (DR-914 comms reset) which now also sweeps the new `docs/competitor-comparison.md` (WEG-174 / DR-926). Source of truth for new wedge text: `context/framing-a-rewrite.md`. The 1:1.5 scope-discipline rule was explicitly overridden by the founder for the 2026-05-12 batch; the rule remains in force for any future grilling-round additions.

## 2026-05-12 strategic additions (founder override, competitor research synthesis)

A 2026-05-12 competitor-research review (Cognee, Memvid, Mem0, Letta, Zep/Graphiti) identified 8 differentiator angles. Founder direction: **"only additions, no cuts"** — explicit override of the Q6/Q7 1:1.5 scope-discipline rule for this batch. The rule remains in force for any future grilling-round additions; this is a one-time override, not a precedent.

**Batch:** 23 new Linear tickets (WEG-174 through WEG-196) + 2 description updates (WEG-89, WEG-107).

| Bucket | Tickets | Points | Sprint | Status |
|---|---|---|---|---|
| Framing A wedge adoption | WEG-174, WEG-183 | 4 | sprint-3 (v0.1) | Doc/text only; existing WEG-89 + WEG-107 absorb the actual rewrite via description amendments. |
| Memory observability (angle #3) | WEG-175, WEG-176, WEG-184, WEG-185, WEG-194 | 18 | sprint-6 (v0.1.1) | Citation graph + `dreamd blame` + counterfactual recall + salience drift dashboard + docs. |
| Branchable memory (angle #1) | WEG-177, WEG-186, WEG-187, WEG-189, WEG-190, WEG-195 | 29 | v0.2 backlog (no sprint) | Snapshot model + `dreamd memory branch/checkout/diff/bisect` + spec + demo. Slot at week 9–10 v0.1.1 grill round. |
| Cryptographic forgetting (angle #2) | WEG-178, WEG-188, WEG-191, WEG-192, WEG-193, WEG-196 | 32 | v0.2 backlog (no sprint) | Merkle-ledger schema + provenance recording + `dreamd forget --proof` + cascade + doctor verification + GDPR/CCPA/HIPAA docs. |
| Research backlog (angles #4, #5, #6, #8) | WEG-179, WEG-180, WEG-181, WEG-182 | 14 (research) | (no sprint) | MCP-for-memory protocol formalization, CRDT multi-agent, compression-unified memory, hyperagent meta-memory. P2 priority, research-grade ACs only. |

**Capacity overflow at v0.1.1 / Sprint 6 (explicit):** Sprint 6 now sits at ~68 pts (was ~50) against 18–22 pts velocity floor. The week 9–10 v0.1.1 grill round (already committed by Q7) is the recalibration point; founder owns the trade. Existing kill criterion stands — OpenCode adapter drops to v0.1.2 if Sprint 5 over capacity.

**Sacred sentence stays.** Public wedge becomes Framing A. Engineering wedge stays in PRD + plan1.md (internal only).

**Source-of-truth files** (gitignored, local-only):
- `context/framing-a-rewrite.md` — wedge revision text + adoption checklist
- `context/competitor-comparison.md` — Cognee/Memvid/Mem0/Letta/Zep FAQ (lifted into `docs/competitor-comparison.md` by WEG-174 / DR-926 at execution time)
- `context/linear-batch-2026-05-12.md` — full ticket batch with ACs and dependency graph

**Angle that did NOT make this batch:** L8 "agent self / continuity layer" reframe (memory module first). Held for v0.2/v0.3 grill rounds. v0.1 ships as memory layer with cross-harness portability; the broader continuity-layer story matures by accretion (formalize `.agent/manifest.json`, elevate `personal/` slot, etc.) once v0.1 has traction.

## Paired-dev-loop conventions

The project runs on a paired-AI development loop (per `/mnt/skills/user/paired-dev-loop/SKILL.md`):

- **PM session** (this chat, web UI) — pre-flights tickets against Linear, writes spec docs to `assignments/WEG-X.v2.md`, produces bare prompts under 400 words for the dev session.
- **Dev session** (Claude Code in the repo) — implements per spec, runs verification, posts numbered report-back with raw command output.
- **PM session** then runs independent verification on the dev's report and signs off.
- **Austin** holds the git commit gate; PM never runs `git stash/add/commit/push`.

Linear is the canonical AC contract. NEXT.md captures sprint intent in brief but is not authoritative. Every pre-flight starts with `get_issue` on the target ticket. AC rewrites are part of pre-flight, not post-mortem. Session-close discipline: pull every touched ticket and verify both description and status before writing memory.

---

## Drift catalog

Empirical surprises caught during dev sessions. Each entry exists because a naive approach would have silently shipped wrong behavior. Read before any code change in the affected area.

### `dreamd-cli` package name is `dreamd`, not `dreamd-cli`

The `[package]` name in `crates/dreamd-cli/Cargo.toml` is `dreamd`. The directory is `dreamd-cli`; the package name and binary name are both `dreamd`.

Correct invocations:
```
cargo test -p dreamd
cargo run -p dreamd -- <subcommand>
cargo build -p dreamd
```

`-p dreamd-cli` will error with "package not found." Do not use it in bare prompts, CI scripts, or verification commands.

Source: WEG-9 dev report, 2026-05-12.

### Error output belongs on stderr; verification pattern for CLI commands

Any `dreamd` subcommand that exits non-zero must write its error message to stderr, not stdout. Success output goes to stdout only.

Verification pattern (paste into bare prompts and Step 5 checks):
```
# stdout must be empty on error:
cargo run -q -p dreamd -- <subcommand> 2>/dev/null
# stderr must contain the error message:
cargo run -q -p dreamd -- <subcommand> 2>&1 1>/dev/null
```

WEG-9 incident: "no project root" message was initially routed to stdout. Caught in Step 5; fixed before commit. Exit code 2 for missing-root, 1 for other I/O errors. WEG-18's missing-subcommand arm mirrors this (exit 2 to stderr).

Source: WEG-9 dev report, 2026-05-12.

### `cargo test` filter: positional = fn name, `--test` = binary name

`cargo test -p <pkg> <positional>` filters by **test function name** (substring), not the test binary/file name. A filter that doesn't match any fn name runs 0 tests and exits 0 — silent false positive.

To run a whole test binary (a file under `tests/`), use `--test <binary>`:
```
cargo test -p dreamd --test init_golden     # runs tests/init_golden.rs (3 fns)
cargo test -p dreamd init_golden            # runs zero tests; no fn name
                                            # contains "init_golden"
```

Verification pattern for golden-style binaries: always use `--test` form. Positional filters are reserved for narrowing within a binary by fn name.

Source: WEG-17 dev report, 2026-05-13. Spec for WEG-17 used the positional form; three fns are `first_run_matches_golden`, `rerun_matches_golden`, `no_project_root_fails_and_skips_scaffold` — none matched. Dev correctly substituted `--test init_golden`.

### `vergen-gitcl` `fail_on_error(false)` emits sentinels, not unset env vars

`vergen-gitcl` (and umbrella vergen 9+) with `fail_on_error(false)` does NOT leave failed-instruction env vars unset. It emits the literal string `VERGEN_IDEMPOTENT_OUTPUT` as the value. So `option_env!("VERGEN_GIT_SHA")` returns `Some("VERGEN_IDEMPOTENT_OUTPUT")` on tarball-from-crates.io builds, NOT `None`.

Any fallback pattern that relies on `match option_env!(...) { Some(s) => s, None => "unknown" }` is broken — the `Some` arm fires with sentinel content. Worst case: a 7-char-truncated SHA reading `VERGEN_` ships to crates.io users and no test catches it because the with-`.git` build path looks fine.

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

The rename-`.git` simulation is the only thing that catches this pre-merge; keep it in every vergen-touching ticket's verification block.

Also: `vergen-gitcl 1.0.8` re-exports `Emitter` from `vergen-lib 0.1.6` but its build/cargo/rustc feature flags route through `vergen`. `vergen >= 9.1` brings in `vergen-lib 9.1`, causing duplicate-`vergen-lib` trait mismatches at the `add_instructions(...)` call sites. Pin `vergen = "=9.0.6"` as a build-dep to keep the resolver from upgrading; revisit the pin in v0.1.1's dep-audit pass.

Source: WEG-18 dev report, 2026-05-13.

### `clap` auto-`--version` prepends the bin name

`clap`'s `#[command(version = LITERAL)]` formats `--version` output as `<bin_name> <LITERAL>`. If `LITERAL` already includes the bin name (e.g., `"dreamd 0.0.0 (...)"`), the output becomes `"dreamd dreamd 0.0.0 (...)"` — double prefix, byte-exact spec contracts break.

Fix: `#[command(disable_version_flag = true)]` + manual `-V` / `--version` handling in the dispatch. Side effect: `cli.command` becomes `Option<Command>` because `--version` is valid with no subcommand; add an explicit exit-2-to-stderr arm for the missing-subcommand case.

Source: WEG-18 dev report, 2026-05-13.

### Compile-time string assembly: `const_format` over `LazyLock`

For CLI strings that must be `&'static str` (clap's `version = ...` attribute, embedded asset identifiers, etc.), the established `dreamd-cli` pattern is `const_format` — `concatcp!` for assembly, `str_index!` for slicing — not `std::sync::LazyLock<String>`. Reasons:

- Keeps the value usable in const position (clap attributes, match arms).
- `str_index!(..7)` on a 7-char `"unknown"` fallback string is a no-op; on a 40-char vergen SHA it returns the first 7 — same code path, no panic.
- Adds one tiny pure-Rust dep (`const_format = "0.2"` → `konst`), no runtime init ceremony.

Use `LazyLock<String>` only if you genuinely need allocation-backed assembly (format args that const_format can't express, runtime env reads).

Source: WEG-18 dev report, 2026-05-13.

### `dreamd-protocol` deps stay locked to `serde + chrono + serde_json` — ULID minting lives in `dreamd-core`

`dreamd-protocol` is the parse/validate boundary for wire and on-disk types; it must not pull in id-generation, time-source, or other side-effectful crates. `EventId(String)` lives in protocol with a `parse(&str) -> Result<EventId, _>` constructor and custom `Serialize`/`Deserialize` that re-run validation. The actual ULID minting (`Ulid::new()` from the `ulid` crate) lives in `dreamd-core::coordinator` and rides into protocol via `EventId::parse(&format!("evt_{}", Ulid::new())).expect(...)`. The `expect` is sound because a freshly minted ULID is always canonical uppercase Crockford.

Adding `ulid` to `dreamd-protocol/Cargo.toml` would silently break the load-bearing dep-discipline policy (CLAUDE.md target architecture). If you find yourself reaching for it in protocol, add the generator to core and pass the validated string through `parse` instead.

Same rule applies to future newtype additions (e.g., agent-root paths, dedup keys promoted to types): protocol owns the *shape*, core owns the *minting*.

Source: WEG-7 dev report, 2026-05-14.

### `truncate_malformed_tail` treats `\n\n` as a torn-write signal

Startup recovery in `MemoryCoordinator::open` walks the JSONL forward and stops at the first unparseable line OR the first empty line (`\n\n`). The empty-line guard is intentional — torn writes can leave a blank gap between records — but it means any future writer that ever emits a blank line will silently halt recovery there, leaving the file half-scanned and subsequent appends landing past a malformed region.

**Rule:** writers must never emit blank lines into `AGENT_LEARNINGS.jsonl`. If a future feature wants empty-line semantics (e.g., record separators), the recovery convention has to change first — not the writer.

Source: WEG-7 dev report, 2026-05-14.