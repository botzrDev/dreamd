# State-Drift benchmark (ANTH-20 / ANTH-1)

Week-0 bake-off gate and scaffold for the neutral State-Drift benchmark.

```bash
# Reference/floor systems (self-proving oracle)
python3 scripts/benchmark/state_drift_bench.py --demo

# Replay determinism check
python3 scripts/benchmark/state_drift_bench.py --verify-determinism

# dreamd vs Mem0 vs Zep gate (requires MEM0_API_KEY, ZEP_API_KEY)
export MEM0_API_KEY=...
export ZEP_API_KEY=...
python3 scripts/benchmark/state_drift_bench.py --bakeoff
```

`dreamd` uses a throwaway `HOME` sandbox (see `scripts/alpha/alpha-suite.sh`). Build the binary first: `cargo build -p dreamd-cli`.

Design doc: [Linear — bake-off harness v0](https://linear.app/wegetit/document/state-drift-benchmark-bake-off-harness-v0-scaffold-verified-d5babfc6700a).
