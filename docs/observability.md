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
