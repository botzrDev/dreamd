#!/usr/bin/env python3
"""Side-by-side table of several run.py result files.

  compare.py --metric recall_any --k 5 --gran session a.jsonl b.jsonl ...
"""
from __future__ import annotations

import argparse
import json
from collections import defaultdict
from pathlib import Path


def load(path: Path):
    rows = [json.loads(l) for l in path.read_text().splitlines() if l.strip()]
    label = rows[0]["backend"]
    if rows[0].get("cluster"):
        label += f"/{rows[0]['cluster']}"
    if rows[0].get("timestamps") == "flat":
        label += "/flat"
    return label, rows


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("files", nargs="+", type=Path)
    ap.add_argument("--metric", default="recall_any", choices=["recall_any", "recall_all", "ndcg_any"])
    ap.add_argument("--k", type=int, default=5)
    ap.add_argument("--gran", default="session", choices=["turn", "session"])
    a = ap.parse_args()
    runs = [load(f) for f in a.files]
    ids = set.intersection(*(set(r["question_id"] for r in rows) for _, rows in runs))
    types: dict[str, list[str]] = defaultdict(list)
    for r in runs[0][1]:
        if r["question_id"] in ids:
            types[r["question_type"]].append(r["question_id"])
    key = f"{a.metric}@{a.k}"
    print(f"{key} at {a.gran} granularity, n={len(ids)} questions common to all files\n")
    hdr = ["type", "n"] + [lbl for lbl, _ in runs]
    print("| " + " | ".join(hdr) + " |")
    print("|" + "---|" * len(hdr))
    order = sorted(types) + ["ALL"]
    types["ALL"] = sorted(ids)
    for t in order:
        qs = set(types[t])
        cells = [t, str(len(qs))]
        for _, rows in runs:
            vals = [r["metrics"][a.gran][key] for r in rows if r["question_id"] in qs]
            cells.append(f"{sum(vals) / len(vals):.3f}")
        print("| " + " | ".join(cells) + " |")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
