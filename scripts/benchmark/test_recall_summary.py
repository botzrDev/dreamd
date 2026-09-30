#!/usr/bin/env python3
"""Tests for recall_summary.py (BZR-177). Run: python3 scripts/benchmark/test_recall_summary.py"""
from __future__ import annotations

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import recall_summary  # noqa: E402

SCRIPT = HERE / "recall_summary.py"


def write_sample(root: Path, n: int, iters: list[float], times: list[float]) -> None:
    path = root / "recall" / "n" / str(n) / "new" / "sample.json"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps({"sampling_mode": "Linear", "iters": iters, "times": times}))


def run_summarize(root: Path) -> subprocess.CompletedProcess:
    return subprocess.run(
        [sys.executable, str(SCRIPT), "--summarize", str(root)],
        capture_output=True,
        text=True,
    )


class RecallSummaryTests(unittest.TestCase):
    def test_percentile_index(self) -> None:
        values = recall_summary.per_iteration_ns(
            {"iters": [1.0] * 100, "times": [float(v) for v in range(100)]}
        )
        p50 = recall_summary.percentile(values, 50)
        p99 = recall_summary.percentile(values, 99)
        self.assertEqual(p50, 50.0)
        self.assertEqual(p99, 98.0)
        self.assertEqual(int(p50 + 0.5), 50)
        self.assertEqual(int(p99 + 0.5), 98)

    def test_divides_by_iters(self) -> None:
        values = recall_summary.per_iteration_ns({"iters": [2.0], "times": [200.0]})
        self.assertEqual(recall_summary.percentile(values, 50), 100.0)
        self.assertEqual(recall_summary.percentile(values, 99), 100.0)

    def test_summarize_three_sizes(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write_sample(root, 1000, [1.0, 2.0, 4.0], [50.0, 120.0, 260.0])
            write_sample(root, 10000, [10.0, 10.0], [3000.0, 3400.0])
            write_sample(root, 100000, [1.0], [2800.0])
            proc = run_summarize(root)
            self.assertEqual(proc.returncode, 0, proc.stderr)
            self.assertEqual(
                proc.stdout,
                "n=1000 p50_ns=60 p99_ns=65\n"
                "n=10000 p50_ns=340 p99_ns=340\n"
                "n=100000 p50_ns=2800 p99_ns=2800\n",
            )

    def test_missing_sample_exits_1(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write_sample(root, 1000, [1.0], [10.0])
            write_sample(root, 10000, [1.0], [20.0])
            proc = run_summarize(root)
            self.assertEqual(proc.returncode, 1)
            self.assertIn("recall/n/100000/new/sample.json", proc.stderr)
            self.assertEqual(proc.stdout, "")

    def test_zero_iters_exits_1(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write_sample(root, 1000, [1.0], [10.0])
            write_sample(root, 10000, [0.0], [20.0])
            write_sample(root, 100000, [1.0], [30.0])
            proc = run_summarize(root)
            self.assertEqual(proc.returncode, 1)
            self.assertIn("bad sample", proc.stderr)
            self.assertEqual(proc.stdout, "")


if __name__ == "__main__":
    unittest.main()
