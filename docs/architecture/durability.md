# JSONL append durability (DR-103 / WEG-7)

> This file lives under `docs/`, which is gitignored. Treat it as the
> architect-side reference for the durability protocol; it is intentionally
> untracked. The user-facing contract is enumerated in the WEG-7 Linear ticket
> and in `CLAUDE.md` "Load-bearing engineering decisions" §1.

## Goals

1. Every line appended to `episodic/AGENT_LEARNINGS.jsonl` is atomic at the
   line granularity — no half-written or interleaved records.
2. A line is durable on disk before the HTTP handler returns `201`.
3. A hard kill mid-write cannot leave the JSONL in a state that poisons
   subsequent appends (no garbage-prefixed lines, no torn fields).
4. Duplicate `POST /api/v1/learn` calls (e.g., client retry after timeout)
   produce one durable line and report the same `EventId`, for the lifetime
   of the daemon process.

## Architecture

All writes funnel through the singleton `MemoryCoordinator` actor
(`crates/dreamd-core/src/coordinator.rs`). The actor owns the `File` handle
directly; `&mut self` on the run loop is the exclusivity guarantee — there
is no `Mutex<File>` wrapper because there is no second handle to compete
with. Concurrent third-party writers are explicitly unsupported in v0.1
(CLAUDE.md §1).

## EventId

Daemon-assigned ids are `evt_` + a canonical 26-char Crockford base32 ULID,
e.g. `evt_01ARZ3NDEKTSV4RRFFQ69G5FAV`. The `EventId` newtype lives in
`dreamd-protocol` with a private inner field; the only constructor is
`EventId::parse`, which validates prefix, length, and alphabet. ULID minting
itself lives in `dreamd-core` (the `ulid = "1"` crate); `dreamd-protocol`
intentionally has no `ulid` dependency. Wire and on-disk format pass through
the same serde path, so malformed ids are unrepresentable end-to-end.

`AgentLearning.id` is the `EventId` type. No `schema_version` bump — this
is a pre-data correction to the WEG-6 shape, applied before any real records
exist.

## Per-write protocol

For each `AppendLearning` message the coordinator runs the following steps
**in order**:

1. **Idempotency lookup.** If the message carries a `client_dedup_key`,
   look up `(canonicalized AgentRoot path, key)` in the in-memory LRU.
   On hit, return the cached `EventId` immediately — no second line is
   appended.
2. **Mint `EventId`.** `format!("evt_{}", Ulid::new())`, then
   `EventId::parse(...)`. The expect is sound: a freshly minted ULID is
   always valid.
3. **Serialize.** `serde_json::to_string(&learning)` (with the minted id
   substituted into the struct).
4. **Ensure trailing `\n`.** If the serialized form does not end with
   `\n`, append one. This guarantees every line is fully terminated, which
   is what the malformed-tail-skip recovery (below) relies on.
5. **4 KiB cap check.** If the resulting byte length exceeds
   `MAX_LEARNING_LINE_BYTES` (4096), return `CoordinatorError::PayloadTooLarge`
   without writing. The HTTP handler maps this to **413 Payload Too Large**.
   Sidecar storage is deferred to v0.1.1.
6. **Single `write_all`.** One contiguous `write` system call for the
   whole buffer. This minimises the window in which a kill could leave a
   torn line. (Linux does not formally guarantee atomicity for arbitrary
   buffer sizes, but at ≤4 KiB on a page-aligned filesystem the practical
   torn-line incidence is negligible.)
7. **`sync_data`.** Force the bytes to durable storage. The HTTP handler
   does not return `201` until this completes.
8. **LRU insert.** Only on `sync_data` success and only if a
   `client_dedup_key` was provided, insert `(root, key) -> EventId` into
   the LRU. Insert-before-write would poison the cache on write failure
   (a retry would see a phantom hit despite no durable bytes on disk).

## Idempotency LRU

- Type: `LruCache<(PathBuf, String), EventId>`.
- Capacity: 1024 entries.
- Lifetime: in-memory only; cleared on restart.
- Key: `(canonicalized AgentRoot path, client_dedup_key)`. The path component
  matters in multi-project setups where one daemon may someday serve multiple
  roots; today it is a single root per daemon, but the key shape is
  forward-compatible.

Durable replay protection is **out of scope** for v0.1. A restart drops the
cache, so a client that retries across a restart can produce two durable
records for one logical event. The trade is intentional: the alternative
(a sidecar dedup journal) would multiply the IOPS per write and was deferred
to v0.1.1.

## Startup recovery: malformed-tail-skip

At construction (`MemoryCoordinator::open_at`) the coordinator scans the
existing JSONL from offset 0 forward. It records the byte offset just after
each successfully parsed line. Three terminating conditions:

- End of file with a final `\n` — clean state, no truncation.
- A line whose bytes do **not** parse as `AgentLearning` — truncate
  everything from the **start** of that line onward.
- Trailing bytes with no terminating `\n` — truncate them; the prior
  `\n`-terminated lines are the recovery point.

After truncation the file is `sync_data`'d so the recovery itself is durable
across a follow-up crash. The file handle then seeks to end-of-file and the
actor begins serving messages.

This is what makes a `kill -9` mid-`write_all` safe: even if a partial buffer
landed on disk, the next process start prunes it before any new append.

## Test coverage in this crate

The four behaviors are covered as unit tests in `coordinator.rs`:

- `append_learning_mints_id_and_persists_durably`
- `idempotency_lru_short_circuits_on_dedup_key_hit`
- `payload_over_4kib_rejected_with_payload_too_large`
- `malformed_tail_skipped_on_startup`

Cross-cutting verification — 1000 concurrent `/learn` calls landing 1000
complete lines (DR-110 / WEG-12) — lives behind the HTTP surface and is
scheduled with the API endpoint ticket.

## Non-goals (v0.1)

- Concurrent third-party writers to the JSONL.
- Durable replay protection across restart.
- Sidecar storage for oversized payloads (deferred to v0.1.1).
- Windows-specific durability semantics (`FlushFileBuffers`, ReFS) —
  Windows lifecycle lands in v0.1.1; see `docs/windows.md`.
