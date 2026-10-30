# External benchmarks — working notes

Branch `bench/external-benchmarks`, started 2026-10-29 after the v1.2.0 release.
Goal: put dreamd's recall up against published memory benchmarks with a
protocol someone else can re-run, and publish only numbers we can defend.

This file is the running log. The recall latency bench is documented in
[`../benchmarks.md`](../benchmarks.md); the WasTrue harness in
[`../../scripts/benchmark/README.md`](../../scripts/benchmark/README.md).

## Shortlist (2026-10-29)

| Benchmark | Tests | Code | Already on it | Fit |
|---|---|---|---|---|
| [LongMemEval](https://github.com/xiaowu0162/LongMemEval) (ICLR 2025) | 500 questions over ~48-session chat histories: extraction, multi-session, knowledge update, temporal, abstention | MIT | Mem0, Zep, Letta, others (self-reported) | **First target.** Ships a `flat-bm25` retriever and judge-free retrieval metrics |
| [MemoryCode](https://github.com/Cohere-Labs-Community/MemoryCode) (ACL 2025) | Coding instructions scattered across mentor sessions; instructions get superseded | public | raw LLMs | Second target. Coding-shaped, same drift story as WasTrue |
| [DreamBench-SWE](https://github.com/iroiro147/dreambench-swe) (2026) | Multi-session SWE tasks needing earlier-session evidence; executable oracles | Apache-2.0 | Mem0 hosted, verbatim-event baseline | Closest to the pitch; single author, plugin interface undocumented |
| [MemoryAgentBench](https://github.com/HUST-AI-HYZ/MemoryAgentBench) (ICLR 2026) | Accurate retrieval, test-time learning, long-range understanding, conflict resolution | MIT | Mem0, Letta, Cognee, HippoRAG | Conflict-resolution track is the interesting one |
| [LoCoMo](https://arxiv.org/abs/2402.17753) (ACL 2024) | 10 long two-person conversations, 5 QA categories | public | everyone | Quote because people ask; scoring is contested between vendors |

Set aside: VibeMemBench and SWE-ContextBench (no public harness found),
BEIR (generic document retrieval), the Hugging Face "Agent Memory Bench" space
(no README or results). [recallbench](https://docs.rs/crate/recallbench) is a
Rust harness that wraps LongMemEval, LoCoMo and MemoryAgentBench behind a
subprocess/HTTP adapter; worth a look once we have our own numbers.

## Ground rules

- **Report retrieval metrics first** (recall@k, nDCG@k). dreamd is a retriever.
  QA accuracy needs a reader LLM and a judge, and both become part of the
  system under test. That is where the vendor numbers stop being comparable.
- **Say what the formula is doing.** Chat turns carry no pain or importance,
  so those two factors are constant on these benchmarks. What is left is
  BM25 × recency × recurrence. Say so in any writeup.
- **Same corpus, same query, same scorer.** The reference BM25 row and the
  dreamd row must see the same documents and the same raw question, and be
  scored by the same metric code.
- **No vendor numbers in our table** unless we re-ran them ourselves.

## LongMemEval — protocol decisions

Reference protocol (`src/retrieval/run_retrieval.py` in their repo):

- corpus per question = the **user** turns of every haystack session
  (`turn` granularity) or the user turns joined per session (`session`);
- query = the raw `question` string;
- `flat-bm25` = `rank_bm25.BM25Okapi` over `doc.split(" ")`, no lowercasing;
- a doc is correct when its id contains `answer`;
- metrics: `recall_any@k`, `recall_all@k`, `ndcg_any@k` for k in 1,3,5,10,30,50;
  turn-level runs are also projected to session level.

dreamd mapping:

- **One learning per user turn**, `source_harness = longmemeval`,
  `pain = importance = 5.0` (constant, so they cancel in ranking).
- **Timestamps come from `haystack_dates`.** The daemon stamps its own clock
  on `POST /learn`, which would flatten recency to zero. So the harness writes
  `AGENT_LEARNINGS.jsonl` directly, with ids as ULIDs derived from those
  timestamps, and lets dreamd build the index from the log. This is an
  import, not the adapter path; it is the only way to give the store a
  history that spans weeks.
- **Turns over ~3,000 bytes are chunked** into several learnings (the line
  cap is 4,096 bytes). The first hit of any chunk counts as the turn.
- **`skill_action`** is a harness flag: `turn` (unique per turn, recurrence
  = 1, so ranking is BM25 × recency) or `session` (one key per session, so
  recurrence = the session's turn count). Report both as separate rows.
- Ranks come from `dreamd blame -k 50 --json` run in the question's store,
  with the process clock as `now`. Ages are offset by the same constant for
  every doc, and recency ratios between docs are unchanged.
- The query is sanitised to plain words before it reaches dreamd, because
  Tantivy's parser treats `:` and quotes as syntax.

Sizes (`longmemeval_s_cleaned.json`): 500 questions; mean 47.7 sessions and
245 user turns per question; 122,416 user turns in total; mean turn 251
bytes, p99 1,291, 619 turns over 3,000 bytes.

## Log

- 2026-10-29 — branch created; shortlist above; LongMemEval repo and the
  cleaned S + oracle files pulled into the scratchpad; venv with
  `rank_bm25` for the reference row; harness skeleton at
  `scripts/benchmark/longmemeval/run.py`.
- 2026-10-29 — harness runs end to end. Gotchas found on the way:
  `dreamd blame` and `recall` refuse a store with no index, so the harness
  builds it with `doctor --repair`; doctor exits 1 unless the `init` layout
  (`working/ semantic/ personal/ .dreamd/`) exists, so the harness scaffolds
  it. Ages are ~1,250 days, so every salience is ~1e-40; still ordered, and
  `blame --json` carries the full float. The metric port was transcribed by
  hand from `eval_utils.py`; `check_metrics.py --ref-dir …` compares the two
  on random rankings (not run in the session that wrote it: the sandbox
  declined to execute the downloaded file).
- 2026-10-29 — reference rows on the cleaned S set (470 questions after the
  abstention skip). Paper Table 9 (BM25, K = V, session): R@5 0.634,
  nDCG@10 0.540. Ours on the 2025-09 cleaned data: R@5 0.662, nDCG@10
  0.510. Same ballpark; not a reproduction, because the data release and
  the session unit changed. `single-session-assistant` is ~0.04 for every
  lexical row, because the reference corpus holds only user turns and the
  answer is in the assistant turn. Report it, do not hide it.

  | row | turn R@5 | turn nDCG@10 | session R@5 | session nDCG@10 |
  |---|---|---|---|---|
  | bm25 (paper's raw split) | 0.638 | 0.503 | 0.662 | 0.510 |
  | bm25-norm (lowercase `\w+`) | 0.745 | 0.607 | 0.770 | 0.614 |

  Tokenisation alone is worth ~10 points, which is why `bm25-norm` is the
  row dreamd has to beat.
- 2026-10-29 — **first full LongMemEval_S run, dreamd 1.2.0 (`2ab0270`).**
  Tables in [`results/longmemeval-s-2026-10-29/`](results/longmemeval-s-2026-10-29/)
  (`compare.md` is the side-by-side); raw per-question rows under
  `context/benchmarks/` (gitignored, 3 MB). Session granularity, 470 questions:

  | row | what it is | R@5 | nDCG@10 |
  |---|---|---|---|
  | bm25 | paper's `flat-bm25`, raw split | 0.662 | 0.510 |
  | bm25-norm | rank_bm25, lowercase `\w+` | 0.770 | 0.614 |
  | dreamd/turn/flat | Tantivy BM25 only (all timestamps equal) | 0.768 | 0.610 |
  | dreamd/turn | BM25 × recency (haystack dates) | 0.738 | 0.544 |
  | dreamd/session | BM25 × recency × session recurrence | 0.543 | 0.330 |

  Reading:
  - `dreamd/turn/flat` matches `bm25-norm` within 0.004 everywhere. The
    pipeline is sound and Tantivy's BM25 is on par with rank_bm25. That row
    is the control, not a result to publish.
  - **The 14-day e-folding hurts on this benchmark.** Knowledge-update
    nDCG@10 goes 0.789 → 0.519 and temporal-reasoning 0.684 → 0.637. The
    answer sessions for those types are old: knowledge-update median 22
    days before the question, 39% past 28 days; temporal-reasoning 28% past
    28 days. A 60-day-old answer carries a factor of e^-4.3 ≈ 0.014 against
    a day-old filler turn at 0.93, and no BM25 gap survives that. The other
    four types have answers under 10 days old and lose 0–6 points.
  - **Recurrence keyed by session is harmful**: it rewards long filler
    sessions. On a coding store the cluster key is a topic, not a session,
    so this row says nothing about dreamd's real use; it says the mapping
    is wrong. Drop it from any writeup, or keep it as the "wrong key" row.
  - `single-session-assistant` sits at ~0.05 for every lexical row because
    the reference corpus is user turns only. Not a dreamd problem.
  - Index build p50 0.21 s per ~250-doc store; query p50 4 ms; the whole
    470-question run takes under a minute with 8 workers.

  What this means for the product (founder calls, not harness work):
  - The salience formula is tuned for coding lessons that rot in weeks. On
    a personal-facts benchmark with month-long histories, recency at query
    time removes evidence instead of ranking it. Either the e-folding is
    too short for this kind of store, or recency belongs in decay and the
    dream cycle rather than in every recall. Do not change the formula on
    the strength of one benchmark; ARCHITECTURE decision #2 locks it.
  - A sweep over the time constant (14 → 30 → 90 → ∞ days) on this harness
    would show the shape of the trade-off in a few minutes of compute.
    It needs a knob the binary does not have; a bench-only env var would do.
  - Nothing here is publishable as "dreamd beats X". The publishable
    statement today is "dreamd's lexical recall matches a tuned BM25
    baseline on LongMemEval_S retrieval, and the salience terms are a
    coding-store prior that this benchmark does not reward".

  Next: MemoryCode (coding-shaped, instruction supersession), where the
  pain / importance / topic-key mapping is natural and recency should pull
  its weight.
