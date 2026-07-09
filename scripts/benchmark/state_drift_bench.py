#!/usr/bin/env python3
"""State-Drift benchmark — Week-0 bake-off gate (ANTH-20).

Neutral referee: every memory system is one row. Programmatic oracle with an
explicit AMBIGUOUS bucket. Determinism is checked, not assumed.

Reproduce:
  python3 scripts/benchmark/state_drift_bench.py --demo
  python3 scripts/benchmark/state_drift_bench.py --verify-determinism
  python3 scripts/benchmark/state_drift_bench.py --bakeoff
"""
from __future__ import annotations

import argparse
import json
import math
import os
import random
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid
from abc import ABC, abstractmethod
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Iterable, Sequence

REPO_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_TRIALS = 20
BOOTSTRAP_SAMPLES = 2000
CI_LEVEL = 0.95


# ---------------------------------------------------------------------------
# Scenario + oracle
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class Turn:
    content: str
    role: str = "fact"  # fact | distractor


@dataclass(frozen=True)
class Scenario:
    id: str
    turns: tuple[Turn, ...]
    query: str
    current_markers: tuple[str, ...]
    stale_markers: tuple[str, ...]
    contam_markers: tuple[str, ...] = ()


@dataclass
class TrialOutcome:
    fidelity: str  # pass | fail | ambiguous
    conflict: str  # pass | fail | ambiguous
    stale: str  # pass | fail | ambiguous
    contam: str  # pass | fail | ambiguous


def bakeoff_suite() -> list[Scenario]:
    """Three drift scenarios for the Week-0 gate."""
    return [
        Scenario(
            id="london_tokyo",
            turns=(
                Turn("User profile: home city is London, United Kingdom."),
                Turn("User profile updated: relocated to Tokyo, Japan."),
            ),
            query="What is the user's current home city?",
            current_markers=("tokyo",),
            stale_markers=("london",),
        ),
        Scenario(
            id="db_preference",
            turns=(
                Turn("Project database preference is PostgreSQL."),
                Turn("Project database preference was changed to SQLite."),
            ),
            query="Which database does the project use?",
            current_markers=("sqlite",),
            stale_markers=("postgresql", "postgres"),
        ),
        Scenario(
            id="timeout_distractor",
            turns=(
                Turn("API request timeout is 30 seconds."),
                Turn("Staging environment listens on port 8080.", role="distractor"),
                Turn("API request timeout was updated to 60 seconds."),
            ),
            query="What is the API request timeout setting?",
            current_markers=("60", "sixty"),
            stale_markers=("30", "thirty"),
            contam_markers=("8080", "port 8080"),
        ),
    ]


def recall_query(scenario: Scenario) -> str:
    """BM25-safe query — strip punctuation that breaks Tantivy parsing."""
    return re.sub(r"[^a-zA-Z0-9]+", " ", scenario.query).strip()


def adjudicate(response: str, scenario: Scenario) -> TrialOutcome:
    """Programmatic oracle — no LLM adjudication."""
    text = response.lower().strip()
    if len(text) < 3:
        amb = "ambiguous"
        return TrialOutcome(amb, amb, amb, amb)

    has_current = any(m in text for m in scenario.current_markers)
    has_stale = any(m in text for m in scenario.stale_markers)
    has_contam = any(m in text for m in scenario.contam_markers)

    if not has_current and not has_stale:
        amb = "ambiguous"
        return TrialOutcome(amb, amb, amb, amb)

    if has_current and has_stale:
        return TrialOutcome("fail", "fail", "pass", "pass")

    if has_current and not has_stale:
        contam = "fail" if has_contam and not has_current else ("fail" if has_contam else "pass")
        # Distractor tokens alongside the correct answer still count as contamination.
        if has_contam:
            contam = "fail"
        return TrialOutcome("pass", "pass", "pass", contam)

    # stale only
    return TrialOutcome("fail", "pass", "fail", "pass")


def _rate(values: Sequence[str], label: str) -> float:
    adjudicated = [v for v in values if v != "ambiguous"]
    if not adjudicated:
        return float("nan")
    return sum(1 for v in adjudicated if v == label) / len(adjudicated)


# ---------------------------------------------------------------------------
# Metrics + bootstrap CI
# ---------------------------------------------------------------------------


@dataclass
class AxisRates:
    fidelity: float
    conflict: float
    stale: float
    contam: float
    ambiguous: float
    n: int


@dataclass
class AxisCI:
    low: float
    high: float


@dataclass
class SystemReport:
    name: str
    rates: AxisRates
    cis: dict[str, AxisCI]
    deterministic: bool | None
    wired: bool
    note: str = ""


def _collect_axis(samples: list[float]) -> AxisCI:
    if not samples:
        return AxisCI(float("nan"), float("nan"))
    lo = (1 - CI_LEVEL) / 2
    xs = sorted(samples)
    return AxisCI(xs[int(lo * len(xs))], xs[int((1 - lo) * len(xs)) - 1])


def bootstrap_cis(trials: list[TrialOutcome], *, samples: int = BOOTSTRAP_SAMPLES) -> dict[str, AxisCI]:
    rng = random.Random(0)
    n = len(trials)
    if n == 0:
        nan = AxisCI(float("nan"), float("nan"))
        return {k: nan for k in ("fidelity", "conflict", "stale", "contam", "ambiguous")}

    buckets: dict[str, list[float]] = {k: [] for k in ("fidelity", "conflict", "stale", "contam", "ambiguous")}
    for _ in range(samples):
        draw = [trials[rng.randrange(n)] for _ in range(n)]
        buckets["fidelity"].append(_rate([t.fidelity for t in draw], "pass"))
        buckets["conflict"].append(_rate([t.conflict for t in draw], "pass"))
        buckets["stale"].append(_rate([t.stale for t in draw], "pass"))
        buckets["contam"].append(_rate([t.contam for t in draw], "pass"))
        buckets["ambiguous"].append(_rate([t.fidelity for t in draw], "ambiguous"))
    return {k: _collect_axis(v) for k, v in buckets.items()}


def aggregate_rates(trials: list[TrialOutcome]) -> AxisRates:
    return AxisRates(
        fidelity=_rate([t.fidelity for t in trials], "pass"),
        conflict=_rate([t.conflict for t in trials], "pass"),
        stale=_rate([t.stale for t in trials], "fail"),
        contam=_rate([t.contam for t in trials], "fail"),
        ambiguous=_rate([t.fidelity for t in trials], "ambiguous"),
        n=len(trials),
    )


# ---------------------------------------------------------------------------
# Adapter interface
# ---------------------------------------------------------------------------


class MemoryAdapter(ABC):
    name: str
    wired: bool = True

    @abstractmethod
    def reset(self, run_id: str) -> None:
        ...

    @abstractmethod
    def ingest(self, scenario: Scenario) -> None:
        ...

    @abstractmethod
    def recall(self, scenario: Scenario, *, trial: int) -> str:
        ...

    def check_determinism(self, scenario: Scenario) -> bool:
        self.reset(f"det-{scenario.id}")
        self.ingest(scenario)
        a = self.recall(scenario, trial=0)
        b = self.recall(scenario, trial=0)
        return a == b


# ---------------------------------------------------------------------------
# Reference / floor systems (self-proving)
# ---------------------------------------------------------------------------


class ReferenceFaithfulAdapter(MemoryAdapter):
    """Last-write-wins transactional store — models the dreamd hypothesis."""

    name = "reference-faithful"

    def __init__(self) -> None:
        self._facts: list[str] = []

    def reset(self, run_id: str) -> None:
        self._facts = []

    def ingest(self, scenario: Scenario) -> None:
        for turn in scenario.turns:
            if turn.role == "fact":
                self._facts.append(turn.content)

    def recall(self, scenario: Scenario, *, trial: int) -> str:
        if not self._facts:
            return ""
        # Last fact wins — transactional state fidelity.
        return self._facts[-1]


class FloorSlidingWindowAdapter(MemoryAdapter):
    name = "floor-sliding-window(k=3)"

    def __init__(self, k: int = 3) -> None:
        self.k = k
        self._window: list[str] = []

    def reset(self, run_id: str) -> None:
        self._window = []

    def ingest(self, scenario: Scenario) -> None:
        for turn in scenario.turns:
            self._window.append(turn.content)
            if len(self._window) > self.k:
                self._window.pop(0)

    def recall(self, scenario: Scenario, *, trial: int) -> str:
        # Short-horizon bake-off: latest turn in the window is the answer.
        return self._window[-1] if self._window else ""


class ReferenceLossyAdapter(MemoryAdapter):
    """Stochastic overwrite failures — leaks staleness and blend."""

    name = "reference-lossy"

    def __init__(self, seed: int = 7) -> None:
        self.seed = seed
        self._facts: list[str] = []
        self._run_id = ""

    def reset(self, run_id: str) -> None:
        self._facts = []
        self._run_id = run_id

    def ingest(self, scenario: Scenario) -> None:
        for i, turn in enumerate(scenario.turns):
            if turn.role != "fact":
                continue
            rng = random.Random(hash((self.seed, self._run_id, scenario.id, i)))
            if self._facts and rng.random() < 0.30:
                continue  # failed update — stale persists
            if self._facts:
                self._facts[-1] = turn.content
            else:
                self._facts.append(turn.content)

    def recall(self, scenario: Scenario, *, trial: int) -> str:
        rng = random.Random(hash((self.seed, self._run_id, scenario.id, trial, "recall")))
        parts = list(self._facts)
        # Blend failure: sometimes append an older synthetic stale marker.
        if len(parts) >= 2 and rng.random() < 0.48:
            return f"{parts[-2]}\n{parts[-1]}"
        if parts:
            return parts[-1]
        return ""


class FloorVectorRagAdapter(MemoryAdapter):
    """Naive keyword overlap — no invalidation."""

    name = "floor-vector-rag(naive)"

    def __init__(self) -> None:
        self._chunks: list[str] = []

    def reset(self, run_id: str) -> None:
        self._chunks = []

    def ingest(self, scenario: Scenario) -> None:
        self._chunks.extend(t.content for t in scenario.turns)

    def recall(self, scenario: Scenario, *, trial: int) -> str:
        query_tokens = set(re.findall(r"[a-z0-9]+", scenario.query.lower()))
        scored: list[tuple[int, int, str]] = []
        for idx, chunk in enumerate(self._chunks):
            tokens = set(re.findall(r"[a-z0-9]+", chunk.lower()))
            scored.append((len(query_tokens & tokens), idx, chunk))
        scored.sort(key=lambda x: (-x[0], x[1]))
        if not scored or scored[0][0] == 0:
            return self._chunks[-1] if self._chunks else ""
        best = scored[0][0]
        top = [c for s, _, c in scored if s == best]
        rng = random.Random(hash((scenario.id, trial, "vgrag")) & 0xFFFFFFFF)
        roll = rng.random()
        if roll < 0.22:
            return top[0]
        if roll < 0.38 and len(top) > 1:
            return "\n".join(top[:2])
        return top[-1]


# ---------------------------------------------------------------------------
# dreamd adapter (HTTP over Unix socket)
# ---------------------------------------------------------------------------


def _http_unix(
    sock_path: Path,
    method: str,
    path: str,
    headers: dict[str, str],
    body: bytes | None = None,
) -> tuple[int, str]:
    hdrs = dict(headers)
    if body is not None:
        hdrs.setdefault("Content-Length", str(len(body)))
    payload = (
        f"{method} {path} HTTP/1.1\r\n"
        + "".join(f"{k}: {v}\r\n" for k, v in hdrs.items())
        + "Connection: close\r\n\r\n"
    ).encode()
    if body:
        payload += body

    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
        sock.connect(str(sock_path))
        sock.sendall(payload)
        chunks: list[bytes] = []
        while True:
            part = sock.recv(65536)
            if not part:
                break
            chunks.append(part)
    raw = b"".join(chunks).decode("utf-8", errors="replace")
    _, _, rest = raw.partition("\r\n\r\n")
    status_line = raw.split("\r\n", 1)[0]
    code = int(status_line.split()[1])
    return code, rest


class DreamdAdapter(MemoryAdapter):
    name = "dreamd"
    wired = True

    def __init__(self) -> None:
        self._sandbox: Path | None = None
        self._proj: Path | None = None
        self._daemon: subprocess.Popen[Any] | None = None
        self._bin = REPO_ROOT / "target/debug/dreamd"
        if not self._bin.is_file():
            self.wired = False

    def reset(self, run_id: str) -> None:
        self._stop()
        self._sandbox = Path(tempfile.mkdtemp(prefix="dreamd-bench-"))
        self._proj = self._sandbox / "proj"
        self._proj.mkdir()
        env = os.environ.copy()
        env["HOME"] = str(self._sandbox)
        subprocess.run(["git", "init"], cwd=self._proj, check=True, capture_output=True)
        subprocess.run([str(self._bin), "init"], cwd=self._proj, check=True, capture_output=True, env=env)
        self._daemon = subprocess.Popen(
            [str(self._bin), "watch"],
            cwd=self._proj,
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        sock = self._sandbox / ".agent" / "dreamd.sock"
        for _ in range(40):
            if sock.exists():
                break
            time.sleep(0.25)
        if not sock.exists():
            raise RuntimeError("dreamd daemon failed to bind socket")

    def _stop(self) -> None:
        if self._daemon:
            self._daemon.terminate()
            try:
                self._daemon.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self._daemon.kill()
            self._daemon = None
        if self._sandbox and self._sandbox.exists():
            shutil.rmtree(self._sandbox, ignore_errors=True)
        self._sandbox = None
        self._proj = None

    def ingest(self, scenario: Scenario) -> None:
        assert self._sandbox and self._proj
        sock = self._sandbox / ".agent" / "dreamd.sock"
        env = os.environ.copy()
        env["HOME"] = str(self._sandbox)
        for i, turn in enumerate(scenario.turns):
            body = json.dumps(
                {
                    "schema_version": "1.0.0",
                    "id": "evt_01ARZ3NDEKTSV4RRFFQ69G5FAV",
                    "timestamp": "2026-07-09T00:00:00Z",
                    "pain": 7.0,
                    "importance": 8.0,
                    "skill_action": f"bench::{scenario.id}::turn{i}",
                    "source_harness": "state_drift_bench",
                    "content": turn.content,
                }
            ).encode()
            headers = {
                "Host": "localhost",
                "X-Agent-Root": str(self._proj),
                "Content-Type": "application/json",
            }
            code, _ = _http_unix(sock, "POST", "/api/v1/learn", headers, body)
            if code not in (200, 201):
                raise RuntimeError(f"dreamd learn failed: HTTP {code}")
        self._wait_index(sock, headers, scenario)

    def _wait_index(self, sock: Path, headers: dict[str, str], scenario: Scenario) -> None:
        """Poll recall until indexed or ~12s (index commit cadence ~5s)."""
        tokens = recall_query(scenario).split()
        probe = tokens[0] if tokens else "user"
        path = f"/api/v1/recall?q={urllib.parse.quote(probe)}&k=1"
        for _ in range(24):
            code, body = _http_unix(sock, "GET", path, headers)
            if code == 200 and '"content"' in body:
                return
            time.sleep(0.5)

    def recall(self, scenario: Scenario, *, trial: int) -> str:
        assert self._sandbox and self._proj
        sock = self._sandbox / ".agent" / "dreamd.sock"
        q = urllib.parse.quote(recall_query(scenario))
        headers = {"Host": "localhost", "X-Agent-Root": str(self._proj)}
        code, body = _http_unix(sock, "GET", f"/api/v1/recall?q={q}&k=5", headers)
        if code != 200:
            return ""
        data = json.loads(body)
        return "\n".join(r.get("content", "") for r in data.get("results", []))

    def __del__(self) -> None:
        self._stop()


# ---------------------------------------------------------------------------
# Mem0 adapter (REST, v3)
# ---------------------------------------------------------------------------


class Mem0Adapter(MemoryAdapter):
    name = "mem0"

    def __init__(self) -> None:
        self.api_key = os.environ.get("MEM0_API_KEY", "")
        self.wired = bool(self.api_key)
        self._user_id = ""

    def reset(self, run_id: str) -> None:
        self._user_id = f"bench-{run_id}"

    def _request(self, path: str, payload: dict[str, Any]) -> dict[str, Any]:
        req = urllib.request.Request(
            f"https://api.mem0.ai{path}",
            data=json.dumps(payload).encode(),
            headers={
                "Authorization": f"Token {self.api_key}",
                "Content-Type": "application/json",
            },
            method="POST",
        )
        with urllib.request.urlopen(req, timeout=60) as resp:
            return json.loads(resp.read().decode())

    def ingest(self, scenario: Scenario) -> None:
        for turn in scenario.turns:
            self._request(
                "/v3/memories/add/",
                {
                    "messages": [
                        {"role": "user", "content": turn.content},
                        {
                            "role": "assistant",
                            "content": "Recorded for benchmark state-drift probe.",
                        },
                    ],
                    "user_id": self._user_id,
                },
            )
        time.sleep(2)  # extraction latency

    def recall(self, scenario: Scenario, *, trial: int) -> str:
        data = self._request(
            "/v3/memories/search/",
            {"query": scenario.query, "filters": {"user_id": self._user_id}},
        )
        results = data.get("results") or data.get("memories") or []
        parts: list[str] = []
        for item in results:
            if isinstance(item, dict):
                parts.append(item.get("memory") or item.get("text") or json.dumps(item))
            else:
                parts.append(str(item))
        return "\n".join(parts)


# ---------------------------------------------------------------------------
# Zep adapter (REST, graph search)
# ---------------------------------------------------------------------------


class ZepAdapter(MemoryAdapter):
    name = "zep"

    def __init__(self) -> None:
        self.api_key = os.environ.get("ZEP_API_KEY", "")
        self.wired = bool(self.api_key)
        self._user_id = ""

    def reset(self, run_id: str) -> None:
        self._user_id = f"bench-{run_id}"
        if not self.wired:
            return
        self._request(
            "POST",
            "/api/v2/users",
            {"user_id": self._user_id, "email": f"{self._user_id}@bench.local", "first_name": "Bench"},
            ignore_exists=True,
        )

    def _request(
        self,
        method: str,
        path: str,
        payload: dict[str, Any] | None = None,
        *,
        ignore_exists: bool = False,
    ) -> dict[str, Any]:
        req = urllib.request.Request(
            f"https://api.getzep.com{path}",
            data=json.dumps(payload).encode() if payload is not None else None,
            headers={
                "Authorization": f"Api-Key {self.api_key}",
                "Content-Type": "application/json",
            },
            method=method,
        )
        try:
            with urllib.request.urlopen(req, timeout=90) as resp:
                raw = resp.read().decode()
                return json.loads(raw) if raw else {}
        except urllib.error.HTTPError as exc:
            if ignore_exists and exc.code in (400, 409):
                return {}
            raise

    def ingest(self, scenario: Scenario) -> None:
        for turn in scenario.turns:
            self._request(
                "POST",
                "/api/v2/graph",
                {
                    "user_id": self._user_id,
                    "type": "text",
                    "data": turn.content,
                },
            )
        time.sleep(3)  # graph ingestion

    def recall(self, scenario: Scenario, *, trial: int) -> str:
        data = self._request(
            "POST",
            "/api/v2/graph/search",
            {
                "user_id": self._user_id,
                "query": scenario.query,
                "scope": "edges",
                "limit": 5,
            },
        )
        parts: list[str] = []
        for edge in data.get("edges", []) or []:
            if isinstance(edge, dict):
                parts.append(edge.get("fact") or edge.get("name") or json.dumps(edge))
        return "\n".join(parts)


# ---------------------------------------------------------------------------
# Runner
# ---------------------------------------------------------------------------


def reference_adapters() -> list[MemoryAdapter]:
    return [
        ReferenceFaithfulAdapter(),
        FloorSlidingWindowAdapter(k=3),
        ReferenceLossyAdapter(),
        FloorVectorRagAdapter(),
    ]


def bakeoff_adapters() -> list[MemoryAdapter]:
    return [DreamdAdapter(), Mem0Adapter(), ZepAdapter()]


def run_system(
    adapter: MemoryAdapter,
    scenarios: Sequence[Scenario],
    *,
    trials: int,
) -> tuple[list[TrialOutcome], bool | None]:
    outcomes: list[TrialOutcome] = []
    det: bool | None = None
    for trial in range(trials):
        for scenario in scenarios:
            run_id = f"{scenario.id}-t{trial}-{uuid.uuid4().hex[:8]}"
            adapter.reset(run_id)
            adapter.ingest(scenario)
            response = adapter.recall(scenario, trial=trial)
            outcomes.append(adjudicate(response, scenario))
            if det is None and trial == 0:
                det = adapter.check_determinism(scenario)
    return outcomes, det


def evaluate(adapter: MemoryAdapter, scenarios: Sequence[Scenario], *, trials: int) -> SystemReport:
    if not adapter.wired:
        nan = AxisRates(float("nan"), float("nan"), float("nan"), float("nan"), float("nan"), 0)
        return SystemReport(
            adapter.name,
            nan,
            {},
            None,
            wired=False,
            note="adapter not wired (missing binary or API key)",
        )
    outcomes, det = run_system(adapter, scenarios, trials=trials)
    rates = aggregate_rates(outcomes)
    cis = bootstrap_cis(outcomes)
    return SystemReport(adapter.name, rates, cis, det, wired=True)


def fmt_pct(x: float) -> str:
    if math.isnan(x):
        return "n/a"
    return f"{x * 100:.0f}%"


def print_table(reports: Iterable[SystemReport]) -> None:
    print(f"{'system':<28} {'fidelity':>9} {'conflict':>9} {'stale':>6} {'contam':>7} {'det':>4}")
    for r in reports:
        det = "ok" if r.deterministic else ("no" if r.deterministic is False else "n/a")
        if not r.wired:
            print(f"{r.name:<28} {'— unwired —':>42}")
            continue
        print(
            f"{r.name:<28} {fmt_pct(r.rates.fidelity):>9} {fmt_pct(r.rates.conflict):>9} "
            f"{fmt_pct(r.rates.stale):>6} {fmt_pct(r.rates.contam):>7} {det:>4}"
        )


def ci_clear(a: AxisCI, b: AxisCI) -> bool:
    """True if a's CI is strictly above b's (non-overlapping, a better)."""
    if any(math.isnan(x) for x in (a.low, a.high, b.low, b.high)):
        return False
    return a.low > b.high


def gate_verdict(reports: list[SystemReport]) -> str:
    by_name = {r.name: r for r in reports}
    dreamd = by_name.get("dreamd")
    if not dreamd or not dreamd.wired:
        return "REFUSE: dreamd adapter not wired"
    competitors = [r for r in reports if r.name != "dreamd" and r.wired]
    if len(competitors) < 2:
        unwired = [r.name for r in reports if r.name != "dreamd" and not r.wired]
        return f"REFUSE: bake-off competitors not wired ({', '.join(unwired)})"

    axes = ("fidelity", "conflict", "stale", "contam")
    # Lower stale/contam is better; higher fidelity/conflict is better.
    higher_better = {"fidelity": True, "conflict": True, "stale": False, "contam": False}

    cleared: list[str] = []
    for axis in axes:
        d_ci = dreamd.cis.get(axis)
        if not d_ci:
            continue
        if all(
            ci_clear(d_ci, c.cis[axis]) if higher_better[axis] else ci_clear(c.cis[axis], d_ci)
            for c in competitors
            if axis in c.cis
        ):
            cleared.append(axis)

    if cleared:
        return f"DELTA — dreamd clears field on: {', '.join(cleared)}"
    return "PIVOT — CIs overlap on all axes; ship neutral cross-field map"


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="State-Drift benchmark bake-off gate")
    parser.add_argument("--demo", action="store_true", help="Reference/floor systems only")
    parser.add_argument("--verify-determinism", action="store_true", help="Determinism replay check")
    parser.add_argument("--bakeoff", action="store_true", help="Run dreamd vs Mem0 vs Zep gate")
    parser.add_argument("--trials", type=int, default=DEFAULT_TRIALS, help="Trials per scenario")
    args = parser.parse_args(argv)

    scenarios = bakeoff_suite()

    if args.verify_determinism:
        ok = True
        for adapter in reference_adapters() + [DreamdAdapter()]:
            if not adapter.wired:
                print(f"SKIP {adapter.name}: not wired")
                continue
            for scenario in scenarios:
                adapter.reset(f"det-{scenario.id}")
                adapter.ingest(scenario)
                if adapter.check_determinism(scenario):
                    print(f"OK  {adapter.name} / {scenario.id}")
                else:
                    print(f"FAIL {adapter.name} / {scenario.id}")
                    ok = False
        return 0 if ok else 1

    if args.demo:
        reports = [evaluate(a, scenarios, trials=args.trials) for a in reference_adapters()]
        print_table(reports)
        return 0

    adapters = bakeoff_adapters()
    if not args.bakeoff:
        unwired = [a.name for a in adapters if not a.wired]
        if unwired:
            print(f"REFUSE: bake-off adapters not wired ({', '.join(unwired)})")
            return 0

    reports = [evaluate(a, scenarios, trials=args.trials) for a in adapters]
    print_table(reports)
    print()
    print(gate_verdict(reports))
    return 0


if __name__ == "__main__":
    sys.exit(main())
