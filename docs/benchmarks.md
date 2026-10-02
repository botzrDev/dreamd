# Recall latency benchmark

This page explains how to re-run dreamd's recall latency bench and read its
results. It uses the existing Criterion bench, `crates/dreamd-core/benches/recall.rs`.
There is no second bench target and no CI job that runs it.

## Running it

From the repo root, run `cargo bench -p dreamd-core --bench recall`. To run the
bench and get the summary in one step, run
`python3 scripts/benchmark/recall_summary.py`. It runs that same cargo command
from the repo root, passes cargo's output through, and then prints three lines:

```text
n=1000 p50_ns=<int> p99_ns=<int>
n=10000 p50_ns=<int> p99_ns=<int>
n=100000 p50_ns=<int> p99_ns=<int>
```

If cargo fails, the script exits with cargo's exit code and prints no summary.

`python3 scripts/benchmark/recall_summary.py --summarize DIR` reads existing
samples under `DIR` (a Criterion output directory such as `target/criterion`)
and does not run cargo. A missing or malformed `sample.json` exits 1, names
the file on stderr, and prints nothing on stdout. Any other argument form
prints a usage line and exits 2.

## What it measures

The corpus is built in RAM with `Index::create_in_ram`, so disk speed does not
affect the result. The bench builds corpora of 1000, 10000, and 100000
episodic documents. Each one runs the query `rust error` with `k` set to 10,
through the same `recall()` path and collector as the production indexer.

Criterion stores each sample in `sample.json` as the total nanoseconds
(`times[i]`) for a batch of iterations (`iters[i]`). The printer divides
`times[i]` by `iters[i]` to get the per-iteration time, sorts the results, and
reports the sample P50 and P99. The index is `int((p / 100) * (n - 1) + 0.5)`,
which rounds halves up. The printer does not read `estimates.json`, because
its median is a bootstrap estimate and not a sample percentile.

## Where the samples are

Criterion 0.5 writes under `criterion/` in cargo's target directory, so by
default samples land in `<repo>/target/criterion/recall/n/<size>/new/sample.json`.
The printer reads `<repo>/target/criterion` when you run it with no arguments.

If `CARGO_TARGET_DIR` is set, Criterion writes to `$CARGO_TARGET_DIR/criterion`
instead, and if `CRITERION_HOME` is set it writes there. The no-argument run
does not look in either place and fails with `missing sample: …`. Unset them,
or point the printer at the directory:
`python3 scripts/benchmark/recall_summary.py --summarize "$CARGO_TARGET_DIR/criterion"`.

## Relation to PERF.md

The recall cells in [`../PERF.md`](../PERF.md) (P50 ~0.30 ms and P99 ~0.34 ms
at 10k, measured 2026-07-28 at commit `e8e27fd`) are a historical stamp. This
page does not replace them. A fresh run on your machine gives you your own
numbers. It does not update that table.

## What this page does not cover

This page does not report vector recall or the WasTrue eval. WasTrue is a
separate harness at `scripts/benchmark/wastrue_bench.py`. Recall is still
BM25 × salience, and the `vectors` feature is not part of this run.

There is no website for these numbers. This file is the page.
