#!/usr/bin/env python3
"""Deterministic quality assertions over a dreamd `search_nodes` MCP result.

Reads the raw output of `mcp_driver.py` on stdin (a JSON array of JSON-RPC
tool-call responses), unwraps the `CallToolResult` -> `{"results":[...]}`
payload, and asserts a memory-quality property. Exit 0 = pass, 1 = fail,
2 = bad usage / unparseable input.

These are the ranking + attribution axes that the plumbing suite (alpha-suite.sh)
cannot see, because it uses lexically-unique payloads with only one match.

Subcommands
-----------
  ranking <high_marker> <low_marker>
      The result whose content contains <high_marker> (built with high
      pain/importance) must outrank the <low_marker> result (built with low
      pain/importance but *higher* BM25), proving salience re-ranking — not raw
      lexical match — drives recall order. Gate: high is rank 0, and both
      high.salience > low.salience and high.score > low.score. Also reports
      whether salience overrode a BM25 disadvantage.

  attribution <content_marker> <expected_source_harness> <expected_skill_action>
      The result containing <content_marker> must report the exact
      source_harness and skill_action it was appended with (WEG provenance
      fields survive the append -> index -> recall round-trip through MCP).

Usage:
    mcp_driver.py <bin> <root> < calls.json | quality_check.py ranking HI LO
"""
import json
import sys


def load_results(stream):
    """Unwrap mcp_driver output -> the list under {"results":[...]}.

    Returns (results, error_str). On any structural surprise, results is [] and
    error_str explains what was seen (usually: empty recall / index lag)."""
    try:
        rpc = json.load(stream)
    except json.JSONDecodeError as e:
        return [], f"driver output was not JSON: {e}"
    if not isinstance(rpc, list) or not rpc:
        return [], f"expected a non-empty JSON-RPC array, got: {rpc!r}"
    msg = rpc[-1]  # the last tool call is the search we care about
    if not isinstance(msg, dict) or "result" not in msg:
        return [], f"no `result` in tool response: {msg!r}"
    content = msg["result"].get("content") or []
    if not content or "text" not in content[0]:
        return [], f"no text content in result: {msg['result']!r}"
    try:
        inner = json.loads(content[0]["text"])
    except json.JSONDecodeError as e:
        return [], f"inner recall payload not JSON: {e}"
    return inner.get("results", []), ""


def find(results, marker):
    """First (result, index) whose content contains marker, else (None, -1)."""
    for i, r in enumerate(results):
        if marker in r.get("content", ""):
            return r, i
    return None, -1


def meta(r, key):
    return (r.get("metadata") or {}).get(key)


def cmd_ranking(results, high_marker, low_marker):
    hi, hi_i = find(results, high_marker)
    lo, lo_i = find(results, low_marker)
    if hi is None or lo is None:
        print(f"  ❌ ranking: expected both markers in recall; "
              f"high={'ok' if hi else 'MISSING'} low={'ok' if lo else 'MISSING'} "
              f"({len(results)} results returned)")
        return 1

    ok = True
    if hi_i == 0:
        print(f"  ✅ high-salience record is rank 0 (low is rank {lo_i})")
    else:
        print(f"  ❌ high-salience record is rank {hi_i}, not 0 (low is rank {lo_i})")
        ok = False

    if hi["salience"] > lo["salience"]:
        print(f"  ✅ salience: high {hi['salience']:.4f} > low {lo['salience']:.4f}")
    else:
        print(f"  ❌ salience: high {hi['salience']:.4f} NOT > low {lo['salience']:.4f}")
        ok = False

    if hi["score"] > lo["score"]:
        print(f"  ✅ final score: high {hi['score']:.4f} > low {lo['score']:.4f}")
    else:
        print(f"  ❌ final score: high {hi['score']:.4f} NOT > low {lo['score']:.4f}")
        ok = False

    # Evidence (not a gate): did salience overturn a lexical disadvantage?
    if lo["bm25"] >= hi["bm25"]:
        print(f"  ✅ salience overrode BM25: low had higher/equal BM25 "
              f"({lo['bm25']:.4f} ≥ {hi['bm25']:.4f}) yet ranked below")
    else:
        print(f"  ℹ️  high also had the higher BM25 ({hi['bm25']:.4f} > {lo['bm25']:.4f}); "
              f"salience reinforced rather than overrode lexical order")
    return 0 if ok else 1


def cmd_attribution(results, marker, want_harness, want_skill):
    r, _ = find(results, marker)
    if r is None:
        print(f"  ❌ attribution: no result contained marker "
              f"({len(results)} results returned)")
        return 1
    ok = True
    got_h, got_s = meta(r, "source_harness"), meta(r, "skill_action")
    if got_h == want_harness:
        print(f"  ✅ source_harness == {want_harness!r}")
    else:
        print(f"  ❌ source_harness: got {got_h!r}, want {want_harness!r}")
        ok = False
    if got_s == want_skill:
        print(f"  ✅ skill_action == {want_skill!r}")
    else:
        print(f"  ❌ skill_action: got {got_s!r}, want {want_skill!r}")
        ok = False
    return 0 if ok else 1


def main():
    if len(sys.argv) < 2:
        print(__doc__.strip().splitlines()[0], file=sys.stderr)
        return 2
    mode = sys.argv[1]
    results, err = load_results(sys.stdin)
    if err:
        print(f"  ❌ {mode}: {err}")
        return 1

    if mode == "ranking":
        if len(sys.argv) != 4:
            print("usage: quality_check.py ranking <high_marker> <low_marker>", file=sys.stderr)
            return 2
        return cmd_ranking(results, sys.argv[2], sys.argv[3])
    if mode == "attribution":
        if len(sys.argv) != 5:
            print("usage: quality_check.py attribution <marker> <harness> <skill_action>",
                  file=sys.stderr)
            return 2
        return cmd_attribution(results, sys.argv[2], sys.argv[3], sys.argv[4])

    print(f"unknown mode: {mode}", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main())
