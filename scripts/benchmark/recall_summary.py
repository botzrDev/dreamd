#!/usr/bin/env python3
"""Recall latency summary — sample P50/P99 from the in-RAM Criterion bench (BZR-177).

Reads Criterion 0.5 `sample.json` for `cargo bench -p dreamd-core --bench recall`
and prints one line per corpus size. Per-iteration time is `times[i] / iters[i]`.
`estimates.json` is not read: its median is a bootstrap estimate, not the
sample percentile.

Reproduce:
  python3 scripts/benchmark/recall_summary.py                  # run the bench, then summarize
  python3 scripts/benchmark/recall_summary.py --summarize DIR  # read DIR only, no cargo
"""
from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path

SIZES = (1000, 10000, 100000)
USAGE = "usage: recall_summary.py [--summarize DIR]"

# scripts/benchmark/recall_summary.py -> repo root is the parent of scripts/.
REPO_ROOT = Path(__file__).resolve().parent.parent.parent


class SampleError(Exception):
    """A sample file is missing or malformed; the message is the stderr line."""


def percentile(sorted_values: list[float], p: int) -> float:
    """Nearest-rank percentile on an ascending list. Rounds half up, never `round`."""
    n = len(sorted_values)
    idx = int((p / 100.0) * (n - 1) + 0.5)
    if idx >= n:
        idx = n - 1
    if idx < 0:
        idx = 0
    return sorted_values[idx]


def per_iteration_ns(sample: dict) -> list[float]:
    """`times[i]` is the total for `iters[i]` iterations; divide, then sort ascending."""
    return sorted(t / i for t, i in zip(sample["times"], sample["iters"]))


def sample_path(criterion_dir: Path, n: int) -> Path:
    return criterion_dir / "recall" / "n" / str(n) / "new" / "sample.json"


def load_sample(path: Path) -> dict:
    if not path.is_file():
        raise SampleError(f"missing sample: {path}")
    try:
        raw = json.loads(path.read_text())
        iters = [float(x) for x in raw["iters"]]
        times = [float(x) for x in raw["times"]]
    except (OSError, ValueError, TypeError, KeyError):
        raise SampleError(f"bad sample: {path}") from None
    if not iters or len(iters) != len(times) or any(i <= 0 for i in iters):
        raise SampleError(f"bad sample: {path}")
    return {"iters": iters, "times": times}


def summarize(criterion_dir: Path) -> list[str]:
    """Build all three lines before printing any, so a bad file prints nothing."""
    lines = []
    for n in SIZES:
        values = per_iteration_ns(load_sample(sample_path(criterion_dir, n)))
        p50 = int(percentile(values, 50) + 0.5)
        p99 = int(percentile(values, 99) + 0.5)
        lines.append(f"n={n} p50_ns={p50} p99_ns={p99}")
    return lines


def run_summarize(criterion_dir: Path) -> int:
    try:
        lines = summarize(criterion_dir)
    except SampleError as err:
        print(err, file=sys.stderr)
        return 1
    sys.stdout.write("\n".join(lines) + "\n")
    return 0


def main(argv: list[str]) -> int:
    if not argv:
        # Criterion writes ./target/criterion from the process cwd and ignores
        # CARGO_TARGET_DIR, so run from the repo root and read there.
        code = subprocess.call(
            ["cargo", "bench", "-p", "dreamd-core", "--bench", "recall"],
            cwd=REPO_ROOT,
        )
        if code != 0:
            return code
        return run_summarize(REPO_ROOT / "target" / "criterion")
    if argv[0] == "--summarize" and len(argv) == 2:
        return run_summarize(Path(argv[1]))
    print(USAGE, file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
