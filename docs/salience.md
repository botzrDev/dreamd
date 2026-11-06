# Salience — the recall ranking formula

A factor-by-factor account of the query-time ranking score: what it is, why each term is shaped the way it is, and what it is — and is not — descended from. The formula is locked by [ARCHITECTURE.md decision #2](../ARCHITECTURE.md#2-salience-is-query-time-not-indexed) and frozen for v0.2 in [SPEC.md §Salience](../SPEC.md#salience-formula); this page explains it, it does not propose changing it.

## Formula

Recall ranks every hit by one number:

```
final_score = bm25 * salience
```

Tantivy scores the candidate's `content` against the query with BM25; a custom collector then multiplies that by `salience`, computed at query time from three fastfields — `timestamp_sec` (age), `pain` and `importance` — and `recurrence`, which a per-searcher table supplies (`recurrence.rs`):

```
salience = exp(-age_days / 14) * (pain / 10) * (importance / 10) * (1 + ln(1 + d(recurrence)))

d(0) = 0
d(r) = 1.5 - 0.5 / r    for r >= 1
```

- `exp(-age_days / 14)` — recency, the only time-dependent term.
- `(pain / 10)` — normalised pain, in 0.0–1.0.
- `(importance / 10)` — normalised importance, in 0.0–1.0.
- `(1 + ln(1 + d(recurrence)))` — the recurrence boost, where `d(0) = 0` and `d(r) = 1.5 - 0.5 / r`; the leading `1 +` is load-bearing, not decoration (see [Derivation](#derivation)).

**Input ranges.** `salience()` clamps its inputs before the formula runs. `age_days` is floored at 0: a record dated after `now` (clock skew, a teammate's merged log, a hand edit) scores as if it were written this second. Its recency term is 1.0, never more, and it starts to decay once `now` passes its timestamp. `pain` and `importance` are held to 0–10, and a value that is not a number counts as 0. `POST /api/v1/learn` and MCP `append_node` still reject out-of-range scores; the clamp covers records that reach the index another way. It runs at query time, so it also covers documents indexed before 1.1.0. Salience therefore lies between 0 and `1 + ln(1 + d(recurrence))`, which is at most `1 + ln(2.5)` ≈ 1.916. `--explain` and `citations[].age_days` report the clamped values the score used.

Salience is computed **at query time** and is never stored or indexed. Storing it would force a nightly re-index, because `age_days` drifts continuously. `dreamd recall --explain` (and `dreamd score --explain`, `dreamd blame --explain`) print all four factors for any hit, so the number can be inspected rather than taken on faith. `explain=1` on `GET /api/v1/recall` adds a `citations` array carrying each hit's inputs (`age_days`, `pain`, `importance`, and the dampened `recurrence`) and its BM25 and salience components.

## Derivation

Each factor answers one question, and the arithmetic shape is deliberate.

**Why exponential recency.** `age_days` is `(now - timestamp) / 86400`, floored at 0 and recomputed on every recall, so the recency term must be smooth and continuous — an exponential avoids any cliff where a record is full-weight one day and gone the next, and it means the index never has to be rebuilt as records age. A step function or a nightly re-rank would trade that away.

**Why 14, and why it is an e-folding time constant — not a half-life.** `exp(-age_days / 14)` falls to `1/e` (≈ 0.37) of its starting value after 14 days; that is an **e-folding** time constant. The half-life of the same curve is `14 · ln 2 ≈ 9.7` days, so 14 is **not a half-life**, and calling it one is a factual error. The value is a product decision — a two-week horizon on "how stale is this claim" — not a fitted parameter.

**Why a product, not a sum.** `(pain / 10)` and `(importance / 10)` multiply rather than add. A product means either axis can veto: a `pain` of 0 or an `importance` of 0 collapses the whole score to 0, so an event that scores zero on either dimension is unrankable rather than merely discounted. This is also why a semantic lesson with a missing exemplar must be skipped rather than indexed with zeros — zeros would score 0.0 and never rank.

**Why `1 + ln(1 + d(recurrence))`.** The leading `1 +` keeps an event with no recurrence at full weight: `1 + ln(1 + 0) = 1.0`, not `0`. Dropping that wrapper would zero the score of every event that has not yet recurred.

**Why recurrence saturates.** `d(0) = 0` and `d(r) = 1.5 - 0.5 / r`. `d(1) = 1`, so a one-off event keeps a factor of 1.693. The factor reaches 1.811 at r=2 and 1.896 at r=10, and never passes 1.916, which is 13.2% above a one-off. Recurrence is a subtree count, so a parent key counts every event under it. Without `d`, 40 parents and one child gave each parent a factor of 4.738 against the child's 1.693, which outweighed the child's higher pain (`scripts/benchmark/recall_gate.py`, `hub_drown`). At equal BM25, pain 7 beats pain 6 only while the ratio of the two factors stays under 7/6. `ln`, `sqrt` and `r^(1/6)` all miss that at r=41, and `r^(1/10)` passes at r=41 and fails again near r=151. `d` holds at every cluster size.

**What that costs.** Recurrence is close to a tie-breaker in every recall, not only in a drowning: at equal inputs r=10 beats a one-off by 12%, where it used to win by 101%. At equal pain, importance, age and BM25, a parent still ranks ahead of its child. `d` is dreamd's own and comes from neither paper below. It was chosen against the recall gate and the coding oracle, and no real store with a fat cluster was available to test it on.

**Recurrence is not an event field.** `recurrence` is a per-cluster count, derived per phase rather than stored on the record. `RecurrenceContext` (`salience.rs`) names three policies. `RecallSubtreeCount` is recall's: the number of live episodic events whose `skill_action` equals the candidate's or extends it with `::`, all-time, computed once per searcher from the index (`recurrence.rs`). It counts the event itself, so an event scores at least 1, and a lesson gets the count of its `cluster_key`. `DreamCycleClusterSize` is the live promoted-cluster size the dream cycle ranks with, and `Decay` passes 0 conservatively. Each phase owns its own definition of "how often".

**The recency term and the 90-day pruner are different mechanisms.** The `exp(-age_days / 14)` term is evaluated at query time and never moves a record on disk. The decay pruner (`decay.rs`) is what actually archives a record off the hot path, and its gate is `!pinned && age > 90 days` — age alone, not salience. Pinned records are never archived. Keep the two apart: one is a ranking multiplier, the other is a lifecycle decision.

**Semantic documents score the same way.** The `semantic` layer (LESSONS.md lessons) is still lexical BM25 × salience — never embeddings, never a vector or cosine search. A lesson has no native `pain`/`importance`, so it inherits both from the exemplar event it cites; its `timestamp_sec` is when the lesson was derived (`LessonsFile.last_updated`), never the exemplar's age, because a lesson consolidated today from a 90-day-old event is a fresh claim. See [docs/architecture.md](architecture.md#indexing-and-salience) for the indexing details and [docs/architecture.md](architecture.md#decay) for the decay split.

## ACT-R (Anderson 2007)

The *shape* of the formula — recency combined with strength-like and frequency-like terms — is inspired by the declarative-memory activation equation in Anderson, *How Can the Human Mind Occur in the Physical Universe?* (2007). That is the extent of the debt: **lineage, not isomorphism**.

The contrast is the arithmetic. ACT-R's base-level activation is a **power law** in time — activation decays as `t^{-d}` — whereas dreamd's recency term is **exponential**, e-folding over 14 days. The two curves do not agree, and `14` is not Anderson's `d`: Anderson's `d` is a fitted decay exponent for a power law, while 14 is a product decision naming an e-folding horizon. Mapping one onto the other would be a category error.

## Park et al. 2023

The memory-stream retrieval in *Generative Agents* (Park et al., 2023, [arXiv:2304.03442](https://arxiv.org/abs/2304.03442)) is the precedent for scoring recalled memories by recency, importance, and relevance together. That is where the idea of combining those three dimensions comes from.

The contrast again is arithmetic. Park's retrieval score is a weighted **sum** of recency, importance, and relevance. dreamd's score is a **product**: BM25 stands in as the relevance term, and it is multiplied by the salience factor, never added to it. A sum discounts; a product can veto — a hit with no BM25 match scores zero regardless of how fresh, painful, or important it is.

## Calibration

The three salience inputs are 0–10 scores, and the calibration copy the MCP server already teaches agents is the whole rubric:

- `importance` — how broadly applicable across sessions; **8+ changes behavior on familiar domains.**
- `pain` — how disruptive if unknown; **8+ causes bugs, rework, or failed attempts.**
- Omitted scores default to **5.0**, the neutral midpoint (MCP `append_node` only; `POST /api/v1/learn` requires both).

`GET /api/v1/preferences` reads per-user thresholds from free-form `PREFERENCES.md` — user text, not a structured schema. There is deliberately no config key for `14`, no config key for the formula shape, and no scoring rubric beyond the MCP copy above; the formula is locked as-is. To inspect a hit's factors, use `dreamd recall --explain`; for the distribution of salience across the whole store (no BM25), use `dreamd salience-drift` or `GET /api/v1/observability/salience`.
