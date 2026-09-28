# Citation observability (`GET /api/v1/recall?explain=1`)

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
