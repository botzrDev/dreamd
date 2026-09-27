# Context grant (`GrantedStore`, BZR-827)

**Status: prototype, not wired to MCP.** `crates/dreamd-core/src/context_grant.rs` adds a
fourth `MemoryStore` implementor, `GrantedStore`, on the seam BZR-173 / BZR-183 already
established. It is not connected to any tool. `search_nodes` and `append_node` still hold
`InProcessStore` or `DaemonStore` directly (`mcp/mod.rs`), which is equivalent to the
`ContextGrant::both()` grant below.

## What a grant is

A `ContextGrant` is two booleans, `episodic` and `semantic`, naming the two recall sources
a recall hit's `source` field already carries (`"episodic"` — events in
`AGENT_LEARNINGS.jsonl` — or `"semantic"` — lessons in `LESSONS.md`; see
`crate::index::Layer`). There is no third source and no numeric or graded grant.

`ContextGrant::both()` is the grant every existing caller has today, because MCP never
constructs a `GrantedStore`. On `both()`, recall forwards the inner store's JSON body
unchanged — it is not parsed or re-serialized, even if that body is not valid JSON.

## What is granted

- **Recall.** A narrower grant parses the inner store's `{"results":[...]}` body and keeps
  only the hit objects whose `source` is a granted layer. A hit with `source` unset, or with
  any other value, is dropped. The filtered array is re-serialized as `{"results":[...]}`;
  every other field of a kept hit is passed through unchanged. A `results` field that is
  missing or not an array is not a caller mistake — it means the inner store's body is
  malformed — so it is `MemoryStoreError::Internal`, not `InvalidRequest`.
- **Append.** An append is granted only when `episodic` is set. When it is not, `append`
  returns `MemoryStoreError::InvalidRequest("episodic source is not granted")` and the inner
  store's `append` is never called. There is no append path for `semantic` — lessons are
  written only by the dream cycle, never by a direct caller.

## What is not granted here

- **`personal/`.** Sharing `personal/` content is the dream-cycle `x-dreamd-share-personal`
  header (`client.rs`), decided per dream-cycle call. It is not a `Layer` and not a `source`
  value, so it is outside this grant. `GrantedStore` does not read or set that header.
- **A momo `MemoryRegion`.** The dreamOS charter (momo `RFC-009`, amended by `RFC-011`)
  names a kernel object, `MemoryRegion` (FR-2.4), that this repository does not implement.
  `ContextGrant` is not that object, does not claim to be it, and does not attempt the
  charter's provenance/trust rule. This type does not record a trust label anywhere. A
  `MemoryRegion` is future work in the momo kernel repository, not this prototype.

## Where this sits

`GrantedStore::new(inner, grant)` wraps any existing `MemoryStore` — `InProcessStore`,
`DaemonStore`, or `LettaStore` — the same way `LettaStore` wraps one. Nothing in
`mcp/mod.rs`, `memory_store.rs`, `letta.rs`, or `index.rs` changed to add it. Wiring a
narrower grant into a real caller (an MCP tool parameter, a second transport, a UI) is a
later, separate decision.
