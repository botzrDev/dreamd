#!/usr/bin/env python3
"""Recall-quality gate.

Three falsifiers, each tied to one unbuilt idea. A row prints ``pass`` when
the current ranker does not show that miss, and ``fail`` when it does. Exit
1 when any row fails. Exit 2 when the harness itself breaks.

Run by hand, after ``cargo build -p dreamd --bin dreamd``:

    python3 scripts/benchmark/recall_gate.py
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
BINARY = Path(os.environ.get("DREAMD_BIN", REPO_ROOT / "target" / "debug" / "dreamd"))
ALPHABET = "0123456789ABCDEFGHJKMNPQRSTVWXYZ"


def event_id(n: int) -> str:
    """``evt_`` plus 26 Crockford characters. ``n`` fits in two characters."""
    body = "01ARZ3NDEKTSV4RRFFQ69G5F" + ALPHABET[n // 32] + ALPHABET[n % 32]
    if len(body) != 26:
        raise SystemExit(f"event id body must be 26 characters, got {len(body)}")
    return "evt_" + body


def record(n: int, skill: str, content: str, ts: str, pain: float, importance: float) -> str:
    return json.dumps(
        {
            "schema_version": "1.0.0",
            "id": event_id(n),
            "timestamp": ts,
            "source_harness": "recall_gate",
            "skill_action": skill,
            "content": content,
            "pain": pain,
            "importance": importance,
            "pinned": False,
        },
        separators=(",", ":"),
    )


class Store:
    def __init__(self, home: Path, project: Path) -> None:
        self.home = home
        self.project = project

    def run(self, args: list[str]) -> subprocess.CompletedProcess[str]:
        env = os.environ.copy()
        env["HOME"] = str(self.home)
        env["DREAMD_LOG"] = "off"
        env.pop("DREAMD_SOCK", None)
        env.pop("DREAMD_LOG_FORMAT", None)
        return subprocess.run(
            [str(BINARY), *args],
            cwd=self.project,
            env=env,
            text=True,
            capture_output=True,
            check=False,
        )

    def must(self, args: list[str]) -> subprocess.CompletedProcess[str]:
        out = self.run(args)
        if out.returncode != 0:
            raise SystemExit(
                f"{' '.join(args)} exited {out.returncode}\n"
                f"stdout:\n{out.stdout}\nstderr:\n{out.stderr}"
            )
        return out


def fresh() -> Store:
    home = Path(tempfile.mkdtemp(prefix="recall-gate-home-"))
    project = Path(tempfile.mkdtemp(prefix="recall-gate-proj-"))
    (project / ".git").mkdir()
    store = Store(home, project)
    store.must(["init", "--quiet"])
    return store


def write_log(store: Store, lines: list[str]) -> None:
    path = store.project / ".agent" / "episodic" / "AGENT_LEARNINGS.jsonl"
    path.write_text("".join(line + "\n" for line in lines), encoding="utf-8")
    # doctor --repair exits 0 after a rebuild. Other warnings are not a failure.
    out = store.run(["doctor", "--repair"])
    text = out.stdout + out.stderr
    if "rebuilt index" not in text:
        raise SystemExit(f"doctor --repair did not rebuild the index:\n{text}")


def blame(store: Store, query: str, k: int = 5) -> list[dict]:
    out = store.must(["blame", query, "--json", "-k", str(k)])
    return json.loads(out.stdout)


def top_id(hits: list[dict]) -> str | None:
    return hits[0]["id"] if hits else None


def single_anchor(ts: str) -> str:
    """One bag versus two anchors. ``fail`` means the bag missed and the union hit."""
    store = fresh()
    right = event_id(1)
    wrong = event_id(2)
    write_log(
        store,
        [
            record(
                1,
                "rust::redis::worker",
                "redis worker rejects a job",
                ts,
                6.0,
                6.0,
            ),
            record(
                2,
                "rust::worker::queue",
                "worker queue overflow worker queue overflow worker queue overflow",
                ts,
                6.0,
                6.0,
            ),
        ],
    )
    bag = top_id(blame(store, "redis worker queue overflow"))
    anchors = {
        top_id(blame(store, "redis")),
        top_id(blame(store, "worker queue overflow")),
    }
    if bag == right:
        return "pass"
    if right in anchors and bag == wrong:
        return "fail"
    return "pass"


def hub_drown(ts: str) -> str:
    """A fat parent cluster versus one specific child. ``fail`` means the parent wins."""
    store = fresh()
    needle = event_id(0)
    lines = [
        record(
            0,
            "rust::http::extractor",
            "extractor order for the json body NEEDLE",
            ts,
            7.0,
            6.0,
        )
    ]
    for n in range(1, 41):
        lines.append(
            record(
                n,
                "rust::http",
                "extractor order in the generic handler HUB",
                ts,
                6.0,
                6.0,
            )
        )
    write_log(store, lines)
    top = top_id(blame(store, "extractor order"))
    if top == needle:
        return "pass"
    return "fail"


def token_count(text: str) -> int:
    return len(text.split())


def pack(chunks: list[str], budget: int) -> str:
    total = 0
    kept: list[str] = []
    for chunk in chunks:
        count = token_count(chunk)
        if total + count > budget:
            break
        kept.append(chunk)
        total += count
    return "\n".join(kept)


def consolidation(ts: str) -> str:
    """Recall after a deterministic dream versus stuffing the log.

    Sixteen one-off markers, plus one cluster whose lesson copies the
    exemplar and whose other event carries ``SIBLING``. ``fail`` means
    stuffing kept a marker that recall's packed hits missed.
    """
    store = fresh()
    lines: list[str] = []
    # Oldest first. The sibling fact is not the exemplar.
    lines.append(
        record(70, "rust::eh", "rejection implements IntoResponse SIBLING", ts, 5.0, 5.0)
    )
    lines.append(record(71, "rust::eh", "prefer the question mark EXEMPLAR", ts, 9.0, 8.0))
    lines.append(record(72, "rust::eh", "a third event in the same cluster FILLER", ts, 4.0, 4.0))
    for n in range(16):
        lines.append(
            record(
                80 + n,
                f"rust::marker::m{n:02d}",
                f"unique marker TOKEN{n:02d} and a little surrounding text",
                ts,
                5.0,
                5.0,
            )
        )
    write_log(store, lines)
    dreamed = store.must(["dream", "--no-llm", "--no-commit"])
    if "copy of" not in dreamed.stdout:
        raise SystemExit(
            "the deterministic lesson did not report itself as a copy:\n" + dreamed.stdout
        )

    # Newest line first, matching the coding oracle's stuffing order.
    stuffed = pack(list(reversed(lines)), budget=40)
    missed = False
    for n in range(16):
        marker = f"TOKEN{n:02d}"
        hits = blame(store, marker, k=3)
        packed = pack([hit.get("content", "") for hit in hits], budget=40)
        in_dreamd = marker in packed
        in_stuffing = marker in stuffed
        if in_stuffing and not in_dreamd:
            missed = True
    sibling_hits = blame(store, "SIBLING", k=3)
    sibling_packed = pack([hit.get("content", "") for hit in sibling_hits], budget=40)
    if "SIBLING" in stuffed and "SIBLING" not in sibling_packed:
        missed = True
    return "fail" if missed else "pass"


def main() -> int:
    if not BINARY.is_file():
        print(f"missing {BINARY}; run: cargo build -p dreamd --bin dreamd", file=sys.stderr)
        return 2
    ts = "2026-11-05T15:00:00Z"
    rows = {
        "single_anchor": single_anchor(ts),
        "hub_drown": hub_drown(ts),
        "consolidation": consolidation(ts),
    }
    print(
        "single_anchor={single_anchor} hub_drown={hub_drown} consolidation={consolidation}".format(
            **rows
        )
    )
    return 1 if "fail" in rows.values() else 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except SystemExit:
        raise
    except Exception as exc:  # harness bug, not a falsifier
        print(f"recall_gate: {exc}", file=sys.stderr)
        raise SystemExit(2) from exc
