#!/usr/bin/env python3
"""LLM-judge quality report for dreamd recall (report-only, NON-gating).

The golden gate (quality-suite.sh) proves the salience formula is honored
end-to-end — but only on constructed inputs where the "right" order is
formula-derivable. This complements it: it seeds a small, realistic
engineering-lessons corpus, issues natural-language queries a developer would
actually type (deliberately NOT lexical matches for the stored text), and asks
an LLM to judge whether the top recalled lesson actually answers each query.
It prints a per-query relevance report and a mean score.

This is the fuzzy-relevance axis the deterministic gate can't cover. It is
report-only: it always exits 0 and is never a CI gate.

Auth (first match wins):
  - ANTHROPIC_API_KEY   -> x-api-key
  - ANTHROPIC_AUTH_TOKEN -> Authorization: Bearer + anthropic-beta: oauth-2025-04-20
If neither is set, it prints a skip notice and exits 0 (so it never blocks CI).

Model: DREAMD_JUDGE_MODEL (default claude-opus-4-8).

Usage: scripts/alpha/quality_judge.py   (from repo root; needs target/debug/dreamd)
"""
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
BIN = os.path.join(REPO, "target", "debug", "dreamd")
DRIVER = os.path.join(REPO, "scripts", "alpha", "mcp_driver.py")
API_URL = "https://api.anthropic.com/v1/messages"
MODEL = os.environ.get("DREAMD_JUDGE_MODEL", "claude-opus-4-8")

# Realistic lessons (what one harness stored) paired with a natural-language
# query a developer would type — phrased to AVOID lexical overlap so BM25 alone
# can't trivially match. The judge decides whether recall surfaced the right one.
CASES = [
    {"content": "Always set a statement_timeout on Postgres connections; a runaway analytics query held every pooled connection and took down checkout during peak.",
     "skill": "db::postgres", "query": "our database keeps running out of connections whenever traffic spikes"},
    {"content": "Cache stampede on a hot key: guard the recompute with a mutex and add jittered TTLs so simultaneous expiries don't all hammer the backend at once.",
     "skill": "redis::cache", "query": "how do we stop one popular cached item from overwhelming the backend when it expires"},
    {"content": "A useEffect with a missing dependency array re-ran on every render and triggered an infinite fetch loop in the dashboard.",
     "skill": "react::hooks", "query": "the frontend keeps re-requesting the same data in an endless loop"},
    {"content": "Liveness probe timeout was too aggressive; long GC pauses made the orchestrator kill and restart pods mid-request under load.",
     "skill": "k8s::probes", "query": "pods keep getting restarted under heavy load for no obvious reason"},
    {"content": "JWT validation rejected valid tokens across services because of clock drift; allowing 60s leeway on expiry checks fixed the intermittent failures.",
     "skill": "auth::jwt", "query": "sign-in fails only sometimes when requests cross between microservices"},
    {"content": "Fixed-window rate limiting allowed a 2x burst right at the window boundary; a sliding-window counter smoothed it out.",
     "skill": "api::ratelimit", "query": "clients can briefly exceed their request quota around the reset moment"},
]


def api_key_headers():
    key = os.environ.get("ANTHROPIC_API_KEY")
    if key:
        return {"x-api-key": key}
    tok = os.environ.get("ANTHROPIC_AUTH_TOKEN")
    if tok:
        return {"Authorization": f"Bearer {tok}", "anthropic-beta": "oauth-2025-04-20"}
    # An unset env var doesn't mean no credentials: fall back to an `ant auth
    # login` profile on disk (Claude Code's own auth), minting a short-lived
    # access token. Requires the `ant` CLI and an active profile.
    if shutil.which("ant"):
        try:
            r = subprocess.run(["ant", "auth", "print-credentials", "--access-token"],
                               capture_output=True, text=True, timeout=20)
            oauth = r.stdout.strip()
            if r.returncode == 0 and oauth:
                return {"Authorization": f"Bearer {oauth}", "anthropic-beta": "oauth-2025-04-20"}
        except (subprocess.SubprocessError, OSError):
            pass
    return None


def judge(auth, query, results):
    """Ask the LLM to rate recall relevance 1-5. Returns (score:int, reason:str)."""
    top = results[:3]
    listed = "\n".join(f"  {i+1}. {r.get('content','')}" for i, r in enumerate(top)) or "  (no results returned)"
    prompt = (
        "You are grading a memory system's recall quality. A developer asked a "
        "question; the system returned these past lessons, ranked:\n\n"
        f"QUESTION: {query}\n\nRECALLED LESSONS (rank order):\n{listed}\n\n"
        "Judge whether the #1-ranked lesson actually answers the question. "
        "Score 1-5: 5 = the top lesson directly addresses it; 3 = a relevant "
        "lesson is present but not ranked first; 1 = nothing relevant. "
        'Respond with ONLY a JSON object: '
        '{"score": <int 1-5>, "reason": "<one short sentence>"}'
    )
    body = json.dumps({
        "model": MODEL, "max_tokens": 512,
        "messages": [{"role": "user", "content": prompt}],
    }).encode()
    headers = {"content-type": "application/json", "anthropic-version": "2023-06-01", **auth}
    req = urllib.request.Request(API_URL, data=body, headers=headers, method="POST")
    with urllib.request.urlopen(req, timeout=120) as resp:
        payload = json.load(resp)
    text = "".join(b.get("text", "") for b in payload.get("content", []) if b.get("type") == "text")
    lo, hi = text.find("{"), text.rfind("}")
    obj = json.loads(text[lo:hi + 1]) if lo != -1 and hi != -1 else {}
    return int(obj.get("score", 0)), str(obj.get("reason", text[:80]))


def drive(calls, env):
    out = subprocess.run([sys.executable, DRIVER, BIN, env["_PROJ"]],
                         input=json.dumps(calls), capture_output=True, text=True, env=env)
    try:
        rpc = json.loads(out.stdout)
    except json.JSONDecodeError:
        return []
    if not rpc or "result" not in (rpc[-1] or {}):
        return []
    content = rpc[-1]["result"].get("content") or []
    if not content:
        return []
    try:
        return json.loads(content[0]["text"]).get("results", [])
    except (json.JSONDecodeError, KeyError):
        return []


def main():
    auth = api_key_headers()
    if auth is None:
        print("⏭  quality_judge: skipped — no ANTHROPIC_API_KEY / ANTHROPIC_AUTH_TOKEN set.")
        print("   (set one to run the LLM-judge relevance report; this is never a CI gate.)")
        return 0
    if not os.access(BIN, os.X_OK):
        print(f"FATAL: {BIN} not built (run: cargo build -p dreamd)")
        return 0  # report-only: don't fail callers

    sandbox = tempfile.mkdtemp()
    proj = os.path.join(sandbox, "proj")
    env = {**os.environ, "HOME": sandbox, "_PROJ": proj}
    daemon = None
    try:
        os.makedirs(proj)
        subprocess.run(["git", "init", "-q"], cwd=proj, check=True)
        subprocess.run([BIN, "init"], cwd=proj, env=env, capture_output=True)

        print(f"=== dreamd quality judge (model={MODEL}, sandbox HOME={sandbox}) ===")
        daemon = subprocess.Popen([BIN, "watch"], env=env,
                                  stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        sock = os.path.join(sandbox, ".agent", "dreamd.sock")
        for _ in range(20):
            if os.path.exists(sock):
                break
            time.sleep(0.5)

        # Seed the corpus (one harness writes all lessons).
        for c in CASES:
            drive([{"name": "append_node", "arguments": {
                "content": c["content"], "source_harness": "claude-code",
                "skill_action": c["skill"], "pain": 6.0, "importance": 7.0}}], env)

        # Wait for the index commit (~5s cadence) using a representative query.
        for _ in range(6):
            time.sleep(3)
            if drive([{"name": "search_nodes", "arguments": {"query": "postgres connection pool timeout", "k": 5}}], env):
                break

        scores = []
        for c in CASES:
            results = drive([{"name": "search_nodes", "arguments": {"query": c["query"], "k": 5}}], env)
            try:
                score, reason = judge(auth, c["query"], results)
            except (urllib.error.URLError, urllib.error.HTTPError, ValueError, KeyError) as e:
                print(f"  ⚠️  judge call failed for {c['skill']}: {e}")
                continue
            scores.append(score)
            mark = "✅" if score >= 4 else ("➖" if score >= 3 else "❌")
            print(f"  {mark} [{score}/5] {c['query']}")
            print(f"         → {reason}")

        if scores:
            mean = sum(scores) / len(scores)
            print(f"=== MEAN RELEVANCE: {mean:.2f}/5 over {len(scores)} queries "
                  f"({sum(1 for s in scores if s >= 4)} strong, {sum(1 for s in scores if s < 3)} weak) ===")
            print("   (report only — not a pass/fail gate)")
    finally:
        if daemon is not None:
            daemon.terminate()
            try:
                daemon.wait(timeout=5)
            except subprocess.TimeoutExpired:
                daemon.kill()
        shutil.rmtree(sandbox, ignore_errors=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
