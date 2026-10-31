#!/usr/bin/env python3
"""LongMemEval retrieval harness for dreamd.

The functions `process_item_flat_index`, `dcg`, `ndcg`, `evaluate_retrieval`
and `evaluate_retrieval_turn2session` are transcribed from LongMemEval's
`src/retrieval/run_retrieval.py` and `src/retrieval/eval_utils.py`
(https://github.com/xiaowu0162/LongMemEval), which are MIT licensed:
Copyright (c) 2024 Di Wu. Permission is hereby granted, free of charge, to any
person obtaining a copy of this software and associated documentation files,
to deal in the Software without restriction, subject to the condition that the
above copyright notice and this permission notice be included in all copies or
substantial portions of the Software. The Software is provided "as is", without
warranty of any kind. The rest of this file is Apache-2.0 like the repository.

Re-implements the retrieval protocol of LongMemEval's
`src/retrieval/run_retrieval.py` (corpus construction, correct-doc labels,
recall@k / nDCG@k, turn-to-session projection) and runs it against three
backends that all see the same corpus and the same raw question:

  bm25        the reference `flat-bm25` row: rank_bm25.BM25Okapi over
              `doc.split(" ")`, no lowercasing (what the paper ran);
  bm25-norm   the same library over lowercased `\\w+` tokens, which is what
              Tantivy's default tokenizer does, so the dreamd row cannot win
              on tokenisation alone;
  dreamd      one learning per user turn written to a fresh `.agent/` store
              with the haystack date as its timestamp, the index built with
              `dreamd doctor --repair`, ranks from `dreamd blame --json`.

The dreamd row writes `AGENT_LEARNINGS.jsonl` directly. The daemon stamps
its own clock on every `POST /learn`, which would make every turn the same
age and switch recency off. Writing the log is an import, not the adapter
path; see docs/evals/external-benchmarks.md.

Usage:
  run.py --data longmemeval_s_cleaned.json --backend dreamd --out out.jsonl
  run.py --summarize out.jsonl
"""
from __future__ import annotations

import argparse
import json
import math
import multiprocessing as mp
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
from collections import defaultdict
from dataclasses import dataclass
from pathlib import Path

KS = (1, 3, 5, 10, 30, 50)
CROCKFORD = "0123456789ABCDEFGHJKMNPQRSTVWXYZ"
MAX_LINE_BYTES = 4096
CHUNK_BUDGET = 3300  # serialized-line budget per chunk, leaves headroom under MAX_LINE_BYTES
FLAT_TS = 1_700_000_000  # --timestamps flat: every learning gets this, so recency cancels
REPO_ROOT = Path(__file__).resolve().parents[3]


# ── Reference protocol (ported from LongMemEval src/retrieval) ───────────────


def process_item_flat_index(data: list[dict], sess_id: str, timestamp: str):
    """Turn-granularity corpus for one session. Verbatim port."""
    corpus, ids = [], []
    for i_turn, turn in enumerate(data):
        if turn["role"] == "user":
            corpus.append(turn["content"])
            if "answer" not in sess_id:
                ids.append(sess_id + "_" + str(i_turn + 1))
            else:
                assert turn["has_answer"] in (True, False)
                if turn["has_answer"]:
                    ids.append(sess_id + "_" + str(i_turn + 1))
                else:
                    ids.append((sess_id + "_" + str(i_turn + 1)).replace("answer", "noans"))
    return corpus, ids, [timestamp for _ in corpus]


def dcg(relevances: list[int], k: int) -> float:
    rel = relevances[:k]
    if not rel:
        return 0.0
    return rel[0] + sum(r / math.log2(i + 2) for i, r in enumerate(rel[1:], start=0))


def ndcg(rankings, correct_docs, corpus_ids, k=10) -> float:
    relevances = [1 if doc_id in correct_docs else 0 for doc_id in corpus_ids]
    sorted_rel = [relevances[idx] for idx in rankings[:k]]
    ideal = sorted(relevances, reverse=True)
    ideal_dcg = dcg(ideal, k)
    if ideal_dcg == 0:
        return 0.0
    return dcg(sorted_rel, k) / ideal_dcg


def evaluate_retrieval(rankings, correct_docs, corpus_ids, k=10):
    recalled = set(corpus_ids[idx] for idx in rankings[:k])
    recall_any = float(any(d in recalled for d in correct_docs))
    recall_all = float(all(d in recalled for d in correct_docs))
    return recall_any, recall_all, ndcg(rankings, correct_docs, corpus_ids, k)


def evaluate_retrieval_turn2session(rankings, correct_docs, corpus_ids, k=10):
    def strip_turn_id(docid: str) -> str:
        return "_".join(docid.split("_")[:-1])

    correct_docs = list(set(strip_turn_id(x) for x in correct_docs))
    corpus_ids = [strip_turn_id(x) for x in corpus_ids]
    effective_k = k
    unique = set(corpus_ids[idx] for idx in rankings[:effective_k])
    while effective_k <= len(corpus_ids) and len(unique) < k:
        effective_k += 1
        unique = set(corpus_ids[idx] for idx in rankings[:effective_k])
    return evaluate_retrieval(rankings, correct_docs, corpus_ids, k=effective_k)


# ── Backends ─────────────────────────────────────────────────────────────────


def norm_tokens(text: str) -> list[str]:
    """Approximates Tantivy's default tokenizer: split on non-word, lowercase,
    drop tokens longer than 40 chars."""
    return [t for t in re.findall(r"\w+", text.lower()) if len(t) <= 40]


def rank_bm25_backend(query: str, corpus: list[str], normalised: bool) -> list[int]:
    import numpy as np
    from rank_bm25 import BM25Okapi

    if normalised:
        tokenized = [norm_tokens(d) for d in corpus]
        q = norm_tokens(query)
    else:
        tokenized = [d.split(" ") for d in corpus]
        q = query.split(" ")
    bm25 = BM25Okapi(tokenized)
    scores = bm25.get_scores(q)
    return [int(i) for i in np.argsort(scores)[::-1]]


def parse_date(s: str) -> int:
    """'2023/05/20 (Sat) 02:21' -> unix seconds (treated as UTC)."""
    m = re.match(r"(\d{4})/(\d{2})/(\d{2}) \(\w+\) (\d{2}):(\d{2})", s)
    if not m:
        raise ValueError(f"bad haystack date: {s!r}")
    y, mo, d, h, mi = (int(x) for x in m.groups())
    import calendar

    return calendar.timegm((y, mo, d, h, mi, 0))


def ulid(ts_sec: int, seq: int) -> str:
    n = ((ts_sec * 1000) << 80) | (seq & ((1 << 80) - 1))
    return "".join(CROCKFORD[(n >> (5 * (25 - j))) & 31] for j in range(26))


def iso(ts_sec: int) -> str:
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(ts_sec))


def chunk_content(content: str, overhead: int) -> list[str]:
    """Split content so each serialized line stays under the cap."""
    budget = CHUNK_BUDGET - overhead

    def enc_len(s: str) -> int:
        return len(json.dumps(s, ensure_ascii=False).encode("utf-8"))

    if enc_len(content) <= budget:
        return [content]
    chunks, cur = [], []
    for word in content.split(" "):
        trial = " ".join(cur + [word])
        if cur and enc_len(trial) > budget:
            chunks.append(" ".join(cur))
            cur = [word]
        else:
            cur.append(word)
        # a single word longer than the budget gets hard-cut
        while enc_len(" ".join(cur)) > budget and len(cur) == 1:
            w = cur[0]
            chunks.append(w[: len(w) // 2])
            cur = [w[len(w) // 2 :]]
    if cur:
        chunks.append(" ".join(cur))
    return chunks


def sanitize_query(q: str) -> str:
    """Tantivy's parser treats `:` `"` `(` `)` and AND/OR as syntax."""
    return " ".join(norm_tokens(q))


@dataclass
class DreamdCfg:
    binary: Path
    cluster: str  # "turn" | "session"
    timestamps: str  # "haystack" | "flat"
    workdir: Path
    keep: bool


def dreamd_backend(entry: dict, corpus: list[str], corpus_ids: list[str],
                   corpus_ts: list[str], query: str, cfg: DreamdCfg) -> tuple[list[int], dict]:
    """Returns (rankings over corpus indices, diagnostics)."""
    store = Path(tempfile.mkdtemp(prefix=f"lme-{entry['question_id']}-", dir=cfg.workdir))
    home = store / "home"
    proj = store / "proj"
    # The layout `dreamd init` scaffolds; doctor exits 1 when a subdir is missing.
    for sub in ("episodic", "working", "semantic", "personal", ".dreamd"):
        (proj / ".agent" / sub).mkdir(parents=True)
    home.mkdir()

    # Ingest: ids are ULIDs from the session timestamp; `seq` keeps the order
    # within a second strictly increasing for the id minter's sake.
    lines: list[str] = []
    evt_to_idx: dict[str, int] = {}
    session_of: dict[int, str] = {}
    for idx, cid in enumerate(corpus_ids):
        session_of[idx] = "_".join(cid.split("_")[:-1])
    seq = 0
    skill_prefix = "lme"
    n_chunks = 0
    for idx, (content, cid, ts) in enumerate(zip(corpus, corpus_ids, corpus_ts)):
        ts_sec = parse_date(ts) if cfg.timestamps == "haystack" else FLAT_TS
        if cfg.cluster == "session":
            sa = f"{skill_prefix}::{session_of[idx].replace('-', '_').lower()}"
        else:
            sa = f"{skill_prefix}::{session_of[idx].replace('-', '_').lower()}::t{idx}"
        sa = re.sub(r"[^a-z0-9_:]", "_", sa)
        base = {
            "schema_version": "1.0.0",
            "id": "evt_" + "0" * 26,
            "timestamp": iso(ts_sec),
            "pain": 5.0,
            "importance": 5.0,
            "pinned": False,
            "skill_action": sa,
            "source_harness": "longmemeval",
            "content": "",
        }
        overhead = len(json.dumps(base, ensure_ascii=False).encode("utf-8"))
        for chunk in chunk_content(content, overhead):
            seq += 1
            rec = dict(base)
            rec["id"] = "evt_" + ulid(ts_sec, seq)
            rec["content"] = chunk
            line = json.dumps(rec, ensure_ascii=False)
            assert len(line.encode("utf-8")) < MAX_LINE_BYTES, len(line.encode("utf-8"))
            lines.append(line)
            evt_to_idx[rec["id"]] = idx
            n_chunks += 1
    # The log must be in id order for the minter and the ULID carries the
    # session timestamp, so sort by id (haystack is already time-sorted; this
    # is belt and braces).
    lines.sort(key=lambda l: json.loads(l)["id"])
    (proj / ".agent" / "episodic" / "AGENT_LEARNINGS.jsonl").write_text(
        "\n".join(lines) + "\n", encoding="utf-8"
    )

    env = dict(os.environ)
    env["HOME"] = str(home)
    env["DREAMD_LOG"] = "error"
    t0 = time.monotonic()
    r = subprocess.run([str(cfg.binary), "doctor", "--repair"], cwd=proj, env=env,
                       capture_output=True, text=True)
    if r.returncode != 0:
        raise RuntimeError(f"doctor --repair failed ({r.returncode}):\n{r.stdout[-1200:]}\n{r.stderr[-800:]}")
    t_index = time.monotonic() - t0

    q = sanitize_query(query)
    t0 = time.monotonic()
    r = subprocess.run([str(cfg.binary), "blame", q, "-k", str(len(lines)), "--json"],
                       cwd=proj, env=env, capture_output=True, text=True)
    t_query = time.monotonic() - t0
    if r.returncode != 0:
        raise RuntimeError(f"blame failed ({r.returncode}): {r.stderr[-800:]}")
    # blame prints the JSON array on stdout; tolerate stray log lines around it
    out = r.stdout.strip()
    start = out.find("[")
    hits = json.loads(out[start:]) if start >= 0 else []

    seen: set[int] = set()
    rankings: list[int] = []
    for h in hits:
        idx = evt_to_idx.get(h["id"])
        if idx is None or idx in seen:
            continue
        seen.add(idx)
        rankings.append(idx)
    # Unmatched docs follow in the same order rank_bm25's argsort would put
    # zero-score docs (reverse corpus order), so k-cuts behave alike.
    for idx in range(len(corpus) - 1, -1, -1):
        if idx not in seen:
            rankings.append(idx)

    diag = {"n_docs": len(corpus), "n_lines": n_chunks, "n_hits": len(hits),
            "t_index_s": round(t_index, 3), "t_query_s": round(t_query, 3)}
    if not cfg.keep:
        shutil.rmtree(store, ignore_errors=True)
    else:
        diag["store"] = str(store)
    return rankings, diag


# ── Driver ───────────────────────────────────────────────────────────────────


def build_corpus(entry: dict):
    corpus, ids, ts = [], [], []
    for sid, sess, date in zip(entry["haystack_session_ids"], entry["haystack_sessions"], entry["haystack_dates"]):
        c, i, t = process_item_flat_index(sess, sid, date)
        corpus += c
        ids += i
        ts += t
    return corpus, ids, ts


_CFG: dict = {}


def _init_worker(cfg: dict) -> None:
    _CFG.update(cfg)


def run_one(entry: dict) -> dict:
    cfg = _CFG
    corpus, corpus_ids, corpus_ts = build_corpus(entry)
    correct = list(set(d for d in corpus_ids if "answer" in d))
    query = entry["question"]
    diag: dict = {}
    t0 = time.monotonic()
    if cfg["backend"] == "bm25":
        rankings = rank_bm25_backend(query, corpus, normalised=False)
    elif cfg["backend"] == "bm25-norm":
        rankings = rank_bm25_backend(query, corpus, normalised=True)
    elif cfg["backend"] == "dreamd":
        rankings, diag = dreamd_backend(entry, corpus, corpus_ids, corpus_ts, query, cfg["dreamd"])
    else:
        raise ValueError(cfg["backend"])
    diag["t_total_s"] = round(time.monotonic() - t0, 3)

    metrics = {"turn": {}, "session": {}}
    for k in KS:
        ra, rl, nd = evaluate_retrieval(rankings, correct, corpus_ids, k=k)
        metrics["turn"].update({f"recall_any@{k}": ra, f"recall_all@{k}": rl, f"ndcg_any@{k}": nd})
        ra, rl, nd = evaluate_retrieval_turn2session(rankings, correct, corpus_ids, k=k)
        metrics["session"].update({f"recall_any@{k}": ra, f"recall_all@{k}": rl, f"ndcg_any@{k}": nd})
    return {
        "question_id": entry["question_id"],
        "question_type": entry["question_type"],
        "backend": cfg["backend"],
        "cluster": cfg["dreamd"].cluster if cfg.get("dreamd") else None,
        "timestamps": cfg["dreamd"].timestamps if cfg.get("dreamd") else None,
        "n_correct": len(correct),
        "top10": [corpus_ids[i] for i in rankings[:10]],
        "metrics": metrics,
        "diag": diag,
    }


def summarize(path: Path) -> str:
    rows = [json.loads(l) for l in path.read_text().splitlines() if l.strip()]
    if not rows:
        return "no rows"
    by_type: dict[str, list[dict]] = defaultdict(list)
    for r in rows:
        by_type[r["question_type"]].append(r)
    by_type["ALL"] = rows
    out = [f"# {path.name}: backend={rows[0]['backend']} cluster={rows[0].get('cluster')} "
           f"timestamps={rows[0].get('timestamps')} n={len(rows)}"]
    for gran in ("turn", "session"):
        out.append(f"\n## {gran} granularity")
        hdr = ["type", "n"] + [f"R@{k}" for k in KS] + [f"nDCG@{k}" for k in KS]
        out.append("| " + " | ".join(hdr) + " |")
        out.append("|" + "---|" * len(hdr))
        for t in sorted(by_type, key=lambda x: (x == "ALL", x)):
            rs = by_type[t]
            n = len(rs)
            cells = [t, str(n)]
            for k in KS:
                cells.append(f"{sum(r['metrics'][gran][f'recall_any@{k}'] for r in rs) / n:.3f}")
            for k in KS:
                cells.append(f"{sum(r['metrics'][gran][f'ndcg_any@{k}'] for r in rs) / n:.3f}")
            out.append("| " + " | ".join(cells) + " |")
    diags = [r["diag"] for r in rows if r.get("diag")]
    if diags and "t_index_s" in diags[0]:
        ti = sorted(d["t_index_s"] for d in diags)
        tq = sorted(d["t_query_s"] for d in diags)
        out.append(f"\nindex build p50={ti[len(ti)//2]:.3f}s  query p50={tq[len(tq)//2]:.3f}s  "
                   f"lines/question mean={sum(d['n_lines'] for d in diags)/len(diags):.0f}")
    return "\n".join(out)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--data", type=Path, help="longmemeval_*.json")
    ap.add_argument("--backend", choices=["bm25", "bm25-norm", "dreamd"], default="dreamd")
    ap.add_argument("--cluster", choices=["turn", "session"], default="turn",
                    help="dreamd skill_action scheme (recurrence = 1 per turn, or the session size)")
    ap.add_argument("--timestamps", choices=["haystack", "flat"], default="haystack",
                    help="dreamd timestamps: the haystack session dates, or one constant (recency off)")
    ap.add_argument("--dreamd-bin", type=Path, default=REPO_ROOT / "target/release/dreamd")
    ap.add_argument("--workdir", type=Path, default=None, help="where per-question stores are built")
    ap.add_argument("--keep-stores", action="store_true")
    ap.add_argument("--limit", type=int, default=0)
    ap.add_argument("--offset", type=int, default=0)
    ap.add_argument("--include-abstention", action="store_true",
                    help="keep the *_abs questions (the reference retrieval eval skips them)")
    ap.add_argument("--jobs", type=int, default=max(1, min(8, (os.cpu_count() or 2) // 2)))
    ap.add_argument("--out", type=Path)
    ap.add_argument("--summarize", type=Path, help="print tables for an existing results file and exit")
    args = ap.parse_args()

    if args.summarize:
        print(summarize(args.summarize))
        return 0
    if not args.data or not args.out:
        ap.error("--data and --out are required unless --summarize")

    data = json.loads(args.data.read_text())
    if not args.include_abstention:
        data = [e for e in data if not e["question_id"].endswith("_abs")]
    data = data[args.offset:]
    if args.limit:
        data = data[: args.limit]

    cfg: dict = {"backend": args.backend, "dreamd": None}
    if args.backend == "dreamd":
        if not args.dreamd_bin.is_file():
            print(f"dreamd binary not found: {args.dreamd_bin}", file=sys.stderr)
            return 2
        workdir = args.workdir or Path(tempfile.mkdtemp(prefix="lme-stores-"))
        workdir.mkdir(parents=True, exist_ok=True)
        cfg["dreamd"] = DreamdCfg(binary=args.dreamd_bin.resolve(), cluster=args.cluster,
                                  timestamps=args.timestamps, workdir=workdir, keep=args.keep_stores)
        ver = subprocess.run([str(cfg["dreamd"].binary), "--version"], capture_output=True, text=True).stdout.strip()
        print(f"# {ver}", file=sys.stderr)

    args.out.parent.mkdir(parents=True, exist_ok=True)
    t0 = time.monotonic()
    done = 0
    with args.out.open("w") as fh, mp.Pool(args.jobs, initializer=_init_worker, initargs=(cfg,)) as pool:
        for row in pool.imap_unordered(run_one, data, chunksize=1):
            fh.write(json.dumps(row) + "\n")
            fh.flush()
            done += 1
            if done % 25 == 0 or done == len(data):
                print(f"# {done}/{len(data)} {time.monotonic() - t0:.0f}s", file=sys.stderr)
    print(summarize(args.out))
    return 0


if __name__ == "__main__":
    sys.exit(main())
