# Tantivy migration plan (DR-209 / WEG-130)

Forward-looking architect-side reference for the day dreamd bumps Tantivy across
a major version. dreamd currently pins `tantivy = "0.26"` (resolved **0.26.1** in
`Cargo.lock`; `crates/dreamd-core/Cargo.toml`). This document records what a future
bump has to re-verify so it is a planned change rather than a surprise re-index.

**Scope.** This is documentation of *current* behavior and its assumptions. It does
**not** propose or perform a migration now. No schema, code, or manifest changes are
implied by this file.

## Why an index-time change is a non-event for dreamd

Tantivy dropped index-time sort support in an earlier release; dreamd never relied
on it, because salience ranking is computed entirely at **query time** by the custom
collector (`crates/dreamd-core/src/collector.rs`, WEG-43). The BM25 × salience
product is *never* written to the index — there is no indexed score field and no
nightly re-rank (CLAUDE.md load-bearing decision #2).

The practical consequence: a future Tantivy bump that changes index-time sort or
segment-ordering semantics cannot change dreamd's ranking, because dreamd's ranking
does not live in the index. The surfaces that *can* break on a bump are the
query-time read paths — the FastField accessors and the collector traits — covered
below. This is stated plainly so a maintainer does not go hunting for an index-time
sort dependency that was never there.

## Schema fields locked at v0.1 (DR-201 / WEG-41)

The canonical schema is `build_schema()` in `crates/dreamd-core/src/index.rs`. Eleven
fields, locked at v0.1. Two access shapes matter for a migration:

- **STORED** — retrieved via stored-document hydration (`TantivyDocument`) to
  reconstruct a recall result.
- **FAST** — read by the collector at query time via `tantivy::columnar::Column`,
  never through stored-document retrieval.

| Field | Type | Flags | Consumed by |
|---|---|---|---|
| `content` | text | `TEXT \| STORED` | BM25 full-text matching; STORED text hydrated into the recall result |
| `timestamp_sec` | u64 | `INDEXED \| FAST` | collector salience input (age → recency decay); also range/term-queryable |
| `pain` | f64 | `FAST` | collector salience input (subjective friction) |
| `importance` | f64 | `FAST` | collector salience input (long-term relevance) |
| `recurrence` | u64 | `FAST` | collector salience input (cluster occurrence count) |
| `layer` | text | `STRING \| STORED` | hydrated into the recall result; reserved for the v0.1.1 LESSONS.md pipeline (WEG-136) |
| `last_updated_sec` | u64 | `FAST` | reserved for the v0.1.1 LESSONS.md pipeline (WEG-136) |
| `cited_event_count` | u64 | `FAST` | reserved for the v0.1.1 LESSONS.md pipeline (WEG-136) |
| `event_id` | text | `STRING \| STORED` | exact-match term for delete-and-re-add (WEG-45); STORED for hydration |
| `skill_action` | text | `STRING \| STORED` | hierarchical clustering key; hydrated into the recall result |
| `source_harness` | text | `STRING \| STORED` | provenance harness id; hydrated into the recall result |

The **four salience FastFields** the collector reads per matching document are
`timestamp_sec`, `pain`, `importance`, and `recurrence`. `timestamp_sec` is both
`INDEXED` (so it can be filtered/range-queried) and `FAST` (so the collector can read
it as a column). The remaining FAST-only fields (`last_updated_sec`,
`cited_event_count`) are reserved and not yet read on the recall path.

`STRING` (not `TEXT`) on `event_id`, `layer`, `skill_action`, and `source_harness` is
deliberate: these are exact-match terms, not tokenized text. A migration must preserve
that distinction — re-tokenizing `event_id` would break WEG-45's delete-and-re-add.

## The custom collector + scorer (DR-203 / WEG-43)

`crates/dreamd-core/src/collector.rs` is where dreamd reaches deepest into Tantivy
internals, and therefore where a major bump is most likely to require work. Its
module doc-comment (lines 1–12) enumerates the contract. The Tantivy-internal
assumptions it depends on:

- **`Collector` / `SegmentCollector` trait pair.** The collector implements both
  (`tantivy::collector::{Collector, SegmentCollector}`). Their signatures — how a
  segment collector is created per segment, how `collect(doc, score)` is called, and
  how `merge_fruits` combines per-segment results — are the primary break surface.
- **FastField access via `tantivy::columnar::Column`.** The four salience fastfields
  are read per matching doc through `columnar::Column`. Tantivy has reorganized its
  columnar/fastfield API across majors before; this accessor is the second break
  surface.
- **BM25 `Score` is `f32`, widened to `f64`.** Tantivy hands the collector a raw BM25
  `Score` (`f32`); dreamd widens it and multiplies by the query-time salience score.
  The product stays in `f64` to preserve precision for the `--explain` formatter
  (DR-703). The score is never indexed — see decision #2 above.
- **`BinaryHeap` min-heap of size `k`.** Top-`k` eviction is a `std::collections::BinaryHeap`
  keyed on `Reverse<OrderedFloat<f64>>` (`ordered-float` for the `Ord` impl `f64`
  alone lacks). This is dreamd-owned, not Tantivy-owned, so it is bump-stable — but it
  consumes `DocAddress` / `DocId` / `SegmentOrdinal`, whose shapes come from Tantivy.
- **Query construction and hydration types.** The recall path also uses
  `QueryParser`, `BooleanQuery` / `TermQuery` / `Occur`, and `Term` / `IndexRecordOption`
  / `TantivyDocument` / `Value` for stored-field hydration. These are lower-risk but
  should be smoke-checked on a major bump.

**On a major bump, re-verify (in order of risk):** (1) the `Collector` /
`SegmentCollector` trait signatures still compile against `collector.rs`; (2) the
`columnar::Column` fastfield read path is unchanged; (3) BM25 `Score` is still `f32`
(the widen point assumes it); (4) query-builder and `TantivyDocument`/`Value`
hydration APIs. A green `cargo test -p dreamd-core` over the collector's unit tests is
the acceptance signal.

## Forward-compatibility notes

Targets: the **next** Tantivy major beyond 0.26.1, and the v0.2 alpha vector backend
(WEG-155 / WEG-156 / WEG-158), which introduces a second retrieval path alongside
BM25 × salience.

Checklist a maintainer works before bumping:

1. **FastField / `columnar` API stability** — the collector's per-doc read path
   (above) is the highest-risk surface.
2. **Collector trait signatures** — confirm `Collector` / `SegmentCollector` still
   match; adjust `collector.rs` if the trait shape moved.
3. **Re-index required?** — decide whether the on-disk segment format changed. If it
   did, the index must be rebuilt from the durable JSONL (`episodic/AGENT_LEARNINGS.jsonl`),
   which remains the source of truth; the Tantivy index is a derived cache.
4. **Schema-version manifest gate.** The per-project manifest at
   `<project>/.agent/.dreamd/index_manifest.json` carries the schema version from
   `dreamd-core::index::SCHEMA_VERSION` (`index.rs:24` — `index/1.3` at time of
   writing; read the constant, this line goes stale on the next bump). WEG-42
   writes it on first index init; WEG-49 enforces it on daemon startup. A manifest
   *older* than the binary (`NeedsMigration`) makes `TantivyIndexHandle::open`
   rebuild the derived index from `episodic/AGENT_LEARNINGS.jsonl` in place,
   resetting the replay watermark so the full JSONL re-indexes rather than the
   tail; no `dreamd migrate` step is involved (see `../architecture.md`). A
   manifest *newer* than the binary aborts startup with
   `ManifestVersionError::TooNew` — a hard error, not a rebuild. A bump that
   requires a re-index must also bump `SCHEMA_VERSION` so this gate fires instead
   of silently serving a stale or incompatible index.
5. **Lock-file discipline unchanged.** Never `rm` `.tantivy-writer.lock` /
   `.tantivy-meta.lock` across a bump (WEG-24-A) — the kernel handles advisory flock.

## Performance baseline (DR-908)

Real recall-latency figures, sourced from `README.md § Performance` (warm in-RAM
index, Criterion 0.5, WSL2/Linux; mean across 100 samples, used as a P50 proxy):

| Corpus size | Mean (warm) |
|---|---|
| 1 000 entries | ~50 µs |
| 10 000 entries | ~313 µs |
| 100 000 entries | ~2.8 ms |

All three are well under the `<5ms P50 warm` recall NFR. Reproduce with
`cargo bench -p dreamd-core`. A migration should re-run this benchmark and confirm no
regression past the NFR before landing. (Read-after-write visibility — up to the
5-second index commit cadence — is a freshness constraint, not a query-latency one,
and must not be conflated with these numbers; see `../architecture.md`.)

## Cross-references

- **DR-201 / WEG-41** — schema (`crates/dreamd-core/src/index.rs`, `build_schema()`).
- **DR-203 / WEG-43** — custom collector (`crates/dreamd-core/src/collector.rs`).
- **DR-908** — benchmark methodology and figures (`README.md § Performance`).
- [`../architecture.md`](../architecture.md) — the Indexing section this deep-dive
  expands; and `../architecture/durability.md` for the JSONL durability protocol that
  backs re-index-from-source.
