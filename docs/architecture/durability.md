# JSONL append durability (DR-103 / WEG-7)

> Architect-side reference for the durability protocol. The decision and its
> constraints are in [`ARCHITECTURE.md` §1](../../ARCHITECTURE.md#1-jsonl-append-durability);
> the code is `crates/dreamd-core/src/coordinator.rs` (the actor) and
> `crates/dreamd-core/src/episodic.rs` (the one read / append / recover seam).

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

All appends to a project's log funnel through that project's
`MemoryCoordinator` actor (`crates/dreamd-core/src/coordinator.rs`) — one actor
per project root; the daemon keeps them in a `SupervisorMap`. The actor owns
the `File` handle directly; `&mut self` on the run loop is the exclusivity
guarantee — there is no `Mutex<File>` wrapper because there is no second handle
to compete with. Concurrent third-party writers are explicitly unsupported
(`ARCHITECTURE.md` §1).

The same actor also runs the two operations that replace the file by atomic
rename — the dream cycle's filesystem phases (`RunDreamCycle`) and the forget
cascade (`Forget`) — and reopens its fd afterward, so an append never lands on
an orphaned inode.

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
2. **Mint `EventId` and stamp the daemon-owned fields.**
   `format!("evt_{}", Ulid::new())`, then `EventId::parse(...)`. The expect is
   sound: a freshly minted ULID is always valid. The coordinator also overwrites
   `schema_version` (`RECORD_SCHEMA_VERSION`, `"1.0.0"`) and `timestamp`
   (`Utc::now()`), so a client cannot persist its own values for any of the
   three.
3. **Serialize.** `serde_json::to_string(&learning)` (with the minted id
   substituted into the struct). Steps 3–7 are `episodic::append`.
4. **Ensure trailing `\n`.** If the serialized form does not end with
   `\n`, append one. This guarantees every line is fully terminated, which
   is what the malformed-tail-skip recovery (below) relies on.
5. **4 KiB cap check.** If the resulting byte length exceeds
   `MAX_LEARNING_LINE_BYTES` (4096), return `CoordinatorError::PayloadTooLarge`
   without writing. The HTTP handler maps this to **413 Payload Too Large**.
   There is no sidecar storage for oversized payloads.
6. **Single `write_all`.** One call for the whole buffer. `write_all` loops
   over `write` until the buffer is drained, so it is not a single-syscall
   guarantee; at ≤4 KiB it is one `write` in practice, which minimises the
   window in which a kill could leave a torn line. A torn line that does land
   is removed by startup recovery (below).
7. **`sync_data`.** Force the bytes to durable storage. The HTTP handler
   does not return `201` until this completes.
8. **LRU insert.** Only on `sync_data` success and only if a
   `client_dedup_key` was provided, insert `(root, key) -> EventId` into
   the LRU. Insert-before-write would poison the cache on write failure
   (a retry would see a phantom hit despite no durable bytes on disk).
9. **Hand off to the indexer.** The coordinator awaits `send` of
   `IndexerMsg::Append` on the indexer channel, then replies. This is index
   freshness, not durability — the line is already on disk — but it is why a
   full indexer channel shows up as append latency
   (`ARCHITECTURE.md` §4).

## Idempotency LRU

- Type: `LruCache<(PathBuf, String), IdempotencyEntry>`; an entry holds the
  `EventId` and the stamped `timestamp`, and a hit returns both with
  `deduplicated: true`.
- Capacity: 1024 entries, per coordinator (so per project).
- Lifetime: in-memory only; cleared on restart, and when the daemon evicts an
  idle project's coordinator.
- Key: `(canonicalized AgentRoot path, client_dedup_key)`. One daemon serves
  many project roots, each with its own coordinator and its own cache; the path
  component keeps the key unambiguous regardless.
- Over HTTP the key is the `X-Client-Dedup-Key` request header; MCP
  `append_node` takes it as the optional `client_dedup_key` argument. It is not
  written into the JSONL record.

Durable replay protection is **out of scope**. A restart drops the
cache, so a client that retries across a restart can produce two durable
records for one logical event. The trade is intentional: the alternative
(a sidecar dedup journal) would multiply the IOPS per write and has not been
built.

## Startup recovery: truncate the torn tail, quarantine mid-file damage

At construction (`MemoryCoordinator::open_at`) the coordinator calls
`episodic::recover`, which scans the existing JSONL from offset 0 with the one
shared scan policy (`episodic::scan`, SPEC §"Corrupt input"). Three cases:

- Every line parses and the file ends with `\n` — clean state, nothing is
  written.
- Trailing bytes with no terminating `\n` (a torn tail) and no other damage —
  `set_len` back to the end of the last complete line. No sidecar.
- A `\n`-terminated line that is blank or does **not** parse as
  `AgentLearning` — the raw line is appended to
  `episodic/.corrupt-<YYYY-MM-DD>.jsonl` beside the log (same-day runs append
  to the same sidecar), and the live file is rewritten **in place, on the
  coordinator's own fd** to the well-formed records. Valid records on both
  sides of the bad line survive; a torn tail in the same file is dropped in
  the same rewrite. A `WARN` names the sidecar and the counts (AILAB-163).

Recovery never truncates from the first bad line onward — that would discard
good records after it — and never swaps the inode (no rename), because the
coordinator holds this fd for every later append. After either write the file
is `sync_data`'d so the recovery itself is durable across a follow-up crash.
The file handle then seeks to end-of-file and the actor begins serving
messages.

Plain readers (`episodic::read_all`: the dream cycle, decay, the index replay,
`dreamd doctor`) apply the same scan but rewrite nothing: they skip a bad
`\n`-terminated line and ignore a torn tail.

This is what makes a `kill -9` mid-`write_all` safe: even if a partial buffer
landed on disk, the next process start prunes it before any new append.

## Test coverage in this crate

The four behaviors are covered as unit tests in `coordinator.rs`:

- `append_learning_mints_id_and_persists_durably`
- `idempotency_lru_short_circuits_on_dedup_key_hit`
- `payload_over_4kib_rejected_with_payload_too_large`
- `malformed_tail_skipped_on_startup`

The recovery cases are unit tests in `episodic.rs` (`recover_*`), with a
property test in `tests/episodic_scan_proptest.rs`.

Cross-cutting verification — 1000 concurrent appends landing 1000 complete
lines (DR-110 / WEG-12) — is `tests/weg12_concurrency_torture.rs`. It drives
the coordinator channel directly rather than `POST /api/v1/learn`: the HTTP
path sheds with 503 once the 256-slot inbox stays full past 100 ms, so an HTTP
hammer cannot assert an exact line count without retries.

## Non-goals

- Concurrent third-party writers to the JSONL.
- Durable replay protection across restart.
- Sidecar storage for oversized payloads (not shipped; an over-4 KiB line is
  rejected).
- Windows-specific durability semantics (`FlushFileBuffers`, ReFS). Windows
  runs `watch` + learn only; `io::write_atomic` is unsupported there, so the
  dream cycle and the index do not run. See [`../windows.md`](../windows.md).
