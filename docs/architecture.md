# dreamd architecture

This document captures the load-bearing structural decisions for the running
codebase. Read it before touching the actor topology, the JSONL writer, or
the crate split.

## Contents

The spine, in reading order:

1. [Crate-split tripwires](#crate-split-tripwires) — the three-crate workspace
   and the exact conditions under which it splits.
2. [Actor model: `MemoryCoordinator` is the serialization point](#actor-model-memorycoordinator-is-the-serialization-point)
   — IPC topology and the single-writer durability path.
3. [Indexing and salience](#indexing-and-salience) — Tantivy schema, manifest
   versioning, query-time scoring.
4. [Dream cycle](#dream-cycle) — consolidation (deterministic, or opt-in
   LLM-composed with a deterministic fallback), decay, autobiography, and the
   two manual triggers.
5. [WAL / crash recovery](#wal--crash-recovery) — `dream_in_progress.wal`, and
   what "never mid-cycle" does and does not cover.
6. [API stability](#api-stability) — what the SPEC freezes and what it does not.
7. [Threat model](#threat-model) — the trust boundary, in one paragraph.

Supporting detail, after the spine:

8. [Writer-process lifecycle](#writer-process-lifecycle-weg-21--dr-118)
9. [API backpressure](#api-backpressure)
10. [Observability](#observability-weg-32--dr-004)
11. [Related docs](#related-docs)

## Crate-split tripwires

The workspace currently has three crates:

- `dreamd-protocol` — pure serde types (DR-102 / WEG-6).
- `dreamd-core` — the whole engine. Storage and paths: `layout`, `io`,
  `episodic`, `coordinator`, `registry`, `daemon_state`, `config`, `privacy`,
  `redaction`, `migrate`. Recall: `index`, `collector`, `salience`, `ingress`,
  `memory_store`. Dream cycle: `dream_cycle`, `consolidation`, `lessons`, `llm`,
  `decay`, `wal`, `autobiography`. Forget, branches, provenance: `forget`,
  `snapshot`, `memory_diff`, `bisect`, `provenance`. Transport: `server`
  (HTTP + daemon), `mcp`, `client` / `daemon_client`, `observability`.
  Library-only, not wired into recall or MCP: `context_grant`, `letta`, `rrf`,
  and `vectors` (behind the non-default `vectors` feature).
- `dreamd` (the CLI crate, package name `dreamd`, dir `dreamd-cli`) — clap
  definitions in `cli.rs`, one module per subcommand under `commands/`.

Neither `dreamd-server` nor `dreamd-store` exists. Both names below are
extraction **triggers** — conditions to re-evaluate under — not workspace
members. Future splits will happen exactly when their tripwires fire, not
before:

- **`dreamd-server`** — WEG-21 landed (2026-05-14) without extracting a
  separate crate. Resolution: UDS binding, double-fork lifecycle, the HTTP
  surface, and the `MemoryCoordinator` supervisor all live in `dreamd-core`
  behind `pub mod server`. Module layout:
  - `crates/dreamd-core/src/server/uds.rs` — `bind_writer_socket` with
    orphan-recovery and 0600 perms; `SocketGuard` Drop-cleans the path.
    Production `dreamd watch` uses `uds_server::bind_api_socket` instead
    (manual unlink on shutdown); see ARCHITECTURE.md §8.1.
  - `crates/dreamd-core/src/server/http/` — the Axum router and handlers
    behind `/api/v1/*`. There is no separate server crate to point at; this
    directory is the whole HTTP surface.
  - `crates/dreamd-core/src/server/lifecycle.rs` — `Supervisor` owning the
    coordinator sender + handle, plus the Unix `detach_double_fork` helper.
  - `crates/dreamd-core/src/server/index_map.rs` — `IndexHandle` trait,
    `IndexError`, and `ProjectIndexMap<H>` (LRU cap 10, idle eviction 30 min;
    the policy itself lives in `project_resource_map.rs` and is shared with
    `supervisor_map.rs`) with `TestIndexHandle`.
  - `crates/dreamd-core/src/server/tantivy_handle.rs` — the Tantivy-backed
    `TantivyIndexHandle` (`open` / `flush` / `shutdown` / `close`) and the
    startup replay. The indexer task is `server/indexer_actor.rs`; the
    watermark and `assess_index_freshness` are `server/index_freshness.rs`
    (BZR-170 split; `tantivy_handle` re-exports their public items).
  - `crates/dreamd-core/src/server/watch.rs` — `run_watch`: daemon boot,
    signal handling, and the bounded shutdown drain.

  **Re-evaluation triggers** (when, not if, to revisit the extraction):
    1. dreamd-core's compile time crosses an empirical threshold (rule of
       thumb: >2× the cold compile measured when the Tantivy dep landed).
    2. A second Rust binary consumer needs the server stack without
       pulling the index/dream/io modules along with it.

  Until one of those fires, the single-crate layout keeps the `cargo test
  -p dreamd-core` story simple and the supervisor adjacent to the
  coordinator it manages.

  Lint override: `dreamd-core/Cargo.toml` locally downgrades
  `unsafe_code` from `forbid` (workspace) to `deny`. The sole authorised
  callsite is `server::lifecycle::detach_double_fork`, which calls
  `nix::unistd::fork` (an `unsafe fn`). **Zero production call sites in
  any shipped path** — `dreamd service install` (systemd `--user` unit,
  AILAB-190; macOS LaunchAgent, AILAB-169; Windows scheduled task,
  AILAB-203) supervises the foreground `dreamd watch` it registers rather
  than calling this helper (see `ARCHITECTURE.md` §8.1). All other modules in the crate still
  surface unsafe usage at compile time.

- **`dreamd-layout`** splits out when (and only when) a no-tokio consumer
  of layout appears. Today every layout caller already pulls in tokio
  indirectly via the coordinator; a split would create an empty re-export
  crate. The DR-002 plan anticipates a `dreamd-store` split too — the index
  and WAL code have landed inside `dreamd-core`, and no consumer has needed
  them without the rest of the crate, so that tripwire has not fired either.

Premature splits are reversible but costly: each split adds a `version =`
bump, a Cargo path entry, and a CI matrix dimension. The default answer to
"should this be a new crate?" is **no, until a tripwire fires.**

## Actor model: `MemoryCoordinator` is the serialization point

State management is an actor model. A single `MemoryCoordinator` tokio task
owns the mutable handle to `AGENT_LEARNINGS.jsonl`. API handlers, the
in-process MCP store, and the dream-cycle and forget paths send messages
(`AppendLearning`, `RunDreamCycle`, `Forget`, `Shutdown`) over a
`tokio::sync::mpsc` channel; only the coordinator writes. The one exception is
a CLI command run with no daemon (`dreamd dream`, `dreamd forget`,
`dreamd archive`, `dreamd memory checkout`): it rewrites the files in-process,
and `forget`, `archive`, and `checkout` refuse while a daemon is live.

The coordinator does **not** wrap its `File` in a `Mutex`. The `&mut self`
on `MemoryCoordinator::run` is the exclusivity guarantee — there is no other
handle to the file, so there is nothing to lock against. DR-103 specifies a
"`tokio::sync::Mutex<File>` owned by the coordinator" as a logical contract;
the implementation realises that contract by making the coordinator the sole
owner of an unwrapped `File`. The serialization is structural, not
lock-based.

Durability path for `AppendLearning` (`MemoryCoordinator::handle_append`, which
first checks the idempotency LRU and stamps the daemon-minted `id`,
`schema_version`, and `timestamp`, then calls `episodic::append`):

1. `serde_json::to_string(&learning)` — produce a single JSON line.
2. Ensure a trailing `\n`; reject a line over 4 KiB (`MAX_LEARNING_LINE_BYTES`).
3. `file.write_all(...)` — returns only once the complete prepared line has been
   written, or errors. It is not a single-syscall guarantee: `write_all` loops
   over the underlying `write` until the buffer is drained, so a line may reach
   the file through more than one write.
4. `file.sync_data()` — fdatasync to disk.
5. Await the hand-off of `IndexerMsg::Append` to the indexer task, then send
   `Ok(AppendOutcome)` over the oneshot.

The `POST /api/v1/learn` 201 response must not return until step 5 fires.
Concurrent third-party writers to the JSONL are explicitly out of scope
despite the looser language in PRD FR-1.2.

Single-writer serialization and fsync-before-ack are the two claims here that
draw the most scrutiny, so they should be checked against live evidence rather
than this summary of it:
[`../ARCHITECTURE.md` §1 "JSONL append durability"](../ARCHITECTURE.md#1-jsonl-append-durability)
for the decision and its constraints, [`../SPEC.md`](../SPEC.md) for the
on-disk contract those writes satisfy, and [`compared.md`](compared.md) for
how the guarantee lines up against Mem0 / Letta Code / MCP-ref / Cline Memory
Bank.

Blocking I/O note: the coordinator uses `std::fs::File`, so `write_all` and
`sync_data` block the tokio task. This is acceptable for v0.1 because the
actor already serializes mutations — there is no concurrency to preserve.
Do not introduce `tokio::fs` or `spawn_blocking` without benchmark evidence
and an ADR amendment; the cost of context switching per append likely
dominates the benefit at our target write rates.

The message enum `MemoryCoordinatorMsg` is `#[non_exhaustive]` so additions
stay non-breaking for downstream `match` consumers. Its variants today are
`AppendLearning`, `RunDreamCycle`, `Forget` (BZR-158), and `Shutdown`.

## Indexing and salience

Tantivy 0.26.1 backs episodic recall. The schema is defined in
`dreamd-core::index::build_schema()` — eleven fields: one TEXT field
(`content`), four FastFields for the salience inputs (`timestamp_sec`, `pain`,
`importance`, `recurrence`), two more FastFields that are written but not read
on the recall path (`last_updated_sec`, `cited_event_count`), and four
exact-match `STRING | STORED` fields (`layer`, `event_id`, `skill_action`,
`source_harness`). One index holds two document layers: `episodic` events and
`semantic` lessons from `LESSONS.md` (AILAB-205, ids `lsn_<lesson id>`). The
semantic layer is still lexical BM25 × salience — not embeddings. Salience is
computed at query time by a custom collector (WEG-43) that reads the FastFields
and combines BM25 × `exp(-age/14) × ...` — no indexed score, no nightly
re-rank (see `ARCHITECTURE.md` load-bearing decision #2).

The per-project index manifest at `<project>/.agent/.dreamd/index_manifest.json`
— the path is `AgentRoot::dreamd_dir()`, i.e. `.dreamd` under the project's
`.agent/` directory (`layout.rs`) — carries the schema version defined
by `dreamd-core::index::SCHEMA_VERSION`
(`index.rs` — `index/1.3` at time of writing, pinned by a guard test in the
same file). Read the constant, not this sentence: the value bumps
whenever the schema changes, and any copy of it in prose goes stale on the
next bump. `TantivyIndexHandle::open` writes the manifest on first index init
and checks it against the binary on every open. WEG-24-A binds:
never `rm` `.tantivy-writer.lock` / `.tantivy-meta.lock` — kernel handles
advisory flock.

On open, `<project>/.agent/.dreamd/index_manifest.json` is read
via `dreamd-core::index::check_manifest_version`. A missing manifest is
a pre-index state (`ManifestCheckOutcome::Absent`); the open writes one.

A manifest version **older** than the binary (`NeedsMigration`) does not wait
for a migration tool and does not merely warn — `TantivyIndexHandle::open`
**rebuilds the derived index in place** (`server/tantivy_handle.rs`).
It logs `"index schema outdated; rebuilding from JSONL"`, removes the index
directory, the manifest, **and the progress watermark** — resetting the
watermark is what forces a *full* replay rather than a tail replay — then
re-indexes through `replay_two_pass`. The framing comment above that match
gives the reason in one line: **the index is a rebuildable cache.**
No `dreamd migrate` step is involved in this path. The same wipe-and-rebuild
runs when Tantivy rejects the on-disk index with a schema error
(`IndexError::SchemaIncompatible`, the only variant that gates a wipe).

A manifest *newer* than the binary (`ManifestVersionError::TooNew`) makes
`open` return `IndexError::Other("index schema … is newer than binary …")`,
which `dreamd watch` surfaces as a startup failure — a hard error, not a
rebuild. The user must upgrade dreamd, or wipe the index directory to rebuild
under the older binary.

The scope of that self-heal is worth stating precisely, because the two halves
of schema versioning are governed by different rules.
[`../ARCHITECTURE.md` §4](../ARCHITECTURE.md#4-index-freshness-vs-jsonl-durability-v01-contract)
covers the **derived index cache**, which self-heals as described above.
[`../ARCHITECTURE.md` §7](../ARCHITECTURE.md#7-schema-versioning) covers the
**durable** store: every persisted episodic record carries
`schema_version: "1.0.0"` and daemon `state.json` carries
`schema_version: "1.0"`, on independent version streams, and a `dreamd migrate`
path must exist before either version changes. `dreamd migrate --from 1.0.0 --to 1.0.0`
is the shipped no-op stub (episodic `RECORD_SCHEMA_VERSION` only). It is the
durable store's problem, not the index's — do not cite it as a prerequisite for
an index schema bump.

### Query-time salience

Salience is **not stored** — it is recomputed per hit at query time
(`salience.rs` module doc), which is exactly what lets the index stay static and
removes any need for a nightly re-index pass. The formula
(`salience::salience`) is locked by `ARCHITECTURE.md`
[decision #2](../ARCHITECTURE.md#2-salience-is-query-time-not-indexed) and PRD
FR-4.2:

```rust
(-age_days / 14.0).exp() * (pain / 10.0) * (importance / 10.0) * (1.0 + (1.0 + recurrence as f64).ln())
```

Read left to right: exponential recency decay (a 14-day e-folding constant —
see [Decay](#decay); **not** a half-life), times normalised pain, times
normalised importance, times a logarithmic recurrence boost.

The ranking score is **BM25 × salience — a product, not a weighted sum**
(`collector.rs` module doc). `RecallResult` carries `bm25` (the raw,
pre-multiply score) and `salience` as separate fields, so `dreamd recall
--explain` can show both factors instead of one blended number.

**Property tests (WEG-47).** Monotonicity and finiteness invariants for the
formula above are enforced by a `proptest` suite in
`crates/dreamd-core/tests/salience_proptest.rs` (512 cases by default;
nightly CI sets `PROPTEST_CASES=1024`). Valid ranges: `age_days` ∈ \[0, 10000\],
`pain` / `importance` ∈ \[0, 10\], `recurrence` ∈ \[0, `u64::MAX`/2\]. Edge
cases: `pain=0` or `importance=0` → score 0; `recurrence=0` → ln factor 1.

Full derivation and lineage: [`salience.md`](salience.md).

### Read-after-write visibility (commit-cadence window)

The indexer commits to Tantivy on a wall-clock cadence
(`DEFAULT_COMMIT_CADENCE`, default 5 seconds — `server/tantivy_handle.rs`). A
document appended via `POST /api/v1/learn` at T+0 is **not** searchable until
the next commit lands — worst-case T+5s.

This is a *freshness* constraint, not a *latency* constraint. The
`<5ms P50 warm` recall latency applies to the query operation itself and
is unaffected by the commit cadence. The two must not be conflated in
public copy or benchmark commentary. To re-run the Criterion recall bench at
n=1k/10k/100k see [`benchmarks.md`](benchmarks.md); the last recorded values
are in [`../PERF.md`](../PERF.md).

A lower cadence (toward 1s) would buy sub-5s freshness at the cost of higher
I/O, but there is no user-facing cadence setting (DR-307 / WEG-140, not
shipped): the value is a constructor argument, and every production caller
passes `DEFAULT_COMMIT_CADENCE`.

For the forward-looking plan across a future Tantivy major bump — how each schema
field is consumed, the custom collector's Tantivy-internal assumptions, and what a
maintainer must re-verify before bumping — see
[`architecture/tantivy-migration.md`](architecture/tantivy-migration.md).

## Dream cycle

`dream_cycle::run_guarded_cycle` is the one sequencer (BZR-172); both triggers
are thin adapters over it. It runs the 409 in-progress guard, captures the
dirty-tree state, dispatches the filesystem phases (`run_filesystem_phases`),
then runs the index and autobiography phases (`run_post_phases`). `now_sec` is
caller-provided rather than read from the wall clock, so the deterministic path
is reproducible from its inputs.

Phase order is fixed (`dream_cycle.rs` module doc): **consolidation → decay →
index → autobiography.**

**The lesson body has two producers.** Cluster selection, the exemplar, the
pins, and the WAL intent are the same either way; only the prose, the
`citations` list, and the frontmatter `prompt_version` differ
(`consolidation::LessonBodySource`):

- **Deterministic** — the exemplar event's own `content`, with
  `prompt_version: "deterministic-only"` (`DETERMINISTIC_PROMPT_ID`). No network
  call. This is what `--no-llm` (or `x-dreamd-no-llm: 1`) forces, and what runs
  when no API key resolves.
- **LLM-composed** (since v0.1.1; taken only when an API key resolves) —
  `dream_cycle::compose_lesson_body` builds a prompt from the promoted cluster,
  prices it against `cost_cap_usd`
  ([`llm-cost-accuracy.md`](llm-cost-accuracy.md)), and awaits the model
  **inside** the WAL envelope. The completion must end in a `citations:`
  trailer naming only events in the promoted cluster. `prompt_version` is then
  `llm::VERSIONED_PROMPT_ID`. Composition is infallible by construction: a
  missing key, an over-cap estimate, an unpriced model, a transport failure,
  an empty answer, or an invalid citation trailer each fall back to the
  deterministic body. `.agent/personal/` reaches the prompt only with
  per-call consent (`--share-personal` / `x-dreamd-share-personal: 1`).

`consolidation::run_deterministic_dream_cycle` still exists as the
deterministic-only pairing of select-then-write; the production path is
`run_filesystem_phases`.

**Triggers: there are exactly two, and both are manual.** The CLI `dreamd
dream` (`Command::Dream` in `cli.rs`) and `POST /api/v1/dream`
(`server/http/router.rs`). **There is no scheduler** — nothing runs unless a
user or an agent asks for it, so do not read "nightly cycle" into any part of
this design. `--auto` is a hidden flag that exits 2. Auto mode is not
supported. `--dry` previews: it prints the `LESSONS.md` the cycle
would write and writes nothing, skipping the daemon proxy because a
`POST /api/v1/dream` is itself a write (AILAB-341). `--no-llm` is forwarded
through the proxy as a header. `--no-commit` skips the git step **and** the
daemon proxy: the cycle runs in-process, so do not use it for a project a
running `dreamd watch` is serving — it rewrites the JSONL under the daemon,
and its index phase fails on the writer lock the daemon holds.

A real cycle also does two things that are not phases of their own, both
inside the WAL envelope: `snapshot::create_autosnap` saves a pre-mutation copy
of the live files under `.dreamd/branches/` (a `snap-*` ref; see
[`branching.md`](branching.md)), and `provenance::record_cycle` appends
`lesson_citation` / `recurrence` edges to `.dreamd/provenance/ledger.jsonl`
(see [`provenance.md`](provenance.md)).

### Clustering: a prefix tree over `skill_action`

`run_cluster_engine(agent_root, now_sec) -> Result<ClusterOutput, ConsolidationError>`
(`consolidation.rs`) builds a **prefix tree** over each event's
`skill_action`, split on `::`. Every event contributes a count to *every*
prefix of its own key: `split("::")`, then `parts[..len].join("::")` for `len`
in `1..=parts.len()` (`build_prefix_tree`). An event keyed
`a::b::c` therefore counts toward `a`, `a::b`, and `a::b::c`.

Assignment is **deepest-wins**: prefixes are sorted longest-first and each
event is claimed by the deepest qualifying cluster through a `Vacant` entry
guard (`compute_promoted_clusters`, which `dreamd doctor --cluster-health` and
`dreamd dream --dry` share with the write path). Each event belongs to exactly
**one** cluster. Counts fan out across the tree; membership does not.

The output is `ClusterOutput { promoted: Vec<PromotedCluster> }`, where
`PromotedCluster { cluster_key, events, salience_sum }`.

### Recurrence and promotion

A cluster qualifies for promotion when it holds at least
`PROMOTION_THRESHOLD = 3` events in **either** the 7-day **or** the 30-day
trailing window (`WINDOW_7_DAYS_SEC`, `WINDOW_30_DAYS_SEC`; the OR-logic is in
`compute_promoted_clusters`). Either window alone is
sufficient — this catches both a burst inside a week and a slow drip across a
month. The counts are written to `semantic/recurrence_counts.json` as
`RecurrenceSidecar { schema_version: "1.0", clusters }` (`run_cluster_engine`).

Of the qualifying clusters, only the **single top cluster by `salience_sum`**
is promoted to `LESSONS.md`, and it contributes exactly **one** `Lesson`, filed
under the exemplar's id (`select_lesson_or_retire`). `pick_exemplar` ranks
salience → pain → importance → lowest `EventId`. A cycle is therefore a narrow
instrument: at most one lesson per run, not a batch summarisation pass.

A cycle that promotes **no** cluster **unlinks** `semantic/LESSONS.md` if it is
present (`select_lesson_or_retire`, `consolidation.rs`) rather than
writing an empty file — the never-dreamed state and the retired state are both
"the file is absent". Retirement is therefore structural, not scheduled: a
cluster that stops recurring simply fails the window test on the next cycle,
drops out of the file, and the post-cycle semantic pass turns that absence into
`delete_term(layer="semantic")` and a commit (`index_semantic_lessons`,
`server/indexer_actor.rs`), so recall stops serving the stale lesson without a
daemon restart. There is no expiry timer, no age filter, and no read-time
staleness check — one cycle run past the trailing window is the whole
mechanism. A malformed `LESSONS.md` is the deliberate exception: it leaves the
index untouched, because a corrupt file must never wipe the semantic layer. The
cited exemplars **stay pinned** — retirement removes the derived document, not
the source events.

Every pass that resolves lessons records the ones it could **not** index — a
lesson whose exemplar is no longer in the episodic log is skipped rather than
indexed at score 0.0 — to `<project>/.agent/.dreamd/semantic_pass.json`
(`SemanticPassRecord`, `server/indexer_actor.rs`). `dreamd doctor` reads that
sidecar and names the skipped lessons, so a silently un-recallable lesson is
visible without the daemon running and without doctor re-parsing `LESSONS.md`
(AILAB-700). The malformed case writes nothing: the index was untouched, so the
previous record still describes it.

Pins are **unioned, never unset**. `apply_pin_unpin` computes
`event.pinned = event.pinned || cited_ids.contains(...)`, so a cycle
can pin an event it cites but can never clear a pin it did not set
(`consolidation.rs` module doc; SPEC §67 / WEG-426). "Cited" is every
`Lesson.id` plus every id in the file-level `citations` frontmatter
(AILAB-200). `dreamd archive --force-unpin` is the operator's way to clear a
pin.

### Decay

Two distinct mechanisms share the word "decay". Keep them apart in prose and in
your head:

- The **recency term inside the salience formula**, `(-age_days / 14.0).exp()`
  (`salience::salience`), is a 14-day **e-folding time constant**: salience falls
  to `1/e` of its starting value after 14 days. It is **not** a half-life —
  the half-life of that curve is ~9.7 days, and calling it one is a factual
  error. It is evaluated at query time and never moves a record on disk.
- The **decay pruner** (`decay.rs`) is what actually archives records off the
  hot path. Its effective gate is **`!pinned && age > 90d`**
  (`DECAY_AGE_THRESHOLD_SEC`, `decay.rs`) — age alone, not age plus
  interest. Pinned records are **never** archived (the first check in
  `should_decay`). Archived records are appended to
  `.agent/.dreamd/snapshots/<YYYY-MM-DD>.jsonl`, never deleted.
- `DECAY_SALIENCE_THRESHOLD = 2.0` is evaluated in `should_decay`
  but **cannot currently discriminate**. The pruner scores
  through `RecurrenceContext::decay()`, hardcoded to recurrence `0`
  (`salience.rs`), so past 90 days the recency term alone caps
  salience at `exp(-90/14) ≈ 0.0016` — below `2.0` even at maximum pain and
  importance. Read the constant as a **defensive floor**, not a live filter:
  it is retained so a future change to decay-phase recurrence cannot silently
  start archiving high-salience records.

### Autobiography

The final phase git-commits through `git2` into the user's **outer project
repo**. It stages exactly
`TRACKED_PATHS = [".agent/semantic/LESSONS.md", ".agent/episodic/AGENT_LEARNINGS.jsonl"]`
(`autobiography.rs`), commits under the fixed identity
`dreamd <noreply@dreamd.dev>` with the message shape
`dreamd: cycle YYYY-MM-DD`, and reports
`Committed(Oid)` / `NoRepo` / `Skipped(SkipReason)` (`AutobiographyOutcome`).

The phase is **best-effort**: a failure logs at ERROR and the cycle still
succeeds (`dream_cycle::run_post_phases`). The memory is the JSONL and `LESSONS.md`;
the commit is a convenience layered on top of them.

One sharp edge: the dirty-tree check fires at **cycle start, not commit time**
(`autobiography.rs` module doc). If the tracked files are dirty when the cycle
begins, the cycle **still runs and overwrites those edits** — only the commit
is skipped, with a WARN, and the skip is recorded in `state.json`
(`last_autobiography_skip`), which `dreamd doctor` prints. The check is not a guard on hand-edits to
`LESSONS.md`; it only decides whether to commit.

## WAL / crash recovery

The dream cycle's filesystem phases run inside a write-ahead log at
`.agent/.dreamd/dream_in_progress.wal`, resolved through
`agent_root.wal_path()` (`wal.rs`, `layout.rs`).

The file holds `DreamWal { schema_version, intents: Vec<WalIntent> }`
as pretty JSON, where `WalIntent` is
`ReplaceSemanticMemory { temp_file_path }` | `PruneEpisodicMemory { temp_file_path }`
| `Commit`. Its `STATE_SCHEMA_VERSION = "1.0"` (defined in `daemon_state.rs`,
re-exported from `wal.rs`) is versioned
**independently** of the episodic record schema — see
[`../ARCHITECTURE.md` §7](../ARCHITECTURE.md#7-schema-versioning).

Lifecycle:

- `begin_cycle` writes a fresh WAL with no intents and moves `state.json` to
  `in_progress`. It refuses when `.agent/` does not exist rather than create a
  store.
- `guarded_replace` is the one destructive-replace seam (AILAB-164): it writes
  and fsyncs the temp file, appends the intent naming **that** temp
  (`append_intent` rewrites the whole WAL file atomically), then renames.
  Callers pick a `GuardedReplaceKind` and never build a `temp_file_path`
  themselves. The `LESSONS.md` write, the pin rewrite, and the decay prune
  (the latter two via `episodic::rewrite_guarded`) all go through it.
- `commit_cycle` appends `Commit`, moves state to `complete`, and deletes the
  WAL.

`state.json` is read and patched through the typed
`daemon_state::{read_daemon_state, update_daemon_state}` pair (AILAB-167), so a
status change does not wipe `last_dream_cycle_at` or the autobiography skip
marker.

**One WAL envelope spans both filesystem phases (ARCH-2).**
`dream_cycle::filesystem_phases` is `begin_cycle → autosnap → consolidation →
decay → provenance record → commit_cycle` — a single envelope, not one per
phase. A phase
error short-circuits through `?` *before* `commit_cycle`, leaving an
uncommitted WAL for next-startup recovery. The regression test
`seam_crash_after_consolidation_recovers_not_clean` (`dream_cycle.rs`)
pins this: under the previous two-envelope shape, a crash between consolidation
and decay let a half-finished cycle be silently recorded as a success.

**Recovery.** `wal::recover_if_needed(agent_root, now_sec)` returns
`RecoveryOutcome::{Clean, Recovered { cleaned_files }, CommittedButUnclean}`.
A WAL containing `Commit` means the cycle finished and only the
file outlived it: state goes to `complete`, the WAL is deleted, and the outcome
is `CommittedButUnclean`. Otherwise each intent's temp file and
its `.tmp` neighbour are deleted, the WAL is deleted, state is set to
**`failed`**, and the outcome is `Recovered`.
`recover_on_startup` runs from `run_watch` and from the lazy
per-project open, before any store access. A concurrent trigger is refused
rather than queued: `dream_cycle::ensure_not_in_progress` yields
`DreamCycleError::InProgress` when `state.json` says `in_progress`, which the
HTTP layer maps to 409 and the in-process CLI reports as an error (both paths
share the guard since BZR-172). Recovery keys off the WAL **file**: with no WAL
on disk it returns `Clean` and does not rewrite the status, so a `state.json`
stuck at `in_progress` with no WAL is an operator cleanup, not something a
`dreamd watch` restart clears.

**Two carve-outs that must not be overstated:**

1. Recovery is **compensating cleanup — a roll-back of temp files, not a
   roll-forward.** A recovered cycle lands in `failed`, not `complete`. Nothing
   replays the lost work; the next cycle starts over from the durable store.
2. **Tantivy index mutations are not WAL-protected** (`wal.rs` module doc;
   `ARCHITECTURE.md` §3). The "a reader never sees a mid-cycle store"
   invariant holds for the **durable** store — the JSONL, `LESSONS.md`, and the
   recurrence sidecar — **not** for the derived index. The index is a
   rebuildable cache (see [Indexing and salience](#indexing-and-salience)) and
   is allowed to lag or be rebuilt outright.

Deep dives: [`architecture/durability.md`](architecture/durability.md) for the
full durability argument, and
[`../ARCHITECTURE.md` §3 "Dream cycle WAL"](../ARCHITECTURE.md#3-dream-cycle-wal)
for the mermaid sequence diagram and the scope line. This section
summarises both rather than restating them.

## API stability

**`/api/v1` is the HTTP surface that shipped in 1.0.0.** That release did not
publish a separate stability guarantee for it. Request shapes, response shapes,
and status codes can still change; each breaking change is called out in
[`CHANGELOG.md`](../CHANGELOG.md). Pin an exact version and read the changelog
before upgrading. The endpoint list — `learn`, `recall`, `preferences`,
`health`, `dream`, `observability/salience`, and the `migrate` 501 stub — lives
in [`http-api.md`](http-api.md) and is not duplicated here.

**The on-disk contract is the other promise.**
[`../SPEC.md`](../SPEC.md) v0.1 freezes folder layout, the episodic node schema
(`schema_version` `"1.0.0"`), the salience formula, and the dream-cycle output
shape. Breaking any of those requires SPEC v0.2+ and a documented migration
path (`SPEC.md` §Versioning). That freeze says nothing about the HTTP API; SPEC
places transport (stdio, HTTP, Unix socket, MCP) out of scope. The durable
artifact is the contract; the socket is an implementation detail of one reader
of it.

The practical consequence: a consumer that reads `.agent/` off disk is
insulated from everything this section permits. A consumer that speaks HTTP is
not.

## Threat model

The trust boundary is the local machine and the invoking uid. On Unix the API
is served over a Unix domain socket created with `0600` permissions
(`chmod_0600` in `server/uds.rs`), and `peer_uid_middleware` — the outermost
router layer (`build_router` in `server/http/router.rs`, WEG-72 / DR-407) —
rejects with 403 any peer whose uid does not match the daemon owner's,
including a peer whose uid cannot be determined at all. The uid is read via
`SO_PEERCRED` on Linux and `getpeereid` on macOS. **Unix `dreamd watch` has no
TCP listener** (`--bind` / `--insecure` exit 2 there). Two listeners are TCP,
and both default to loopback and refuse a non-loopback address without
`--insecure`: Windows `dreamd watch` (`127.0.0.1` on an ephemeral port, with a
bearer token from `~/.agent/auth.json` in the slot peer-UID holds on Unix), and
the opt-in `dreamd mcp --bind` Streamable HTTP server (non-default `mcp-http`
feature, unauthenticated). See `ARCHITECTURE.md` §5. That is the whole posture
at this altitude. The canonical
threat model, the socket-auth middleware detail, and the disclosure policy live
in [`../SECURITY.md`](../SECURITY.md). Do not re-derive the threat model here;
link it.

## Writer-process lifecycle (WEG-21 / DR-118)

`server::Supervisor` owns the `MemoryCoordinator` task and the canonical
`mpsc::Sender` into it. Per the 2026-05-14 PM+architect decision (option a),
`MemoryCoordinator::run()` is NOT modified — the shutdown-drain invariant
lives entirely in the lifecycle layer:

1. Drop every issued sender clone (per-connection client tasks finish first).
2. Send `Shutdown { response_tx }` over the supervisor's retained `tx`.
3. The coordinator handles every message queued **ahead of** `Shutdown`
   because the actor loop reads in FIFO order from a bounded channel.
4. `Shutdown` is terminal: the arm closes the receiver and discards anything
   queued **behind** it, so a late sender gets a dropped response channel or a
   typed `mpsc::error::SendError` (wrapped here as `SupervisorSendError`) —
   never a false success. The supervisor unit tests assert both halves of
   this contract.

That is `Supervisor::shutdown(self)`, which tests use. The daemon cannot take
that path — `run_watch` only holds an `Arc<Supervisor>` that live connection
tasks may still share — so production shutdown (SIGINT / SIGTERM) is
`drain_daemon` in `server/watch.rs` (AILAB-162): unlink the socket first, then
`Supervisor::drain(&self)` (the same `Shutdown` message, without step 1 and
without joining the task), then shut down or flush the primary index so the
last uncommitted batch lands. The whole drain is bounded by `DRAIN_TIMEOUT`
(30 s, AILAB-748). A drain that times out can skip the final index commit,
which the next open's replay heals; it does not undo a JSONL line that was
already acknowledged.

`detach_double_fork()` performs the canonical `fork → setsid → fork` daemon
dance. The first fork escapes the parent's process group; `setsid` makes
the child a session leader; the second fork ensures the final process is
not a session leader and therefore can never acquire a controlling
terminal. The session-leader process exits, leaving the kernel to reap the
final detached child via the grandparent (init / launchd / systemd).
**Not called in production** — `dreamd watch` runs in the foreground, and
`dreamd service install` (systemd `--user` unit, AILAB-190; macOS LaunchAgent,
AILAB-169; Windows logon scheduled task, AILAB-203) supervises that foreground
`watch` rather than calling this helper (ARCHITECTURE.md §8.1).

## API backpressure

The coordinator channel is bounded at `COORDINATOR_CHANNEL_CAPACITY = 256` (constant in
`dreamd-core::server::lifecycle`). This is a round number chosen before load testing
and not re-tuned since.

All API handlers must call `Supervisor::try_send` rather than cloning the raw `tx` via
`sender()`. `try_send` applies a 100ms `COORDINATOR_SEND_TIMEOUT`; on expiry it returns
`CoordinatorSendError::Full`, which the handlers map to HTTP 503 +
`Retry-After: 1` with the body `{"error":"coordinator busy, retry"}`. On
`CoordinatorSendError::Closed` the daemon is shutting down; the response is 500
`"coordinator unavailable"` with no retry header. The timeout covers only the
enqueue: once a message is in the inbox, the handler waits for the reply with no
deadline.

**Why a method, not `sender()`:** HTTP handlers reach the supervisor through
`AppState`; routing them through `try_send` prevents bypassing the timeout contract
and keeps the shed-load enforcement point singular. `sender()` hands out a raw
clone for callers that own their lifetime — the in-process MCP store
(`memory_store::InProcessStore`) sends on one with a plain awaited `send` and no
timeout, so in-process appends wait rather than shed.

**Dream cycle routing:** the cycle's filesystem phases run **through** the coordinator,
not beside it. `POST /api/v1/dream` sends `MemoryCoordinatorMsg::RunDreamCycle`
over the same mpsc channel as appends; the actor loop dispatches it
to `handle_run_dream_cycle`, which calls `run_filesystem_phases`
(consolidation → decay, inside one WAL envelope, including any LLM call).
`MemoryCoordinatorMsg::Forget` follows the same pattern for `forget::cascade`.

The reason is fd ownership (WEG-271). Both consolidation and decay can replace
`AGENT_LEARNINGS.jsonl` by atomic rename, which orphans the coordinator's long-lived
`File` — subsequent appends would land on an unlinked inode and be silently lost. Routing
the cycle through the actor lets the handler reopen `self.file` and seek to the end
as part of the same message, so there is no window in which the coordinator
holds a stale fd. `now_sec` / `cycle_date` are supplied by the caller so the cycle stays
wall-clock-free and deterministic in tests (see the `RunDreamCycle` variant doc).

Two consequences follow, and both are load-bearing:

- Appends and a cycle's filesystem phases **serialise in the same FIFO actor loop** — a
  running cycle occupies the coordinator for its duration, model call included.
  Backpressure therefore applies
  to `dream` exactly as it does to `learn`: the handler goes through `try_send`, and a
  full channel returns 503 + `Retry-After: 1` (`handlers/dream.rs`).
- The cycle must be routed to the coordinator that **owns** the target project root, not
  the boot coordinator, or project B's cycle would prune project A's JSONL
  (`state.resolve_supervisor` in `handlers/dream.rs`; WEG-272).

Only the post-filesystem phases leave the coordinator. Index work is handed to the
indexer task over its own bounded channel (`DEFAULT_INDEXER_CHANNEL_CAPACITY = 1024`,
`server/indexer_actor.rs`), and the autobiography commit runs after it in `run_post_phases`
(`dream_cycle.rs`) — outside the coordinator and best-effort. It is not, however,
insulated from appends: the indexer is a single task, so an `ApplyRecurrenceSidecar`,
`IndexSemanticLessons`, or `PruneDecayedEvents` pass occupies it while appends queue, and
an append that finds the 1024-slot channel full now parks the coordinator on its awaited
`send` until the indexer takes the message (`coordinator.rs::route_to_indexer`).

## Observability (WEG-32 / DR-004)

`dreamd_core::observability::init_tracing` installs the process-wide `tracing`
subscriber exactly once, at the top of `cli::run()` before subcommand dispatch.
The `tracing` facade and its macro callsites already exist throughout the crate;
this baseline is what makes them emit. No per-subcommand init. Per-request
enrichment is the HTTP layer's job: `build_router` opens one INFO `http` span
per request and emits one `http request` event into it, with a generated
`x-request-id` (AILAB-189; see [`observability.md`](observability.md)).

Two layers:

- **Console → stderr, always.** stdout is reserved for the MCP JSON-RPC channel
  (`rmcp::transport::stdio`), so logs must never write to it. Format is
  TTY-conditional: pretty human-readable text when stderr is a terminal, JSON
  when it is not (CI, service-managed daemon). Detection uses
  `std::io::IsTerminal` — no `atty` crate.
- **File → `~/.agent/dreamd.log`, JSON always — daemon only.** Only `dreamd
  watch`, the long-running daemon, installs this layer; every other invocation
  (`init`, `status`, `doctor`, `dream`, `reset`, `mcp`, bare `--version`, …)
  passes `None` and is console-only on stderr. The gate is
  `cli::wants_daemon_log` (AILAB-184). The reason is the next sentence: the
  daemon's file is **truncated at startup** — there is no log rotation — and it
  is created as mode `0o666` before the process umask, so a typical umask of
  `022` leaves it `0644`. The socket is `0600`. A short-lived one-shot that opened the same path would erase a
  running daemon's accumulated log while writing nothing of its own into it.
  `mcp` is long-running but excluded too: IDEs spawn several concurrently, and
  they would truncate each other. Written through a non-blocking appender
  (`tracing-appender`); the returned `WorkerGuard` is bound as `_log_guard` in
  `run()` and held until the process exits, since dropping it early discards
  buffered file logs. The path is resolved via `DaemonHome::log_file()`, never
  hardcoded.

Level comes from the `DREAMD_LOG` env var (`error|warn|info|debug|trace`,
standard `EnvFilter` syntax), defaulting to `info`. `DREAMD_LOG` is owned here
(DR-004 / WEG-32), not by the config loader.

If the log directory is not writable, `init_tracing` degrades to console-only
and returns `None` rather than failing. `try_init` makes the call idempotent.

## Related docs

- [`../ARCHITECTURE.md`](../ARCHITECTURE.md) — load-bearing engineering
  decisions and crate boundaries. §2 salience, §3 the WAL sequence diagram, §4
  the index-cache contract, §7 the durable schema-version streams.
- [`../SPEC.md`](../SPEC.md) — the on-disk `.agent/` contract: folder layout,
  episodic node schema, salience formula, dream-cycle output shape.
- [`spec/README.md`](spec/README.md) — two-page digest of that contract for
  adapter authors (layout; record and `LESSONS.md` schemas).
- [`../SECURITY.md`](../SECURITY.md) — canonical threat model, socket auth,
  disclosure policy.
- [`architecture/durability.md`](architecture/durability.md) — WAL + JSONL
  durability deep-dive.
- [`architecture/tantivy-migration.md`](architecture/tantivy-migration.md) —
  what a maintainer must re-verify before a Tantivy major bump.
- [`salience.md`](salience.md) — the salience ranking formula, factor by factor.
- [`http-api.md`](http-api.md) — `/api/v1/*` endpoints, headers, status codes.
- [`llm-cost-accuracy.md`](llm-cost-accuracy.md) — the dream cycle's LLM cost cap.
- [`branching.md`](branching.md) / [`provenance.md`](provenance.md) — the
  `.dreamd/branches/` and `.dreamd/provenance/` formats.
- [`observability.md`](observability.md) — request tracing, recall citations,
  the salience report.
- [`compared.md`](compared.md) — comparison vs Mem0 / Letta Code /
  MCP-ref / Cline Memory Bank.

`PRD`, `DR-`, `WEG-`, `AILAB-`, and `BZR-` identifiers in this document refer
to planning notes and tickets that are not part of the public repository.
