# LongMemEval retrieval harness

Runs dreamd's recall through the retrieval protocol of
[LongMemEval](https://github.com/xiaowu0162/LongMemEval) (ICLR 2025) and
prints the same metrics their `print_retrieval_metrics.py` reports:
`recall_any@k` and `ndcg_any@k` for k in 1, 3, 5, 10, 30, 50, at turn
granularity and projected to session granularity, grouped by question type.
The 30 abstention questions are skipped, as the reference does.

Decisions and the running log are in
[`docs/evals/external-benchmarks.md`](../../../docs/evals/external-benchmarks.md).

## Setup

```bash
# data (the 2025-09 cleaned release)
mkdir -p /path/to/lme && cd /path/to/lme
curl -sSLO https://huggingface.co/datasets/xiaowu0162/longmemeval-cleaned/resolve/main/longmemeval_s_cleaned.json

# reference rows need rank_bm25
python3 -m venv venv && venv/bin/pip install rank_bm25 numpy

# dreamd row needs a release binary
cargo build --release -p dreamd --bin dreamd
```

## Run

```bash
H=scripts/benchmark/longmemeval/run.py
D=/path/to/lme/longmemeval_s_cleaned.json

venv/bin/python -I $H --data $D --backend bm25      --out out/bm25.jsonl       # paper's flat-bm25
venv/bin/python -I $H --data $D --backend bm25-norm --out out/bm25-norm.jsonl  # same, lowercased \w+ tokens
python3        -I $H --data $D --backend dreamd --cluster turn    --out out/dreamd-turn.jsonl
python3        -I $H --data $D --backend dreamd --cluster session --out out/dreamd-session.jsonl

python3 $H --summarize out/dreamd-turn.jsonl   # re-print the tables
```

`--limit N` and `--offset N` slice the question list. `--jobs` sets the
worker count (default: half the cores, at most 8). `--keep-stores` leaves
each question's `.agent/` under `--workdir` for inspection.

## What the dreamd row does

For each question the harness writes a fresh project with one learning per
**user** turn (the reference corpus), `pain = importance = 5.0`,
`source_harness = longmemeval`, and the haystack session's date as the
timestamp. Turns whose serialized line would pass 4 KiB are split into
several learnings; the first chunk to rank counts as the turn. The index is
built with `dreamd doctor --repair` and the ranking read from
`dreamd blame -k <all> --json`, with the question sanitised to plain
lowercase words because Tantivy's parser treats `:` and quotes as syntax.

`--cluster turn` gives every turn its own `skill_action`, so recurrence is 1
and the ranking is BM25 × recency. `--cluster session` keys every turn of a
session together, so recurrence is the session's turn count.

The log is written directly rather than through `POST /api/v1/learn`,
because the daemon stamps its own clock on every append and the benchmark
needs a history that spans weeks. This is a benchmark import, not the way
adapters write (`doc-first-append-via-uds-learn` in `AGENTS.md`).
