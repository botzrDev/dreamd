# CLAUDE.md

Guidance for Claude Code in this repo. **Most context lives in memory** — this file is the runtime contract, not the project encyclopedia. See `MEMORY.md` for grill-locked decisions, Framing A wedge, Linear workflow, drift catalog, and PM session history.

## Project status

Pre-release. Sprint 1 of 6, complete (16/16). Sprint-1 tickets shipped: WEG-5, 6, 7, 8, 9, 10, 11, 15, 16, 17, 18, 20, 21, 26, 28, 173. WEG-15 (reset workspace CLI / DR-113) closed in Linear 2026-05-14; post-ship verification on 2026-05-18 (HEAD 41aacb1) confirms `dreamd reset workspace --yes` ships and tests are green. **Sprint 2 in progress.** WEG-75 v2 (DR-412: registry reader/resolver + `dreamd init --uninstall-project`) PM-verified 2026-05-19; uncommitted (Austin holds commit gate). Workspace version stays at `0.0.0` through v0.1 release-bump. Launch target: week 9. v0.1 wedge framing: see [[framing-a-wedge-competitor-research-additions-2026-05-12-founder-override]] (public) and [[grill-locked-decisions-2026-05-09]] (engineering, internal-only).

The intended end-state architecture lives in `context/PRD.md` and `context/AGILE/plan1.md` — both gitignored and local-only. Treat those as engineering ground truth; on-disk code is partial. Latest dev-session detail in [[dreamd-pm-session-memory-2026-05-19]].

**Story → DR map for pending v0.1 work:**
- `WEG-75 ↔ DR-412` — registry reader/resolver + `--uninstall-project` (PM-verified 2026-05-19; uncommitted)
- `WEG-50 ↔ DR-107` — `dreamd doctor --cluster-health` (Sprint 2)
- `WEG-68` — `POST /api/v1/learn`: wire `Idempotency-Key` → `client_dedup_key` + `PayloadTooLarge` → HTTP 413
- `WEG-81` — `npx dreamd-mcp` distribution (was blocked by WEG-17, now unblocked)

**Backlog (post-v0.1, surfaced by recording-prep dry run 2026-05-18):**
- `WEG-204` — `dreamd init`: surface project-root sentinel requirement in user-facing copy (Low / `epic-8-cli-lifecycle`)

## What dreamd is

Local-first, single-binary daemon that provides a portable memory layer for AI coding agents. Exposes a standardized `.agent/` folder (`working/`, `episodic/`, `semantic/`, `personal/`) and a local HTTP API; an MCP server maps to that API so Claude Code, Cursor, OpenCode, etc. share memory across harnesses. **File system is the source of truth** — the daemon reads/writes plain files (markdown, JSONL) so users can edit them by hand.

## Commands

```
cargo build --workspace                  # build all crates
cargo run -p dreamd -- init              # scaffold .agent/ in cwd
cargo run -p dreamd -- version           # print version block
cargo run -p dreamd -- --version         # print version single-line

cargo test -p dreamd-core                # 23 tests (coordinator + privacy + layout)
cargo test -p dreamd-protocol            # 6 tests (EventId + AgentLearning round-trip)
cargo test -p dreamd --test init_golden  # 3 tests (init stdout golden fixtures)
cargo check --workspace
cargo clippy --workspace
cargo fmt --all

scripts/coverage.sh                      # workspace coverage (html + lcov) → target/coverage/
scripts/coverage.sh --open               # same, open HTML in browser
```

CLI package name is `dreamd`, not `dreamd-cli` — see [[cargo-package-name-is-dreamd]]. Cargo test filter form — see [[cargo-test-filter-form]]. `Justfile` / `cargo xtask` (DR-005) is queued, not present; CI matrix (DR-003) ships at `.github/workflows/ci.yml` (Ubuntu+macOS+Windows).

## Target architecture (end-state)

Cargo workspace per DR-002:

- **`dreamd-protocol`** — shared serde types only. Deps **locked** to `serde + chrono + serde_json`. Owns parse/validate boundary (e.g., `EventId`). See [[protocol-deps-minting-in-core]].
- **`dreamd-core`** — modules: `api` (axum), `io` (FS/WAL), `index` (tantivy), `dream` (consolidation pipeline), `vcs` (git2), `coordinator` (actor). Currently has `layout` + `privacy` + `coordinator` + `lessons` + `server`; rest queued.
- **`dreamd-cli`** — exists; package name `dreamd`. Future `dreamd-store` / `dreamd-server` splits may collapse into `dreamd-core` (DR-114); defer until actor topology ships. **No `dreamd-server` crate** — WEG-21 was the tripwire; decision: stay in `dreamd-core::server` until a second Rust binary consumer exists. See [[no-dreamd-server-crate-until-second-consumer]].

State management is an actor model: a single `MemoryCoordinator` task owns mutable state. **Do not introduce parallel writers** to JSONL or index — every mutation goes through the coordinator. `&mut self` on the run loop is the exclusivity guarantee (no `Mutex<File>` needed). See [[actor-mut-self-is-the-lock]].

## Load-bearing engineering decisions

Binding when the relevant code lands. Don't change without re-reading PRD.

1. **JSONL append durability** (DR-103, shipped WEG-7). All appends to `AGENT_LEARNINGS.jsonl` flow through one `MemoryCoordinator` actor. Write order: idempotency-LRU lookup → mint `EventId` (`evt_` + 26-char Crockford ULID via `ulid` crate in `dreamd-core`, NOT protocol) → overwrite inbound `learning.id` → serialize → ensure trailing `\n` → 4 KiB hard reject (`MAX_LEARNING_LINE_BYTES = 4096`, returns `PayloadTooLarge`, HTTP 413) → single `write_all` → `sync_data` → LRU `put` **only on Ok** (insert-after-sync; pre-sync insert would poison cache on write failure). `POST /api/v1/learn` 201 must not return until `sync_data` completes. Idempotency LRU is in-memory only, cap 1024, keyed by `(canonicalized AgentRoot path, client_dedup_key)`; restart clears it (durable replay-protection is not v0.1). On startup, `truncate_malformed_tail` walks forward, retains lines up to the last cleanly-parseable `\n`-terminated record, `set_len + sync_data`s torn tails. **Writers must never emit blank lines** — see [[torn-write-blank-line-signal]]. Sidecar storage for >4 KiB deferred to v0.1.1. Concurrent third-party writers to the JSONL are **not** supported in v0.1 despite PRD FR-1.2 — deliberate scope cut.

2. **Tantivy salience scoring is query-time, not indexed.** Storing the score would force daily re-indexing as `age_days` drifts. Schema fields: `content` (TEXT), `timestamp_sec` (u64 fastfield), `pain` (f64 fastfield), `importance` (f64 fastfield), `recurrence` (u64 fastfield). Custom `Collector` + `Scorer` fetches FastFields and computes:

   ```
   salience = exp(-age_days / 14.0) * (pain / 10.0) * (importance / 10.0) * (1.0 + ln(1.0 + recurrence))
   final_score = bm25 * salience
   ```

   Tantivy 0.23+ removed index-time sorting; do not rely on it. Indexing is incremental (5-second commit cadence), never a nightly rebuild.

3. **Dream cycle WAL.** Before any destructive op (replacing `LESSONS.md`, pruning JSONL), write `dream_in_progress.wal` containing `WalIntent` entries (`ReplaceSemanticMemory`, `PruneEpisodicMemory`, `Commit`). On startup, if WAL exists, run compensating cleanup before serving traffic. Tested by `kill -9` mid-cycle and asserting `.agent/` is either pre- or post-cycle, never in-between.

4. **LLM cost cap and prompt versioning.** Estimate tokens with `tiktoken-rs` before each dream-cycle call; abort and fall back to deterministic mode if estimate > `$0.10` (DR-307, WEG-140, **deferred to v0.1.1**). Prompts are `include_str!`-bundled with a version ID like `dream-cycle/v1.1@2026-MM-DD`, written into `LESSONS.md` frontmatter. A `--no-llm` mode must always work without network (DR-308, WEG-61 — the deterministic-exemplar path that ships at v0.1). The `personal/` layer is excluded from LLM calls unless `--share-personal`.

5. **Local API security is not optional.**
   - **Unix:** axum bound to UDS at `~/.agent/dreamd.sock`, `0600` perms, middleware validates `SO_PEERCRED` (Linux) / `getpeereid` (macOS) on every request — connecting UID must match daemon owner.
   - **Windows:** bind `127.0.0.1` on ephemeral port; require bearer token written to `~/.agent/auth.json` with Windows ACLs. **Deferred to v0.1.1.**
   - Reject TCP binding to non-localhost without `--insecure`.

6. **MCP tool names.** The MCP server exposes `search_nodes` (→ `/api/v1/recall`) and `append_node` (→ `/api/v1/learn`). Names match the Anthropic reference memory server intentionally; do not rename. **MCP server is the primary v0.1 distribution surface** — `npx dreamd-mcp` is the install path the README leads with, not `dreamd service install` (WEG-81).

7. **Schema versioning is mandatory.** Every persisted record carries `schema_version: "1.0"`. Current version output exposes this field. Add a `dreamd migrate` path before changing it.

8. **`unsafe_code` policy.** Workspace lint is `unsafe_code = "forbid"`. `dreamd-core` carries a scoped `unsafe_code = "deny"` override — the sole exception is `detach_double_fork`, which carries `#[allow(unsafe_code)]` with a SAFETY contract. Do not widen the downgrade to other crates. See [[dreamd-core-unsafe-deny-override]].

## API contract (when built)

Endpoints (axum, JSON, all under `/api/v1`):

- `POST /learn` — append episodic event; returns 201 only after `fdatasync`. Wires `Idempotency-Key` header → `client_dedup_key` and `CoordinatorError::PayloadTooLarge` → HTTP 413 (queued WEG-68).
- `GET /recall?q=&k=` — BM25 × salience search.
- `POST /dream` — manual cycle trigger (202, async).
- `POST /migrate` — schema migration.

Schemas in `context/PRD.md` §Tech Schemas. `AgentLearning` is the canonical episodic record (timestamp ISO 8601, `pain`/`importance` as `f32` 0–10, `recurrence` as `u32`, `skill_action` as clustering key). Type lives in `dreamd-protocol` with `id: EventId`.

## Performance targets (NFRs)

- Idle RSS < 30 MB.
- Stripped release binary < 15 MB. **Current state:** 839 KB stripped at end of Sprint 1 (`init` + `version` only). 17× under budget with daemon/tantivy/HTTP still to land.
- Recall P50 < 1 ms / P99 < 5 ms warm at 10k entries. Public claim softened to `<5ms P50 warm, <50ms P99 cold` until benchmarked.

When changing index, scoring, or hot-path code, run `cargo bench` (criterion, DR-208) and check the binary-size CI gate (DR-809).

## Repo conventions

- License: Apache-2.0 (DR-009).
- Public-facing names and accounts: see [[dreamd-surface-area]] and [[npm-account]].
- `context/`, `.claude/`, `assignments/`, and `docs/` are **gitignored on purpose** — local working notes, PM-side spec docs (`WEG-X.v2.md`), and end-state architecture docs.
- Story IDs in commits/PRs follow `DR-XXX`. WEG-IDs are Linear tracking surface; they appear in branch names (`dataprimecan/weg-NN-...`) but not commit messages.
- **Austin holds the git commit gate.** PM session never runs `git stash/add/commit/push`. See [[dreamd-linear-workflow-+-assignment-tracking-contract]] for the full operating contract.

## Pointers to memory (everything else)

- **Sacred wedge sentence** (do not weaken): *"AGENTS.md is what you wrote down. dreamd is what your agent learned."* Optional tail `, across every tool` (Framing A, see [[framing-a-wedge-competitor-research-additions-2026-05-12-founder-override]]). Recommendations that weaken this are higher-bar; recommendations that strengthen it can be made aggressively.
- **Planning discipline, velocity floor, scope-discipline rule, grilling cadence, v0.1.1 freeze, wedge framing** → [[grill-locked-decisions-2026-05-09]]
- **2026-05-12 strategic additions (Framing A, 23 new Linear tickets, Sprint 6 overflow)** → [[framing-a-wedge-competitor-research-additions-2026-05-12-founder-override]]
- **Linear workflow / sprint-N labels / In-Progress→In-Review→Done / backlog seed status** → [[dreamd-linear-workflow-+-assignment-tracking-contract]]
- **Linear is canonical AC; markdown lags gate amendments** → [[linear-is-canonical-ac-markdown-is-briefing]]
- **Paired-dev-loop conventions** → see `/mnt/skills/user/paired-dev-loop/SKILL.md` + [[dreamd-linear-workflow-+-assignment-tracking-contract]]
- **Sprint 1 retro pending (~2026-05-22)** → [[sprint-1-retro-pending-2026-05-22]]
- **Refinement-bumps metric** → [[refinement-bumps-discipline]]
- **Grill-me collaboration style** → [[grill-me-collaboration-style]]

### Drift catalog (empirical surprises — read before touching the area)

**Build / CLI**
- **Cargo package name is `dreamd`** (not `dreamd-cli`) → [[cargo-package-name-is-dreamd]]
- **stderr/stdout verification pattern** → [[stderr-stdout-verification-pattern-for-cli-error-output]]
- **cargo test filter form** (`--test <binary>` vs positional) → [[cargo-test-filter-form]]
- **`clap` auto-`--version` prepends bin name** → [[clap-auto-version-prepends-bin-name]]
- **`const_format` over `LazyLock` for `&'static str` assembly** → [[const-format-over-lazylock]]
- **vergen `fail_on_error(false)` emits `"VERGEN_IDEMPOTENT_OUTPUT"` sentinel** → [[vergen-fail-on-error-emits-sentinel]]
- **`vergen = "=9.0.6"` pin alongside vergen-gitcl 1.0.8** → [[vergen-gitcl-pin-vergen-9-0-6]]
- **Verify a dep's workspace promotion before claiming it in an AC.** `grep -A 30 '\[workspace.dependencies\]' Cargo.toml` — if the section is absent or the dep isn't listed, write the AC as "add to crate `[dependencies]`", not "use workspace dep". As of WEG-41, the workspace `Cargo.toml` has **no** `[workspace.dependencies]` section (only `insta` as a workspace dev-dep from WEG-20). → [[workspace-dep-preflight]]
- **Per-crate dep preflight: grep `crates/<crate>/Cargo.toml` directly before claiming dep X is available in crate Y.** Other crates' usage of X, `use` paths elsewhere, and architect pattern-match ("daemons need tracing") are **not** evidence. WEG-49 tripwire: pre-flight asserted `tracing` was already a dep in `dreamd-core`; it wasn't. Orthogonal to workspace-dep-preflight — a dep may exist at neither, one, or both levels. → [[per-crate-dep-preflight]]
- **PM spec cannot cite helpers across backward crate deps.** When a spec for crate A says "call helper X," verify X lives in A or in a crate A depends on. If X lives in a crate that depends on A (e.g., `dreamd-cli`'s `check_dream_mode` cited from a `dreamd-core` spec), the backward dep is forbidden and the helper must be inlined. Same pre-flight family as `[[per-crate-dep-preflight]]` but for symbols, not deps. WEG-88 tripwire. → [[spec-cant-cite-helpers-across-backward-crate-deps]]
- **Cargo.lock transitive dep check: grep `Cargo.lock` before adding a crate to `[dependencies]`.** If already resolved transitively, pin to that major — do not introduce a parallel major. WEG-43 tripwire: spec said `ordered-float = "4"`, but `tantivy → tantivy-query-grammar` had already pulled `ordered-float 5.3.0`; pinning "4" would have lock-doubled the major. Orthogonal to the workspace/per-crate pre-flights — checks lockfile reality, not Cargo.toml assertions. → [[cargo-lock-transitive-dep-check]]
- **PM pre-flight: verify epoch literals against ISO dates.** When a prompt or AC carries both `YYYY-MM-DDTHH:MM:SSZ` and a parenthetical unix-int, run a one-line chrono round-trip before queue — epoch math drifts silently and a year-off (`365*86400 = 31536000`) produces plausible-looking numbers. WEG-46 tripwire: prompt said `1748865600 = 2026-06-02T12:00:00Z` but that integer is 2025-06-02; dev caught it via salience-value mismatch against EXPECTED.md. Same pre-flight family as workspace/per-crate/Cargo.lock checks. → [[pm-verify-epoch-literals]]
- **`NOW_SEC` (or any epoch literal) must be verified via Python or chrono round-trip BEFORE the spec is published.** `[[pm-verify-epoch-literals]]` covers the rule; this entry is the operational addition — the verification command must run as part of pre-flight, not be deferred to the dev. Example: `python3 -c "import datetime; print(datetime.datetime.fromtimestamp(1747137600, datetime.UTC))"` confirms `1_747_137_600 = 2025-05-13`, off by exactly one year from a typo'd `2026-05-13`. WEG-70 spec carry-forward. See also `[[pm-verify-epoch-literals]]`. → [[pm-verify-epoch-literals-operationalize]]
- **`tower-http` is NOT transitive via `axum = "0.8"` — add as a direct dep.** Any crate using `tower_http` (`TraceLayer`, `CompressionLayer`, etc.) must add it explicitly. Pre-flight: `grep "tower-http" crates/<crate>/Cargo.toml`. WEG-67-A tripwire. → [[tower-http-not-transitive-via-axum]]
- **`DaemonHome::registry_toml()`, not `registry_path()`.** The registry path helper is named `registry_toml()`; any spec or code calling `registry_path()` is wrong. WEG-67-B tripwire. → [[daemon-home-registry-toml-not-registry-path]]

**Testing / visibility**
- **Integration tests at `<crate>/tests/*.rs` cannot reach `pub(crate)` symbols from a bin-only crate** (no `lib.rs` = no reachable surface). If a test needs internal symbols, either expose them behind `#[cfg(test)]` + `pub` in a `lib.rs`, or restructure. → [[integration-test-pub-crate-visibility]]
- **PM-side AC pre-flight: grep cited symbols for `pub(crate)` and verify `lib.rs` exists before queue.** If the symbol is `pub(crate)` and there is no `lib.rs`, the integration-test AC is un-implementable as written — amend before handoff. → [[pm-preflight-pub-crate-grep]]
- **First consumer of a fixture verifies EXPECTED.md against actual output.** Fixture expected-values authored before the implementation exists are **provisional**; the first integration test that consumes them MUST verify against real output and amend in the same PR if wrong (with an §Audit notes section). WEG-46 tripwire: `tests/fixtures/demo-corpus/EXPECTED.md` asserted top-3 = {E20, E15, E10} but `recall()` returned {E20, E19, E17} — headroom analysis evaluated wrong cap age, and one cluster's lexical-match wasn't analyzed at all. Amendment +80/−53. Same principle as Linear-is-canonical-AC, one layer over from the issue tracker. → [[first-consumer-verifies-expected-md]]
- **Any spec touching `InitArgs`, `Command`, or `cli.rs` must include `cargo test --test cli_help` in report-back §5 and pre-name the expected `cli_help__*.snap` update.** Any new flag or subcommand changes the WEG-20 insta snapshots; without the call-out the dev either misses it (CI fail) or it looks like an unexplained snapshot change at verification. WEG-75.v2 omission — dev handled it correctly anyway. → [[cli-help-snap-in-report-back]]

**I/O / durability**
- **`dreamd-protocol` deps locked; minting in core** → [[protocol-deps-minting-in-core]]
- **`.tmp` file is preserved on `write_atomic` failure — deliberate recovery signal.** Never add cleanup code in the write path. The presence of a `.tmp` is the signal that the previous write was interrupted. → [[write-atomic-tmp-preserved-on-failure]]
- **Parent-dir fsync after rename requires `File::open(parent)?.sync_all()`.** You cannot call `sync_data()` on a `PathBuf`. Open the directory as a `File`, then call `sync_all`. → [[parent-dir-fsync-file-open-sync-all]]
- **Torn-write blank-line halt signal in JSONL recovery** → [[torn-write-blank-line-signal]]
- **Timestamps are always caller-provided.** Writer functions (e.g., `LessonsFile`) must never call `Utc::now()` internally. Timestamps belong to the caller — deterministic tests depend on this. → [[timestamps-caller-provided-no-utc-now]]

**HTTP / API**
- **Axum's `Json<T>` extractor returns 415 (not 501 or 422) when `Content-Type: application/json` is absent.** The content-type check is enforced at the framework level, not in handler code. Tests hitting `/api/v1/learn` without the header must assert 415. WEG-68-A tripwire. → [[axum-json-extractor-415-missing-content-type]]

**Actor / concurrency**
- **`&mut self` in the coordinator run loop IS the exclusivity guarantee.** No `Mutex<File>` inside the actor. "Mutex" in DR-103 means the coordinator is the serialization point, not that a `Mutex` type is used. → [[actor-mut-self-is-the-lock]]
- **`#[non_exhaustive]` on actor message enums.** Define only variants with complete handlers. Adding a Sprint-N variant without a handler produces a compile error — that's the signal, not a stub. → [[non-exhaustive-actor-message-enums]]
- **tokio feature split: library crates declare `features = ["sync"]` only; binary (`dreamd-cli`) owns `["rt-multi-thread", "macros"]`; dev-deps use `["rt", "macros", "sync", "time"]` for `#[tokio::test]`. Exception: `[[bin]]` targets inside a library crate (e.g., `dreamd-core` post-WEG-21) force `rt` + `macros` into `[dependencies]`, not `[dev-dependencies]` — `[[bin]]` targets do not inherit `[dev-dependencies]`. `dreamd-core` features are therefore `["sync", "rt", "macros"]`.** → [[tokio-feature-split-bin-target-exception]]
- **`MemoryCoordinatorMsg::AppendLearning` response carries `Result<AppendOutcome, CoordinatorError>`, not `Result<EventId, …>`.** Call sites must handle `AppendOutcome` and extract `outcome.id` + `outcome.deduplicated`; `EventId` is not the return type. WEG-68-B tripwire. → [[append-learning-returns-append-outcome-not-event-id]]
- **`tokio::net::UnixListener::from_std()` requires `.set_nonblocking(true)` on the stdlib listener immediately before conversion.** Skipping it compiles but panics at runtime on the first accept. WEG-72-A tripwire. → [[unix-listener-from-std-set-nonblocking]]
- **`hyper_util` `serve_connection()` requires wrapping the stream: `TokioIo::new(stream)`.** `tokio::net::UnixStream` does not implement hyper's `IO` trait directly; the missing wrap is a compile error. WEG-72-B tripwire. → [[hyper-util-tokio-io-wrap-required]]

**Safety**
- **`unsafe_code = "forbid"` is workspace-level; `dreamd-core` carries a scoped `"deny"` override.** The sole exception is `detach_double_fork` (`#[allow(unsafe_code)]` + SAFETY contract). Do not widen the downgrade. Documented in `docs/architecture.md` (untracked). → [[dreamd-core-unsafe-deny-override]]

**Index / Tantivy**
- **Never unlink `.tantivy-writer.lock` or `.tantivy-meta.lock` on startup or recovery.** Tantivy 0.26 uses advisory `fs4` flock — the kernel releases the lock when the holder dies; the on-disk file is a marker, not the gate. `IndexWriter::new()` opens cleanly after SIGKILL. Removing a still-held lock file would break a live writer. Lock-file cleanup, if ever needed, belongs behind a manual repair flag. → [[tantivy-lock-file-no-rm-on-startup]]
- **`TantivyIndexHandle::open()` allocates a 50 MB `IndexWriter` even for read-only callers.** Phase 1 `search_nodes` opens a fresh handle per call — acceptable only because calls are infrequent and Phase 2 replaces this path with the daemon bridge. Do not copy this pattern for high-frequency or latency-sensitive callers. WEG-78-A tripwire. → [[tantivy-index-handle-open-allocates-writer]]

**Spec / process**
- **Session-internal summary counts are not source of truth.** When counting tasks/tests/items across any session report, count the table directly. Never trust the report's internal summary number, even from a careful-looking report. If the table and summary disagree, the table wins. WEG-25 spike synthesis tripwire (summary said 51/51 + 11/11; tables showed 50/50 + 10/10). → [[session-internal-counts-are-not-source-of-truth]]
- **`grep -c` is not a reliable count for constant-name searches.** Production grep targeting a constant or symbol name will also hit doc comments, doctest blocks, and test code referencing the constant — so an "expected 0 hits" annotation drifts the moment a comment is added. Either omit the count annotation entirely, or specify "at least 1 hit in production code" with `grep -v '^[^:]*://'` to exclude comments. WEG-70 spec tripwire. → [[grep-c-not-for-constant-names]]
