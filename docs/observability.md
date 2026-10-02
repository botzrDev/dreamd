# Recall citations, blame, and salience drift

This page covers `explain=1` on `GET /api/v1/recall`, `dreamd blame`, counterfactual recall (`--without` / `exclude=`), and `dreamd salience-drift`. The per-request `http request` log line and the `x-request-id` header are in [http-api.md](./http-api.md) (Request ID).

## What this is

`GET /api/v1/recall` accepts an optional `explain` query parameter. When its
value is exactly `1`, the response gains a `citations` array alongside the
existing `results` array, one citation per result in the same order, giving
the salience-factor breakdown behind each hit's score. Any other value —
including the parameter being absent — omits the `citations` key entirely.
There is no config key and no default-on mode: the query parameter is the
whole switch.

MCP `search_nodes` does not take `explain` and never returns `citations`. It
always returns the same body it always has.

## Fields

Each entry in `citations` is:

| Field | Description |
|---|---|
| `id` | Event id (`RecallResult::event_id`); may be empty for pre-WEG-45 docs |
| `score` | Final ranking score: BM25 × salience |
| `bm25_component` | Raw Tantivy BM25 score before the salience multiply |
| `salience_component` | Query-time salience multiplier |
| `layer` | Memory layer: `episodic` or `semantic` — the same token as the paired `results[].source` |
| `snippet` | Full stored learning text, no truncation |
| `age_days` | `(now - timestamp) / 86400`, the same expression the salience formula uses |
| `pain` | Stored 0.0–10.0 subjective friction score — the raw value, not divided by 10 |
| `importance` | Stored 0.0–10.0 long-term relevance score — the raw value, not divided by 10 |
| `recurrence` | Cluster occurrence count from the index fastfield (bounded-staleness sidecar) |

`layer` names the Tantivy document layer a hit was indexed under —
`episodic` (raw events) or `semantic` (dream-cycle-consolidated lessons). It
is not an embedding or vector-similarity dimension; dreamd's recall is BM25 ×
salience end to end. There is no trust label on a citation — `layer` and
`id` describe where a memory came from, not how much to trust it.

## Salience formula

```
BM25 × exp(-age_days / 14) × (pain / 10) × (importance / 10) × (1 + ln(1 + recurrence))
```

The `14` in `exp(-age_days / 14)` is an **e-folding time constant** (the
curve reaches `1/e` at 14 days), not a half-life. The half-life of this curve
is `14 × ln(2) ≈ 9.7` days. `citations[].age_days` is the input to that term;
`citations[].pain` and `citations[].importance` are the stored values before
the `/ 10` the formula applies.

## Where else this shows up

`dreamd recall --explain` already prints this same factor block from the CLI
(`crates/dreamd-cli/src/commands/recall.rs`); this endpoint gives HTTP
callers the same breakdown as structured JSON instead of formatted text.

## `dreamd blame <query>` (BZR-193)

`dreamd blame <query>` is a second, read-only CLI view over the same
salience-scored recall used by `dreamd recall` — same discover / index-open /
`dreamd_core::recall` sequence, different table shape and a smaller default
`-k` (5, vs recall's 10). It does not proxy through a live daemon.

The markdown table columns, in order, are:

`| id | timestamp | skill_action | content | bm25 | salience | total | layer |`

- `id` — `RecallResult::event_id`
- `timestamp` — `timestamp_sec` as a decimal integer
- `content` — the stored learning text, truncated to 80 characters
- `total` — `RecallResult::score` (BM25 × salience)
- `layer` — `episodic` or `semantic`

Empty results still print the header and `(0 hits)`, exiting 0.

`--explain` appends the same per-hit DR-204 factor block `dreamd recall
--explain` prints, using the shared `explain_factors` helper — no separate
arithmetic.

`--json` prints one compact JSON array (plus a trailing newline) instead of
the markdown table. Each object carries `id`, `timestamp`, `skill_action`,
`content` (same 80-character truncation), `bm25`, `salience`, `total`, and
`layer`. With `--explain`, each object also gains `age_days`, `pain`,
`importance`, `recurrence`, `decay`, `pain_factor`, `importance_factor`,
`recurrence_factor`, and `salience_product`.

`dreamd blame` does not change the `dreamd recall` columns or its default
`-k` of 10.

## Counterfactual recall: `--without` / `exclude=` (BZR-194)

Counterfactual recall answers "what would recall return if these memories
were absent?" Matching documents are dropped inside the salience collector,
before the top-k heap is filled, so up to `k` of the remaining hits come
back. The index is not written; nothing is deleted.

- `dreamd recall <query> --without <event_id>` is repeatable and matches the
  stored `event_id` exactly.
- `dreamd recall <query> --without-cluster <skill_action>` is repeatable. It
  drops every hit whose `skill_action` equals the prefix or extends it with
  `::`, on both the episodic and semantic layers. `rust::error_handling`
  matches itself and `rust::error_handling::axum`; it does not match `rust`,
  `rustacean`, or `rust::errors`.
- `GET /api/v1/recall?q=…&exclude=<event_id>` takes a repeatable `exclude`
  (`exclude=evt_a&exclude=evt_b`). There is no cluster query parameter. With
  `explain=1`, `citations` covers the filtered hits.
- An `evt_…` id does not remove the `lsn_evt_…` lesson document derived from
  it. Exclude the cluster to drop the lesson too.
- MCP `search_nodes` and `dreamd blame` do not take these flags.

With no exclusion, recall is unchanged and never reads the stored-document
store during collection.

## Salience distribution: `GET /api/v1/observability/salience` / `dreamd salience-drift` (BZR-195)

Both surfaces report the same distribution over every episodic document in
the project index:

- **Score.** Each document is scored with `salience()` —
  `exp(-age_days/14) × (pain/10) × (importance/10) × (1 + ln(1 + recurrence))`.
  There is no query, so there is no BM25 term.
- **Excluded.** Lesson documents (`event_id` starting with `lsn_`) are derived
  from events, not events, and are not counted.
- **`histogram`.** Ten buckets. Index `i` is `[i/10, (i+1)/10)` for `i` in
  0..9; index 9 is `[0.9, +∞)`. A score below 0 goes in index 0.
- **`top_clusters`.** At most 5 `skill_action` clusters by mean salience,
  descending, ties broken by `skill_action` ascending. Each is
  `{"skill_action","count","mean"}`.
- **Snapshot.** Every call — the GET and the CLI — writes today's snapshot to
  `.agent/.dreamd/observability/salience-<UTC date>.json`, overwriting that
  date's file. There is no background scheduler. The snapshot omits `drift`
  and `drift_against`.
- **`drift`.** Today's `mean` minus the `mean` in
  `salience-<date − 7 days>.json`. It is `null` when that exact file is absent
  or does not parse; a file from 6 or 8 days earlier does not count.
  `drift_against` names that date even when `drift` is `null`.

Body (`200`), field order as shown; `schema_version` is `observability/1.0`:

```json
{"schema_version":"observability/1.0","date":"2026-09-29","total":42,"mean":0.183,"histogram":[20,9,6,3,2,1,1,0,0,0],"top_clusters":[{"skill_action":"rust::tokio","count":4,"mean":0.41}],"drift":null,"drift_against":"2026-09-22"}
```

`dreamd salience-drift` prints one summary line, then one line per cluster:

```
2026-09-29 total=42 mean=0.183000 drift=none
rust::tokio count=4 mean=0.410000
```

`dreamd salience-drift --json` prints the body above as one compact line. The
CLI opens the index read-only and exits 2 when no `.agent/` store is found.
The endpoint takes no query parameters; MCP `search_nodes` is unchanged.
