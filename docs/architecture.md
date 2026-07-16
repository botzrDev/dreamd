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
4. [Dream cycle](#dream-cycle) — deterministic consolidation, decay,
   autobiography, and the two manual triggers.
5. [WAL / crash recovery](#wal--crash-recovery) — `dream_in_progress.wal`, and
   what "never mid-cycle" does and does not cover.
6. [API stability](#api-stability) — what v0.1 freezes and what it does not.
7. [Threat model](#threat-model) — the trust boundary, in one paragraph.

Supporting detail, after the spine:

8. [Writer-process lifecycle](#writer-process-lifecycle-weg-21--dr-118)
9. [API backpressure](#api-backpressure)
10. [Observability](#observability-weg-32--dr-004)
11. [Related docs](#related-docs)

## Crate-split tripwires

The workspace currently has three crates:

- `dreamd-protocol` — pure serde types (DR-102 / WEG-6).
- `dreamd-core` — `layout`, `privacy`, `lessons`, `io`, `coordinator`.
- `dreamd` (the CLI crate, package name `dreamd`, dir `dreamd-cli`).

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
  - `crates/dreamd-core/src/server/index_map.rs` — `IndexHandle` trait
    skeleton + `ProjectIndexMap<H>` (LRU cap 10, idle eviction 30 min) with
    `TestIndexHandle`. The Tantivy-backed `TantivyIndexHandle` lands in
    WEG-42 alongside the index writer.

  **Re-evaluation triggers** (when, not if, to revisit the extraction):
    1. WEG-42 lands the Tantivy dep and dreamd-core's compile time crosses
       an empirical threshold (rule of thumb: >2× current cold compile).
    2. A second Rust binary consumer needs the server stack without
       pulling the index/dream/io modules along with it.

  Until one of those fires, the single-crate layout keeps the `cargo test
  -p dreamd-core` story simple and the supervisor adjacent to the
  coordinator it manages.

  Lint override: `dreamd-core/Cargo.toml` locally downgrades
  `unsafe_code` from `forbid` (workspace) to `deny`. The sole authorised
  callsite is `server::lifecycle::detach_double_fork`, which calls
  `nix::unistd::fork` (an `unsafe fn`). **Zero production call sites in
  v0.1** — the helper is reserved for v0.1.1 `dreamd service` install
  (see `ARCHITECTURE.md` §8.1). All other modules in the crate still
  surface unsafe usage at compile time.

- **`dreamd-layout`** splits out when (and only when) a no-tokio consumer
  of layout appears. Today every layout caller already pulls in tokio
  indirectly via the coordinator; a split would create an empty re-export
  crate. The DR-002 plan anticipates a `dreamd-store` split too — defer
  that until the index + WAL code actually lands and a clear seam exists.

Premature splits are reversible but costly: each split adds a `version =`
bump, a Cargo path entry, and a CI matrix dimension. The default answer to
"should this be a new crate?" is **no, until a tripwire fires.**

## Actor model: `MemoryCoordinator` is the serialization point

State management is an actor model. A single `MemoryCoordinator` tokio task
owns the mutable handle to `AGENT_LEARNINGS.jsonl`. API handlers, the file
watcher, and the dream-cycle pipeline send intents over a
`tokio::sync::mpsc` channel; only the coordinator writes.

The coordinator does **not** wrap its `File` in a `Mutex`. The `&mut self`
on `MemoryCoordinator::run` is the exclusivity guarantee — there is no other
handle to the file, so there is nothing to lock against. DR-103 specifies a
"`tokio::sync::Mutex<File>` owned by the coordinator" as a logical contract;
the implementation realises that contract by making the coordinator the sole
owner of an unwrapped `File`. The serialization is structural, not
lock-based.

Durability path for `AppendLearning`:

1. `serde_json::to_string(&learning)` — produce a single JSON line.
2. Ensure a trailing `\n`.
3. `file.write_all(...)` — returns only once the complete prepared line has been
   written, or errors. It is not a single-syscall guarantee: `write_all` loops
   over the underlying `write` until the buffer is drained, so a line may reach
   the file through more than one write.
4. `file.sync_data()` — fdatasync to disk.
5. Send `Ok(())` over the oneshot.

The `POST /api/v1/learn` 201 response must not return until step 5 fires.
Concurrent third-party writers to the JSONL are explicitly out of scope for
v0.1 despite the looser language in PRD FR-1.2.

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

The message enum `MemoryCoordinatorMsg` is `#[non_exhaustive]` to keep WEG-7
(idempotency + ULID), WEG-50 (dream-cycle trigger), and later additions
non-breaking for downstream `match` consumers.

## Indexing and salience

Tantivy 0.26.1 backs episodic recall. The schema is defined in
`dreamd-core::index::build_schema()` — one TEXT field (`content`), six
u64/f64 FastFields for the salience inputs, plus three forward-compatible
fields (`layer`, `last_updated_sec`, `cited_event_count`) reserved for
the v0.1.1 LESSONS.md indexing pipeline (WEG-136). Salience is computed
at query time by a custom collector (WEG-43) that reads the FastFields
and combines BM25 × `exp(-age/14) × ...` — no indexed score, no nightly
re-rank (see CLAUDE.md load-bearing decision #2).

The per-project index manifest at `<project>/.agent/.dreamd/index_manifest.json`
— the path is `AgentRoot::dreamd_dir()`, i.e. `.dreamd` under the project's
`.agent/` directory (`layout.rs:106-108`) — carries the schema version defined
by `dreamd-core::index::SCHEMA_VERSION`
(`index.rs:24` — `index/1.3` at time of writing, pinned by a guard test at
`index.rs:420-424`). Read the constant, not this sentence: the value bumps
whenever the schema changes, and any copy of it in prose goes stale on the
next bump. WEG-42 writes the manifest on first index init; WEG-49 enforces
`binary.expected == manifest.version` on daemon startup. WEG-24-A binds:
never `rm` `.tantivy-writer.lock` / `.tantivy-meta.lock` — kernel handles
advisory flock.

On startup, the daemon reads `<project>/.agent/.dreamd/index_manifest.json`
via `dreamd-core::index::check_manifest_version`. A missing manifest is
a pre-index state and logs a warning.

A manifest version **older** than the binary (`NeedsMigration`) does not wait
for a migration tool and does not merely warn — `TantivyIndexHandle::open`
**rebuilds the derived index in place** (`server/tantivy_handle.rs:226-238`).
It logs `"index schema outdated; rebuilding from JSONL"`, removes the index
directory, the manifest, **and the progress watermark** — resetting the
watermark is what forces a *full* replay rather than a tail replay — then
re-indexes through `replay_two_pass` (`:257`). The framing comment at
`:224-225` gives the reason in one line: **the index is a rebuildable cache.**
No `dreamd migrate` step is involved in this path.

A manifest *newer* than the binary aborts startup with
`ServerError::ManifestCheck(ManifestVersionError::TooNew)` (`:239-244`) — a
hard error, not a rebuild. The user must downgrade or migrate.

The scope of that self-heal is worth stating precisely, because the two halves
of schema versioning are governed by different rules.
[`../ARCHITECTURE.md` §4](../ARCHITECTURE.md#4-index-freshness-vs-jsonl-durability-v01-contract)
covers the **derived index cache**, which self-heals as described above.
[`../ARCHITECTURE.md` §7](../ARCHITECTURE.md#7-schema-versioning) covers the
**durable** store: every persisted episodic record carries
`schema_version: "1.0.0"` and daemon `state.json` carries
`schema_version: "1.0"`, on independent version streams, and a `dreamd migrate`
path must exist before either version changes. `dreamd migrate` is
unimplemented (DR-108 / v0.1.1). It is the durable store's problem, not the
index's — do not cite it as a prerequisite for an index schema bump.

### Query-time salience

Salience is **not stored** — it is recomputed per hit at query time
(`salience.rs:5-7`), which is exactly what lets the index stay static and
removes any need for a nightly re-index pass. The formula
(`salience.rs:84-90`) is locked by `ARCHITECTURE.md`
[decision #2](../ARCHITECTURE.md#2-salience-is-query-time-not-indexed) and PRD
FR-4.2:

```rust
(-age_days / 14.0).exp() * (pain / 10.0) * (importance / 10.0) * (1.0 + (1.0 + recurrence as f64).ln())
```

Read left to right: exponential recency decay (a 14-day e-folding constant —
see [Decay](#decay); **not** a half-life), times normalised pain, times
normalised importance, times a logarithmic recurrence boost.

The ranking score is **BM25 × salience — a product, not a weighted sum**
(`collector.rs:1`, `:4`). `RecallHit` carries `bm25` (the raw, pre-multiply
score) and `salience` as separate fields (`:53-56`), so `dreamd recall
--explain` can show both factors instead of one blended number.

### Read-after-write visibility (commit-cadence window)

The indexer commits to Tantivy on a wall-clock cadence
(`DEFAULT_COMMIT_CADENCE`, default 5 seconds — `tantivy_handle.rs:63`). A
document appended via `POST /api/v1/learn` at T+0 is **not** searchable until
the next commit lands — worst-case T+5s.

This is a *freshness* constraint, not a *latency* constraint. The
`<5ms P50 warm` recall latency applies to the query operation itself and
is unaffected by the commit cadence. The two must not be conflated in
public copy or benchmark commentary. For Criterion-measured recall numbers
at n=1k/10k/100k see `README.md § Performance`.

Users who need sub-5s freshness can lower the cadence (toward 1s) at the
cost of higher I/O. User-facing cadence config is deferred to v0.1.1
(DR-307 / WEG-140); the value is a constructor argument today.

For the forward-looking plan across a future Tantivy major bump — how each schema
field is consumed, the custom collector's Tantivy-internal assumptions, and what a
maintainer must re-verify before bumping — see
[`architecture/tantivy-migration.md`](architecture/tantivy-migration.md).

## Dream cycle

The v0.1 dream cycle is **deterministic — no LLM is involved anywhere in it.**
The entry point is `consolidation::run_deterministic_dream_cycle`
(`consolidation.rs:258`), and the `LessonsFile` it writes hardcodes
`prompt_version: "deterministic-only"` (`consolidation.rs:302`). `now_sec` is
caller-provided rather than read from the wall clock (`consolidation.rs:88`),
so a cycle is reproducible from its inputs. LLM-assisted consolidation is
deferred past v0.1.

Phase order is fixed (`dream_cycle.rs:3-7`): **consolidation → decay → index →
autobiography.**

**Triggers: there are exactly two, and both are manual.** The CLI `dreamd
dream` (`cli.rs:78-79`) and `POST /api/v1/dream` (`router.rs:23`). **There is
no scheduler in v0.1** — nothing runs unless a user or an agent asks for it, so
do not read "nightly cycle" into any part of this design. `--auto`
(`cli.rs:42-45`) and `--dry` (`cli.rs:38-41`) are hidden flags that reject with
"Not yet supported at v0.1; ships v0.1.1". `--no-commit` (`cli.rs:46-49`) skips
only the git step.

### Clustering: a prefix tree over `skill_action`

`run_cluster_engine(agent_root, now_sec) -> Result<ClusterOutput, ConsolidationError>`
(`consolidation.rs:102`) builds a **prefix tree** over each event's
`skill_action`, split on `::`. Every event contributes a count to *every*
prefix of its own key: `split("::")`, then `parts[..len].join("::")` for `len`
in `1..=parts.len()` (`consolidation.rs:115-119`). An event keyed
`a::b::c` therefore counts toward `a`, `a::b`, and `a::b::c`.

Assignment is **deepest-wins**: prefixes are sorted longest-first and each
event is claimed by the deepest qualifying cluster through a `Vacant` entry
guard (`consolidation.rs:124-148`). Each event belongs to exactly **one**
cluster. Counts fan out across the tree; membership does not.

The output is `ClusterOutput { promoted: Vec<PromotedCluster> }` (`:60`), where
`PromotedCluster { cluster_key, events, salience_sum }` (`:68-75`).

### Recurrence and promotion

A cluster qualifies for promotion when it holds at least
`PROMOTION_THRESHOLD = 3` (`consolidation.rs:78`) events in **either** the
7-day **or** the 30-day trailing window (`WINDOW_7_DAYS_SEC` `:81`,
`WINDOW_30_DAYS_SEC` `:84`; the OR-logic is at `:139`). Either window alone is
sufficient — this catches both a burst inside a week and a slow drip across a
month. The counts are written to `semantic/recurrence_counts.json` as
`RecurrenceSidecar { schema_version: "1.0", clusters }` (`:185-193`).

Of the qualifying clusters, only the **single top cluster by `salience_sum`**
is promoted to `LESSONS.md`, and it contributes exactly **one** `Lesson` — the
exemplar (`:270-285`). `pick_exemplar` ranks salience → pain → importance →
reverse `EventId` (`:316-318`). A cycle is therefore a narrow instrument: at
most one lesson per run, not a batch summarisation pass.

Pins are **unioned, never unset**. `apply_pin_unpin` computes
`event.pinned = event.pinned || cited_ids.contains(...)` (`:221`), so a cycle
can pin an event it cites but can never clear a pin it did not set
(`consolidation.rs:8-10`, `:198-200`; SPEC §67 / WEG-426).

### Decay

Two distinct mechanisms share the word "decay". Keep them apart in prose and in
your head:

- The **recency term inside the salience formula**, `(-age_days / 14.0).exp()`
  (`salience.rs:86`), is a 14-day **e-folding time constant**: salience falls
  to `1/e` of its starting value after 14 days. It is **not** a half-life —
  the half-life of that curve is ~9.7 days, and calling it one is a factual
  error. It is evaluated at query time and never moves a record on disk.
- The **decay pruner** (`decay.rs`) is what actually archives records off the
  hot path. It fires only when `DECAY_AGE_THRESHOLD_SEC = 90 days`
  (`decay.rs:20`) **and** `DECAY_SALIENCE_THRESHOLD = 2.0` (`decay.rs:26`)
  both hold — old-and-uninteresting, not merely old. Pinned records are
  **never** archived (`decay.rs:33-35`).

### Autobiography

The final phase git-commits through `git2` into the user's **outer project
repo**. It stages exactly
`TRACKED_PATHS = [".agent/semantic/LESSONS.md", ".agent/episodic/AGENT_LEARNINGS.jsonl"]`
(`autobiography.rs:34-37`), commits under the fixed identity
`dreamd <noreply@dreamd.dev>` (`:29-30`) with the message shape
`dreamd: cycle YYYY-MM-DD` (`:7`), and reports
`Committed(Oid)` / `NoRepo` / `Skipped(SkipReason)` (`:51-60`).

The phase is **best-effort**: a failure logs at ERROR and the cycle still
succeeds (`dream_cycle.rs:125-131`). The memory is the JSONL and `LESSONS.md`;
the commit is a convenience layered on top of them.

One sharp edge: the dirty-tree check fires at **cycle start, not commit time**
(`autobiography.rs:16-20`). If the tracked files are dirty when the cycle
begins, the cycle **still runs and overwrites those edits** — only the commit
is skipped, with a WARN. The check is not a guard on hand-edits to
`LESSONS.md`; it only decides whether to commit.

## WAL / crash recovery

The dream cycle's filesystem phases run inside a write-ahead log at
`.agent/.dreamd/dream_in_progress.wal`, resolved through
`agent_root.wal_path()` (`wal.rs:5`, `:35-36`).

The file holds `DreamWal { schema_version, intents: Vec<WalIntent> }`
(`wal.rs:39-44`) as pretty JSON, where `WalIntent` is
`ReplaceSemanticMemory { temp_file_path }` | `PruneEpisodicMemory { temp_file_path }`
| `Commit` (`:25-31`). Its `STATE_SCHEMA_VERSION = "1.0"` (`:23`) is versioned
**independently** of the episodic record schema — see
[`../ARCHITECTURE.md` §7](../ARCHITECTURE.md#7-schema-versioning).

Lifecycle:

- `begin_cycle` writes a fresh WAL with no intents and moves `state.json` to
  `in_progress` (`:68-89`).
- `append_intent` rewrites the whole file atomically **before** each
  destructive operation (`:93-103`).
- `commit_cycle` appends `Commit`, moves state to `complete`, and deletes the
  WAL (`:107-118`).

**One WAL envelope spans both filesystem phases (ARCH-2).**
`run_filesystem_phases` is `begin_cycle → consolidation → decay → commit_cycle`
(`dream_cycle.rs:105-109`) — a single envelope, not one per phase. A phase
error short-circuits through `?` *before* `commit_cycle`, leaving an
uncommitted WAL for next-startup recovery. The regression test
`seam_crash_after_consolidation_recovers_not_clean` (`dream_cycle.rs:341-370`)
pins this: under the previous two-envelope shape, a crash between consolidation
and decay let a half-finished cycle be silently recorded as a success.

**Recovery.** `wal::recover_if_needed(agent_root, now_sec)` (`:122`) returns
`RecoveryOutcome::{Clean, Recovered { cleaned_files }, CommittedButUnclean}`
(`:46-54`). A WAL containing `Commit` means the cycle finished and only the
file outlived it: state goes to `complete`, the WAL is deleted, and the outcome
is `CommittedButUnclean` (`:134-138`). Otherwise each intent's temp file and
its `.tmp` neighbour are deleted, the WAL is deleted, state is set to
**`failed`**, and the outcome is `Recovered` (`:140-162`).
`recover_on_startup` (`:165-187`) runs from `run_watch` and from the lazy
per-project open, before any store access. A concurrent trigger is refused
rather than queued: `dream_cycle::ensure_not_in_progress` yields
`DreamCycleError::InProgress` (`dream_cycle.rs:83-88`, `:36-37`), which the
HTTP layer maps to 409.

**Two carve-outs that must not be overstated:**

1. Recovery is **compensating cleanup — a roll-back of temp files, not a
   roll-forward.** A recovered cycle lands in `failed`, not `complete`. Nothing
   replays the lost work; the next cycle starts over from the durable store.
2. **Tantivy index mutations are not WAL-protected in v0.1** (`wal.rs:8-11`;
   `ARCHITECTURE.md:151`). The "a reader never sees a mid-cycle store"
   invariant holds for the **durable** store — the JSONL, `LESSONS.md`, and the
   recurrence sidecar — **not** for the derived index. The index is a
   rebuildable cache (see [Indexing and salience](#indexing-and-salience)) and
   is allowed to lag or be rebuilt outright.

Deep dives: [`architecture/durability.md`](architecture/durability.md) for the
full durability argument, and
[`../ARCHITECTURE.md` §3 "Dream cycle WAL"](../ARCHITECTURE.md#3-dream-cycle-wal)
for the mermaid sequence diagram and the v0.1 scope line. This section
summarises both rather than restating them.

## API stability

**`/api/v1/*` is not a stable interface in v0.1.** Breaking changes to request
shapes, response shapes, and status codes are acceptable **between v0.1.x
releases**, provided each one lands with a `CHANGELOG.md` callout. The intent
is to stabilise the HTTP surface at **v0.2**. Integrators building against v0.1
should pin an exact version and read the changelog before upgrading. The
endpoint list — `learn`, `recall`, `preferences`, `health`, `dream` — lives in
[`http-api.md`](http-api.md) and is not duplicated here.

**This is the deliberate opposite of the on-disk contract.**
[`../SPEC.md`](../SPEC.md) freezes the on-disk contract for v0.1: folder
layout, the episodic node schema (`schema_version` `"1.0.0"`), the salience
formula, and the dream-cycle output shape — breaking any of those requires SPEC
v0.2+ and a documented migration path (`SPEC.md:162`). That freeze says
**nothing about the HTTP API**; SPEC explicitly places transport (stdio, HTTP,
Unix socket, MCP) out of scope. The durable artifact is the contract; the
socket is an implementation detail of one reader of it.

The practical consequence: a consumer that reads `.agent/` off disk is
insulated from everything this section permits. A consumer that speaks HTTP is
not.

## Threat model

The trust boundary is the local machine and the invoking uid. The API is served
over a Unix domain socket created with `0600` permissions (`server/uds.rs:187`),
and `peer_uid_middleware` — the outermost router layer (`server/http/router.rs:30-32`,
WEG-72 / DR-407) — rejects with 403 any peer whose uid does not match the
daemon owner's, including a peer whose uid cannot be determined at all. The uid
is read via `SO_PEERCRED` on Linux and `getpeereid` on macOS. **There is no TCP
listener in v0.1** — there is no remote attack surface to reason about because
there is no remote. That is the whole posture at this altitude. The canonical
threat model, the socket-auth middleware detail, and the disclosure policy live
in [`../SECURITY.md`](../SECURITY.md) — `docs/security.md` is only a redirect
to it. Do not re-derive the threat model here; link it.

## Writer-process lifecycle (WEG-21 / DR-118)

`server::Supervisor` owns the `MemoryCoordinator` task and the canonical
`mpsc::Sender` into it. Per the 2026-05-14 PM+architect decision (option a),
`MemoryCoordinator::run()` is NOT modified — the shutdown-drain invariant
lives entirely in the lifecycle layer:

1. Drop every issued sender clone (per-connection client tasks finish first).
2. Send `Shutdown { response_tx }` over the supervisor's retained `tx`.
3. The coordinator drains every queued `AppendLearning` ahead of `Shutdown`
   because the actor loop reads in FIFO order from a bounded channel.
4. `Shutdown` is terminal — any post-`Shutdown` send on a clone returns a
   typed `mpsc::error::SendError` (wrapped here as `SupervisorSendError`),
   never a silent drop. The supervisor unit tests assert both halves of
   this contract.

`detach_double_fork()` performs the canonical `fork → setsid → fork` daemon
dance. The first fork escapes the parent's process group; `setsid` makes
the child a session leader; the second fork ensures the final process is
not a session leader and therefore can never acquire a controlling
terminal. The session-leader process exits, leaving the kernel to reap the
final detached child via the grandparent (init / launchd / systemd).
**Not called in v0.1 production** — `dreamd watch` runs in the foreground;
this helper is reserved for v0.1.1 service install (ARCHITECTURE.md §8.1).
Windows daemonisation is DR-121 / WEG-135, deferred to v0.1.1.

## API backpressure

The coordinator channel is bounded at `COORDINATOR_CHANNEL_CAPACITY = 256` (constant in
`dreamd-core::server::lifecycle`). This is a round number chosen before load testing;
revisit after the DR-208 / DR-808 criterion benchmarks land.

All API handlers must call `Supervisor::try_send` rather than cloning the raw `tx` via
`sender()`. `try_send` applies a 100ms `COORDINATOR_SEND_TIMEOUT`; on expiry it returns
`CoordinatorSendError::Full`, which the Axum layer maps to HTTP 503 +
`Retry-After: 1` and emits a structured tracing event
(`dreamd_event="shed_load" route=... queue_depth=...`). On `CoordinatorSendError::Closed`
the daemon is shutting down; the response is 503 with no retry header.

**Why a method, not `sender()`:** `sender()` exists for the UDS connection pattern where
each task owns a raw clone and manages its own lifetime. HTTP handlers hold
`Arc<AppState>`; routing them through `try_send` prevents bypassing the timeout contract
and keeps the shed-load enforcement point singular.

**Dream cycle routing:** the cycle's filesystem phases run **through** the coordinator,
not beside it. `POST /api/v1/dream` sends `MemoryCoordinatorMsg::RunDreamCycle`
(`coordinator.rs:120`) over the same mpsc channel as appends; the actor loop dispatches it
(`:229`) to `handle_run_dream_cycle` (`:328`), which calls `run_filesystem_phases`
(consolidation → decay, inside one WAL envelope).

The reason is fd ownership (WEG-271). Both consolidation and decay can replace
`AGENT_LEARNINGS.jsonl` by atomic rename, which orphans the coordinator's long-lived
`File` — subsequent appends would land on an unlinked inode and be silently lost. Routing
the cycle through the actor lets the handler reopen `self.file` and seek to the end
(`:337-343`) as part of the same message, so there is no window in which the coordinator
holds a stale fd. `now_sec` / `cycle_date` are supplied by the caller so the cycle stays
wall-clock-free and deterministic in tests (`coordinator.rs:112-119`).

Two consequences follow, and both are load-bearing:

- Appends and a cycle's filesystem phases **serialise in the same FIFO actor loop** — a
  running cycle occupies the coordinator for its duration. Backpressure therefore applies
  to `dream` exactly as it does to `learn`: the handler goes through `try_send`, and a
  full channel returns 503 + `Retry-After: 1` (`handlers/dream.rs:84-93`).
- The cycle must be routed to the coordinator that **owns** the target project root, not
  the boot coordinator, or project B's cycle would prune project A's JSONL
  (`handlers/dream.rs:70-76`; WEG-272).

Only the post-filesystem phases leave the coordinator. Index work is handed to the
indexer task over its own bounded channel (`DEFAULT_INDEXER_CHANNEL_CAPACITY = 1024`,
`tantivy_handle.rs:75`), and the autobiography commit runs after it in `run_post_phases`
(`dream_cycle.rs:113-130`) — outside the coordinator, best-effort, and unable to block an
append.

## Observability (WEG-32 / DR-004)

`dreamd_core::observability::init_tracing` installs the process-wide `tracing`
subscriber exactly once, at the top of `cli::run()` before subcommand dispatch.
The `tracing` facade and its macro callsites already exist throughout the crate;
this baseline is what makes them emit. No per-subcommand init, no per-request
enrichment (that is DR-410 / WEG-144, deferred to v0.1.1).

Two layers:

- **Console → stderr, always.** stdout is reserved for the MCP JSON-RPC channel
  (`rmcp::transport::stdio`), so logs must never write to it. Format is
  TTY-conditional: pretty human-readable text when stderr is a terminal, JSON
  when it is not (CI, service-managed daemon). Detection uses
  `std::io::IsTerminal` — no `atty` crate.
- **File → `~/.agent/dreamd.log`, JSON always.** Written through a non-blocking
  appender (`tracing-appender`). The returned `WorkerGuard` is bound as
  `_log_guard` in `run()` and held until the process exits; dropping it early
  discards buffered file logs. The file is **truncated at startup** for v0.1 —
  log rotation is deferred to v0.1.1. The path is resolved via
  `DaemonHome::log_file()`, never hardcoded.

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
- [`../SECURITY.md`](../SECURITY.md) — canonical threat model, socket auth,
  disclosure policy (`docs/security.md` redirects here).
- [`architecture/durability.md`](architecture/durability.md) — WAL + JSONL
  durability deep-dive.
- [`architecture/tantivy-migration.md`](architecture/tantivy-migration.md) —
  what a maintainer must re-verify before a Tantivy major bump.
- [`http-api.md`](http-api.md) — `/api/v1/*` endpoints, headers, status codes.
- [`compared.md`](compared.md) — honest v0.1 comparison vs Mem0 / Letta Code /
  MCP-ref / Cline Memory Bank.
- `context/planning/PRD.md` Part IV — the authoritative end-state spec.
- `context/planning/agile/plan1.md` — sprint plan + decision register.
