# dreamd architecture

This document captures the load-bearing structural decisions for the running
codebase. Read it before touching the actor topology, the JSONL writer, or
the crate split.

## Actor model: `MemoryCoordinator` is the serialization point

State management is an actor model. A single `MemoryCoordinator` tokio task
owns the mutable handle to `AGENT_LEARNINGS.jsonl`. API handlers, the file
watcher, and (later) the dream-cycle pipeline send intents over a
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
3. `file.write_all(...)` — single syscall (no partial writes).
4. `file.sync_data()` — fdatasync to disk.
5. Send `Ok(())` over the oneshot.

The `POST /api/v1/learn` 201 response (when the HTTP layer lands) must not
return until step 5 fires. Concurrent third-party writers to the JSONL are
explicitly out of scope for v0.1 despite the looser language in PRD FR-1.2.

Blocking I/O note: the coordinator uses `std::fs::File`, so `write_all` and
`sync_data` block the tokio task. This is acceptable for v0.1 because the
actor already serializes mutations — there is no concurrency to preserve.
Do not introduce `tokio::fs` or `spawn_blocking` without benchmark evidence
and an ADR amendment; the cost of context switching per append likely
dominates the benefit at our target write rates.

The message enum `MemoryCoordinatorMsg` is `#[non_exhaustive]` to keep WEG-7
(idempotency + ULID), WEG-50 (dream-cycle trigger), and later additions
non-breaking for downstream `match` consumers.

## Crate-split tripwires

The workspace currently has three crates:

- `dreamd-protocol` — pure serde types (DR-102 / WEG-6).
- `dreamd-core` — `layout`, `privacy`, `lessons`, `io`, `coordinator`.
- `dreamd` (the CLI crate, package name `dreamd`, dir `dreamd-cli`).

Future splits will happen exactly when their tripwires fire — not before:

- **`dreamd-server`** — WEG-21 landed (2026-05-14) without extracting a
  separate crate. Resolution: UDS binding, double-fork lifecycle, and the
  `MemoryCoordinator` supervisor all live in `dreamd-core` behind
  `pub mod server`. Module layout:
  - `crates/dreamd-core/src/server/uds.rs` — `bind_writer_socket` with
    orphan-recovery and 0600 perms; `SocketGuard` Drop-cleans the path.
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
  `nix::unistd::fork` (an `unsafe fn`). All other modules in the crate
  still surface unsafe usage at compile time.

- **`dreamd-layout`** splits out when (and only when) a no-tokio consumer
  of layout appears. Today every layout caller already pulls in tokio
  indirectly via the coordinator; a split would create an empty re-export
  crate. The DR-002 plan anticipates a `dreamd-store` split too — defer
  that until the index + WAL code actually lands and a clear seam exists.

Premature splits are reversible but costly: each split adds a `version =`
bump, a Cargo path entry, and a CI matrix dimension. The default answer to
"should this be a new crate?" is **no, until a tripwire fires.**

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

**Dream cycle isolation (policy):** the dream cycle task does **not** share the coordinator
mpsc channel. It runs as a separate spawned task; coordinator appends and dream cycle
execution serialise only at file-lock and `IndexWriter`-mutex boundaries. Implementation
lands in WEG-70.

## Indexing

Tantivy 0.26.1 backs episodic recall. The schema is defined in
`dreamd-core::index::build_schema()` — one TEXT field (`content`), six
u64/f64 FastFields for the salience inputs, plus three forward-compatible
fields (`layer`, `last_updated_sec`, `cited_event_count`) reserved for
the v0.1.1 LESSONS.md indexing pipeline (WEG-136). Salience is computed
at query time by a custom collector (WEG-43) that reads the FastFields
and combines BM25 × `exp(-age/14) × ...` — no indexed score, no nightly
re-rank (see CLAUDE.md load-bearing decision #2).

The per-project index manifest at `<agent_root>/.dreamd/index_manifest.json`
carries `schema_version: "index/1.0"`. WEG-42 writes it on first index
init; WEG-49 enforces `binary.expected == manifest.version` on daemon
startup. WEG-24-A binds: never `rm` `.tantivy-writer.lock` /
`.tantivy-meta.lock` — kernel handles advisory flock.

On startup, the daemon reads `<agent_root>/.dreamd/index_manifest.json`
via `dreamd-core::index::check_manifest_version`. A missing manifest is
a pre-index state and logs a warning. A manifest version older than the
binary logs a `NeedsMigration` warning (`dreamd migrate` ships in
DR-108 / v0.1.1). A manifest *newer* than the binary aborts startup
with `ServerError::ManifestCheck(ManifestVersionError::TooNew)` —
the user must downgrade or migrate.

### Read-after-write visibility (commit-cadence window)

The indexer commits to Tantivy on a wall-clock cadence
(`DEFAULT_COMMIT_CADENCE`, default 5 seconds). A document appended via
`POST /api/v1/learn` at T+0 is **not** searchable until the next commit
lands — worst-case T+5s.

This is a *freshness* constraint, not a *latency* constraint. The
`<5ms P50 warm` recall latency applies to the query operation itself and
is unaffected by the commit cadence. The two must not be conflated in
public copy or benchmark commentary. For Criterion-measured recall numbers
at n=1k/10k/100k see `README.md § Performance`.

Users who need sub-5s freshness can lower the cadence (toward 1s) at the
cost of higher I/O. User-facing cadence config is deferred to v0.1.1
(DR-307 / WEG-140); the value is a constructor argument today.

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

- `docs/architecture/durability.md` — WAL + JSONL durability deep-dive
  (forward-looking: lands with WEG-7's sidecar idempotency + ULID work).
- `docs/security.md` — threat model, UDS perms, `SO_PEERCRED` middleware.
- `context/planning/PRD.md` Part IV — the authoritative end-state spec.
- `context/planning/agile/plan1.md` — sprint plan + decision register.
