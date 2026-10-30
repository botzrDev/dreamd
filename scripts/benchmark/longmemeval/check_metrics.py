#!/usr/bin/env python3
"""Cross-check run.py's metric port against LongMemEval's own eval_utils.py.

  check_metrics.py --ref-dir /path/to/LongMemEval/src/retrieval

Loads both modules, scores 3000 random rankings with each, and reports
mismatches. Needs numpy (the reference imports it).
"""
from __future__ import annotations

import argparse
import importlib.util
import random
from pathlib import Path


def load(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    mod = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(mod)
    return mod


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--ref-dir", type=Path, required=True, help="LongMemEval/src/retrieval")
    ap.add_argument("--trials", type=int, default=3000)
    a = ap.parse_args()
    ref = load("ref_eval", a.ref_dir / "eval_utils.py")
    mine = load("mine", Path(__file__).with_name("run.py"))
    rng = random.Random(7)
    bad = comparisons = 0
    for _ in range(a.trials):
        n = rng.randint(5, 80)
        ids = []
        for i in range(n):
            s = f"s{rng.randint(0, 9)}"
            if rng.random() < 0.15:
                s = "answer_" + s
            ids.append(f"{s}_{i + 1}")
        correct = list(set(d for d in ids if "answer" in d))
        if not correct:
            continue
        rk = list(range(n))
        rng.shuffle(rk)
        for k in (1, 3, 5, 10, 30, 50):
            pairs = [
                (ref.evaluate_retrieval(rk, correct, ids, k), mine.evaluate_retrieval(rk, correct, ids, k)),
                (ref.evaluate_retrieval_turn2session(rk, correct, ids, k),
                 mine.evaluate_retrieval_turn2session(rk, correct, ids, k)),
            ]
            comparisons += 1
            if any(abs(x - y) > 1e-9 for p, q in pairs for x, y in zip(p, q)):
                bad += 1
                if bad <= 3:
                    print("MISMATCH", k, pairs)
    print(f"{comparisons} comparisons, {bad} mismatches")
    return 1 if bad else 0


if __name__ == "__main__":
    raise SystemExit(main())
