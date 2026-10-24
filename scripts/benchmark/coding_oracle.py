#!/usr/bin/env python3
"""Coding recall oracle.

Whitespace tokens, whole-chunk packing, one frozen fixture. ``--check`` stays
local. A live run writes the JSONL, starts ``dreamd watch``, runs
``dreamd dream --no-llm``, and packs ``k=32`` recall to the same budget.
"""
from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.parse
from pathlib import Path
from typing import Any, Sequence

REPO_ROOT = Path(__file__).resolve().parents[2]
FIXTURE_PATH = Path(__file__).with_name("coding_oracle_fixture.json")
BINARY = REPO_ROOT / "target" / "debug" / "dreamd"
CROCKFORD = "0123456789ABCDEFGHJKMNPQRSTVWXYZ"
SKILL_RE = re.compile(r"^[a-z0-9_]+(?:::[a-z0-9_]+)*$")
EPISODE_IDS = tuple(f"ep{i:02d}" for i in range(1, 13))


def token_count(text: str) -> int:
    """A token is a whitespace-separated word. An empty chunk has 0 tokens."""
    return len(text.split())


def pack(chunks: Sequence[str], budget: int) -> str:
    """Pack whole chunks in order. Stop before the next chunk would exceed."""
    total = 0
    kept: list[str] = []
    for chunk in chunks:
        count = token_count(chunk)
        if total + count > budget:
            break
        kept.append(chunk)
        total += count
    return "\n".join(kept)


def stuffing_chunks(episode: dict[str, Any]) -> list[str]:
    """Newest distractor first (list index 0), needle last."""
    distractors = episode.get("distractors") or []
    needle = episode.get("needle") or {}
    return [str(item.get("content", "")) for item in distractors] + [
        str(needle.get("content", ""))
    ]


def load_fixture(path: Path | None = None) -> dict[str, Any]:
    return json.loads((path or FIXTURE_PATH).read_text(encoding="utf-8"))


def invariant_failures(data: dict[str, Any]) -> list[str]:
    """Step-1 fixture invariants. Each line names an episode id when it has one."""
    failures: list[str] = []
    budget = data.get("budget_tokens")
    budget_ok = type(budget) is int and budget > 0
    if not budget_ok:
        failures.append("fixture: budget_tokens must be a positive integer")
    if data.get("needle_age_sec") != 3456000:
        failures.append("fixture: needle_age_sec must be 3456000")

    episodes = data.get("episodes")
    if not isinstance(episodes, list):
        failures.append("fixture: episodes must be a list")
        return failures
    if len(episodes) != 12:
        failures.append("fixture: expected 12 episodes")

    seen_skills: dict[str, str] = {}
    seen_markers: dict[str, str] = {}
    for index, episode in enumerate(episodes):
        if not isinstance(episode, dict):
            failures.append(f"ep{index + 1:02d}: episode must be an object")
            continue
        episode_id = str(episode.get("id", f"ep{index + 1:02d}"))
        if index < len(EPISODE_IDS) and episode_id != EPISODE_IDS[index]:
            failures.append(f"{episode_id}: id must be {EPISODE_IDS[index]}")

        query = episode.get("query")
        marker = episode.get("marker")
        needle = episode.get("needle")
        distractors = episode.get("distractors")

        if not isinstance(query, str) or ":" in query:
            failures.append(f"{episode_id}: query contains ':'")
            query = query if isinstance(query, str) else ""
        if (
            not isinstance(marker, str)
            or not marker
            or any(ch.isspace() for ch in marker)
        ):
            failures.append(f"{episode_id}: marker must be one whitespace-free token")
            marker = ""
        elif marker in seen_markers:
            failures.append(f"{episode_id}: marker repeats {seen_markers[marker]}")
        else:
            seen_markers[marker] = episode_id

        if not isinstance(needle, dict):
            failures.append(f"{episode_id}: needle must be an object")
            needle = {}
        if needle.get("pain") != 10 or needle.get("importance") != 10:
            failures.append(f"{episode_id}: needle pain and importance must be 10")
        needle_content = needle.get("content")
        if not isinstance(needle_content, str):
            failures.append(f"{episode_id}: needle content missing")
            needle_content = ""

        if not isinstance(distractors, list) or len(distractors) != 4:
            failures.append(f"{episode_id}: expected 4 distractors")
            distractors = distractors if isinstance(distractors, list) else []

        if marker and marker not in needle_content:
            failures.append(f"{episode_id}: marker missing from needle")
        if marker and marker in query:
            failures.append(f"{episode_id}: marker occurs in query")

        distractor_contents: list[str] = []
        for item in distractors:
            if not isinstance(item, dict):
                failures.append(f"{episode_id}: distractor must be an object")
                continue
            if item.get("pain") != 1 or item.get("importance") != 1:
                failures.append(f"{episode_id}: distractor pain and importance must be 1")
            content = item.get("content")
            if not isinstance(content, str):
                failures.append(f"{episode_id}: distractor content missing")
                content = ""
            distractor_contents.append(content)
            if marker and marker in content:
                failures.append(f"{episode_id}: marker occurs in distractor")
            _check_skill(item.get("skill_action"), episode_id, seen_skills, failures)

        _check_skill(needle.get("skill_action"), episode_id, seen_skills, failures)

        for tok in query.split():
            if tok not in needle_content:
                failures.append(f"{episode_id}: query token missing from needle")
                break
            if not any(tok in content for content in distractor_contents):
                failures.append(f"{episode_id}: query token missing from distractors")
                break

        if budget_ok and token_count(needle_content) > budget:
            failures.append(f"{episode_id}: needle exceeds budget")
        if budget_ok and marker:
            stuffed = pack(stuffing_chunks(episode), budget)
            if marker in stuffed:
                failures.append(f"{episode_id}: stuffing pack includes marker")

    return failures


def _check_skill(
    skill: Any,
    episode_id: str,
    seen: dict[str, str],
    failures: list[str],
) -> None:
    if (
        not isinstance(skill, str)
        or SKILL_RE.fullmatch(skill) is None
        or len(skill.encode("utf-8")) > 256
    ):
        failures.append(f"{episode_id}: skill_action invalid")
        return
    if skill in seen:
        failures.append(f"{episode_id}: skill_action repeats {seen[skill]}")
        return
    seen[skill] = episode_id


def _event_id(n: int) -> str:
    chars = ["0"] * 26
    value = n
    index = 25
    while value and index >= 0:
        chars[index] = CROCKFORD[value % 32]
        value //= 32
        index -= 1
    if value:
        raise RuntimeError("event id space exhausted")
    return "evt_" + "".join(chars)


def _iso_z(unix_sec: int) -> str:
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(unix_sec))


def _jsonl(data: dict[str, Any], now_sec: int) -> str:
    needle_ts = _iso_z(now_sec - int(data["needle_age_sec"]))
    distractor_ts = _iso_z(now_sec)
    lines: list[str] = []
    serial = 1
    for episode in data["episodes"]:
        ordered = [episode["needle"], *reversed(episode["distractors"])]
        for offset, record in enumerate(ordered):
            timestamp = needle_ts if offset == 0 else distractor_ts
            lines.append(
                json.dumps(
                    {
                        "schema_version": "1.0.0",
                        "id": _event_id(serial),
                        "timestamp": timestamp,
                        "pain": record["pain"],
                        "importance": record["importance"],
                        "pinned": False,
                        "skill_action": record["skill_action"],
                        "source_harness": "coding_oracle",
                        "content": record["content"],
                    },
                    ensure_ascii=False,
                    separators=(",", ":"),
                )
            )
            serial += 1
    return "\n".join(lines) + "\n"


def _http_unix(
    sock_path: Path,
    method: str,
    path: str,
    headers: dict[str, str],
) -> tuple[int, str]:
    hdrs = {"Host": "localhost", "Accept": "*/*", **headers}
    payload = (
        f"{method} {path} HTTP/1.1\r\n"
        + "".join(f"{k}: {v}\r\n" for k, v in hdrs.items())
        + "Connection: close\r\n\r\n"
    ).encode()
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
        sock.settimeout(2)
        sock.connect(str(sock_path))
        sock.sendall(payload)
        chunks: list[bytes] = []
        while True:
            try:
                part = sock.recv(65536)
            except socket.timeout:
                break
            if not part:
                break
            chunks.append(part)
    raw = b"".join(chunks).decode("utf-8", errors="replace")
    status_line = raw.split("\r\n", 1)[0]
    code = int(status_line.split()[1])
    _, _, body = raw.partition("\r\n\r\n")
    return code, body


def _child_env(home: Path) -> dict[str, str]:
    env = os.environ.copy()
    env["HOME"] = str(home)
    env.pop("DREAMD_SOCK", None)
    return env


def _stop_daemon(proc: subprocess.Popen[bytes] | None) -> None:
    if proc is None or proc.poll() is not None:
        return
    proc.terminate()
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=5)


def _result_contents(body: str) -> list[str]:
    start = body.find("{")
    end = body.rfind("}")
    if start < 0 or end < start:
        raise ValueError("recall body has no json object")
    payload = json.loads(body[start : end + 1])
    return [str(hit["content"]) for hit in payload["results"]]


def _outcome(marker: str, dreamd_text: str, stuffing_text: str) -> str:
    in_dreamd = marker in dreamd_text
    in_stuffing = marker in stuffing_text
    if in_dreamd and in_stuffing:
        return "both"
    if in_dreamd:
        return "dreamd_only"
    if in_stuffing:
        return "stuffing_only"
    return "neither"


def live(data: dict[str, Any]) -> int:
    if not BINARY.is_file():
        print("missing binary: target/debug/dreamd", file=sys.stderr)
        return 2

    now_sec = int(time.time())
    sandbox = Path(tempfile.mkdtemp(prefix="dreamd-oracle-"))
    proj = sandbox / "proj"
    daemon: subprocess.Popen[bytes] | None = None
    env = _child_env(sandbox)
    try:
        proj.mkdir()
        git = subprocess.run(
            ["git", "init"],
            cwd=proj,
            env=env,
            check=False,
            capture_output=True,
        )
        if git.returncode != 0:
            print("abort: git init failed", file=sys.stderr)
            return 1
        init = subprocess.run(
            [str(BINARY), "init"],
            cwd=proj,
            env=env,
            check=False,
            capture_output=True,
        )
        if init.returncode != 0:
            print("abort: dreamd init failed", file=sys.stderr)
            return 1

        jsonl_path = proj / ".agent" / "episodic" / "AGENT_LEARNINGS.jsonl"
        jsonl_path.write_text(_jsonl(data, now_sec), encoding="utf-8")

        daemon = subprocess.Popen(
            [str(BINARY), "watch"],
            cwd=proj,
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        sock = sandbox / ".agent" / "dreamd.sock"
        headers = {"X-Agent-Root": str(proj)}
        first = data["episodes"][0]
        probe = str(first["query"].split()[0])
        marker = str(first["marker"])
        probe_path = "/api/v1/recall?q=" + urllib.parse.quote(probe) + "&k=1"
        deadline = time.monotonic() + 30
        indexed = False
        while time.monotonic() < deadline:
            if sock.exists():
                try:
                    code, body = _http_unix(sock, "GET", probe_path, headers)
                except (OSError, ValueError, IndexError):
                    code, body = 0, ""
                if code == 200 and marker in body:
                    indexed = True
                    break
            time.sleep(0.25)
        if not indexed:
            print(f"abort: {first['id']} marker not recallable within 30s", file=sys.stderr)
            return 1

        dream = subprocess.run(
            [str(BINARY), "dream", "--no-llm"],
            cwd=proj,
            env=env,
            check=False,
            capture_output=True,
            timeout=180,
        )
        if dream.returncode != 0:
            print(f"abort: dream exited {dream.returncode}", file=sys.stderr)
            sys.stderr.buffer.write(dream.stderr)
            return 1

        budget = int(data["budget_tokens"])
        rows: list[tuple[str, str]] = []
        counts = {"dreamd_only": 0, "both": 0, "stuffing_only": 0, "neither": 0}
        for episode in data["episodes"]:
            query = urllib.parse.quote(str(episode["query"]))
            path = f"/api/v1/recall?q={query}&k=32"
            try:
                code, body = _http_unix(sock, "GET", path, headers)
            except OSError as exc:
                print(f"abort: {episode['id']} recall failed: {exc}", file=sys.stderr)
                return 1
            if code != 200:
                print(f"abort: {episode['id']} recall HTTP {code}", file=sys.stderr)
                return 1
            try:
                contents = _result_contents(body)
            except (json.JSONDecodeError, KeyError, TypeError, ValueError) as exc:
                print(f"abort: {episode['id']} recall body: {exc}", file=sys.stderr)
                return 1
            outcome = _outcome(
                str(episode["marker"]),
                pack(contents, budget),
                pack(stuffing_chunks(episode), budget),
            )
            counts[outcome] += 1
            rows.append((str(episode["id"]), outcome))

        print(
            f"budget_tokens={budget} now_sec={now_sec} needle_age_sec=3456000"
        )
        for episode_id, outcome in rows:
            print(f"ep={episode_id} outcome={outcome}")
        print(
            "dreamd_only={dreamd_only} both={both} stuffing_only={stuffing_only} "
            "neither={neither} n=12".format(**counts)
        )
        return 0
    finally:
        _stop_daemon(daemon)
        if sandbox.exists():
            shutil.rmtree(sandbox, ignore_errors=True)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Coding recall oracle")
    parser.add_argument(
        "--check",
        action="store_true",
        help="Check the frozen fixture locally and print nothing on success",
    )
    args = parser.parse_args(argv)
    try:
        data = load_fixture()
    except (OSError, json.JSONDecodeError) as exc:
        print(f"fixture: {exc}", file=sys.stderr)
        return 2
    if args.check:
        failures = invariant_failures(data)
        if failures:
            for line in failures:
                print(line, file=sys.stderr)
            return 2
        return 0
    return live(data)


if __name__ == "__main__":
    sys.exit(main())
