# WasTrue benchmark (ANTH-20 / ANTH-1)

Week-0 bake-off gate and scaffold for the neutral WasTrue benchmark.

```bash
# Reference/floor systems (self-proving oracle)
python3 scripts/benchmark/wastrue_bench.py --demo

# Replay determinism check
python3 scripts/benchmark/wastrue_bench.py --verify-determinism

# dreamd vs Mem0 vs Zep gate (requires MEM0_API_KEY, ZEP_API_KEY)
export MEM0_API_KEY=...
export ZEP_API_KEY=...
python3 scripts/benchmark/wastrue_bench.py --bakeoff
```

`dreamd` uses a throwaway `HOME` sandbox (see `scripts/alpha/alpha-suite.sh`). Build the binary first: `cargo build -p dreamd --bin dreamd`. The harness looks for it at `target/debug/dreamd`, so this must be a debug build.

`--demo` and `--verify-determinism` need no API keys. `--trials N` sets the trials per scenario. For `--bakeoff`, the keys can also come from a file of `export KEY=VAL` lines: pass `--env-file PATH`, or run `bash scripts/benchmark/run_bakeoff.sh [ENV_FILE]`, which sources the file (default `/tmp/anth20_bakeoff.env`) and then runs `--bakeoff`.

Design doc: [Linear — bake-off harness v0](https://linear.app/wegetit/document/state-drift-benchmark-bake-off-harness-v0-scaffold-verified-d5babfc6700a).

## Recall latency summary

The other two files here are unrelated to WasTrue. `recall_summary.py` runs the Criterion recall bench (`cargo bench -p dreamd-core --bench recall`) and prints sample P50/P99 per corpus size; `--summarize DIR` reads existing samples without running cargo. Usage and method are in [`docs/benchmarks.md`](../../docs/benchmarks.md). Its unit tests are `python3 scripts/benchmark/test_recall_summary.py`.
