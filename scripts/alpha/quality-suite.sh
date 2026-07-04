#!/usr/bin/env bash
# dreamd quality suite — deterministic golden gate for MEMORY QUALITY.
#
# The plumbing suite (alpha-suite.sh) proves a write is *retrievable*; it uses
# lexically-unique payloads so recall is unambiguous — which means it cannot see
# a RANKING regression (only ever one match). This suite proves the memory is
# actually *good* through the exact MCP surface a real harness uses:
#
#   Axis 1 — Salience ranking (the wedge): a painful/important lesson outranks a
#            benign one that has HIGHER raw BM25, proving salience re-ranking —
#            not lexical match — drives recall order (score = bm25 × salience).
#   Axis 2 — Attribution: a recalled record reports the exact source_harness +
#            skill_action it was appended with (provenance survives the round-trip).
#   Axis 3 — Dream-cycle promotion: ≥3 events sharing a skill_action cluster get
#            promoted into semantic/LESSONS.md with a recurrence count ≥3.
#
# The core library already unit-tests the salience formula in isolation
# (crates/dreamd-core/tests/bm25_fastfield_integration.rs); this closes the gap
# that NONE of those tests cross the MCP boundary.
#
# Fully sandboxed (HOME -> mktemp); the real ~/.agent is never touched.
# Usage: scripts/alpha/quality-suite.sh   (run from repo root; needs target/debug/dreamd)
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN="$REPO/target/debug/dreamd"
DRIVER="$REPO/scripts/alpha/mcp_driver.py"
CHECK="$REPO/scripts/alpha/quality_check.py"

SANDBOX="$(mktemp -d)"
export HOME="$SANDBOX"
PROJ="$SANDBOX/proj"
DAEMON_PID=""

pass=0; fail=0
ok()  { echo "  ✅ $1"; pass=$((pass+1)); }
bad() { echo "  ❌ $1"; fail=$((fail+1)); }

cleanup() {
  [ -n "$DAEMON_PID" ] && kill "$DAEMON_PID" 2>/dev/null
  wait "$DAEMON_PID" 2>/dev/null
  rm -rf "$SANDBOX"
}
trap cleanup EXIT

[ -x "$BIN" ] || { echo "FATAL: $BIN not built (run: cargo build -p dreamd)"; exit 1; }

echo "=== dreamd quality suite (sandbox HOME=$SANDBOX) ==="
"$BIN" version 2>/dev/null | head -2 || true

mkdir -p "$PROJ"
( cd "$PROJ" && git init -q )
( cd "$PROJ" && "$BIN" init ) >/dev/null 2>&1 \
  || { echo "FATAL: dreamd init failed"; exit 1; }

drive() { python3 "$DRIVER" "$BIN" "$PROJ"; }  # reads a JSON call-array on stdin
append_json() { # <content> <harness> <skill_action> <pain> <importance>
  printf '[{"name":"append_node","arguments":{"content":"%s","source_harness":"%s","skill_action":"%s","pain":%s,"importance":%s}}]' \
    "$1" "$2" "$3" "$4" "$5"
}
search_json() { printf '[{"name":"search_nodes","arguments":{"query":"%s","k":10}}]' "$1"; }
minted() { echo "$1" | grep -q 'evt_' && ! echo "$1" | grep -q '"isError": *true\|"error"'; }

# Poll the daemon until every marker is present in one recall (index-commit lag
# ~5s). Echoes the last response; returns 0 if all markers surfaced.
poll_markers() { # <query> <marker...>
  local q="$1"; shift; local resp="" i m all
  for i in $(seq 1 6); do
    sleep 3
    resp="$(search_json "$q" | drive)"
    all=1
    for m in "$@"; do echo "$resp" | grep -q "$m" || all=0; done
    [ "$all" = 1 ] && { printf '%s' "$resp"; return 0; }
  done
  printf '%s' "$resp"; return 1
}

# --- Bring up the daemon (Phase 2 shared index; the realistic surface) --------
"$BIN" watch >"$SANDBOX/daemon.log" 2>&1 &
DAEMON_PID=$!
for i in $(seq 1 20); do [ -S "$SANDBOX/.agent/dreamd.sock" ] && break; sleep 0.5; done
[ -S "$SANDBOX/.agent/dreamd.sock" ] && ok "daemon bound socket" || bad "daemon never bound socket"

# =============================================================================
# AXIS 1 — Salience ranking (the wedge claim, end-to-end through MCP)
# HIGH: high pain/importance, minimal lexical repetition.
# LOW : low pain/importance, but repeats the query terms -> HIGHER BM25.
# If HIGH still wins, salience (not BM25) drove the order.
# =============================================================================
echo "--- Axis 1: salience ranking ---"
Q1="database connection pool"
HI="MARKERHI database connection pool exhaustion took down prod checkout during peak"
LO="MARKERLO database connection pool connection pool connection pool minor tuning footnote"
r="$(append_json "$HI" "claude-code" "db::pool" 9 9 | drive)"; minted "$r" && ok "append HIGH (pain=9,imp=9)" || bad "append HIGH failed: $r"
r="$(append_json "$LO" "cursor"      "db::pool" 1 1 | drive)"; minted "$r" && ok "append LOW (pain=1,imp=1)" || bad "append LOW failed: $r"
if resp="$(poll_markers "$Q1" MARKERHI MARKERLO)"; then
  ok "both ranking records indexed"
  if printf '%s' "$resp" | python3 "$CHECK" ranking MARKERHI MARKERLO; then ok "RANKING GATE: salience-weighted order correct"; else bad "RANKING GATE failed"; fi
else
  bad "ranking records never both indexed (index lag?)"; echo "    last: $resp"
fi

# =============================================================================
# AXIS 2 — Attribution: provenance survives append -> index -> recall via MCP
# =============================================================================
echo "--- Axis 2: attribution ---"
Q2="redis cache stampede"
AT="MARKERATTR redis cache stampede avoided with jittered ttl and single-flight"
r="$(append_json "$AT" "claude-code" "redis::cache_stampede" 6 7 | drive)"; minted "$r" && ok "append ATTR record" || bad "append ATTR failed: $r"
if resp="$(poll_markers "$Q2" MARKERATTR)"; then
  ok "attribution record indexed"
  if printf '%s' "$resp" | python3 "$CHECK" attribution MARKERATTR "claude-code" "redis::cache_stampede"; then ok "ATTRIBUTION GATE: source_harness + skill_action correct"; else bad "ATTRIBUTION GATE failed"; fi
else
  bad "attribution record never indexed"; echo "    last: $resp"
fi

# --- Stop the daemon: the dream cycle (--no-commit) runs in-process ----------
kill "$DAEMON_PID" 2>/dev/null; wait "$DAEMON_PID" 2>/dev/null; DAEMON_PID=""
for i in $(seq 1 10); do [ -S "$SANDBOX/.agent/dreamd.sock" ] || break; sleep 0.3; done

# =============================================================================
# AXIS 3 — Dream-cycle promotion: 3 events in ONE skill_action cluster promote
# to semantic/LESSONS.md (PROMOTION_THRESHOLD = 3). Only rust::lifetime_elision
# reaches the threshold, so a non-empty LESSONS.md means that cluster promoted.
# =============================================================================
echo "--- Axis 3: dream-cycle promotion ---"
for n in 1 2 3; do
  case $n in
    1) C="MARKERP1 rust lifetime elision confused the borrow checker inside an async fn";;
    2) C="MARKERP2 rust lifetime elision needed an explicit bound on a boxed trait object";;
    3) C="MARKERP3 rust lifetime elision broke with nested closures capturing references";;
  esac
  r="$(append_json "$C" "claude-code" "rust::lifetime_elision" 6 6 | drive)"
  minted "$r" && ok "append cluster event P$n" || bad "append P$n failed: $r"
done

( cd "$PROJ" && SOURCE_DATE_EPOCH="$(date +%s)" "$BIN" dream --no-commit ) >"$SANDBOX/dream.log" 2>&1 \
  && ok "dream cycle ran" || { bad "dream cycle errored"; cat "$SANDBOX/dream.log"; }

LESSONS="$PROJ/.agent/semantic/LESSONS.md"
RECUR="$PROJ/.agent/semantic/recurrence_counts.json"
[ -f "$LESSONS" ] || LESSONS="$(find "$PROJ" -name LESSONS.md 2>/dev/null | head -1)"
[ -f "$RECUR" ]   || RECUR="$(find "$PROJ" -name recurrence_counts.json 2>/dev/null | head -1)"

if [ -s "$LESSONS" ] && grep -qi "lifetime" "$LESSONS"; then
  ok "PROMOTION GATE: LESSONS.md promoted the rust::lifetime_elision cluster"
else
  bad "PROMOTION GATE: LESSONS.md missing/empty or lacks the cluster theme"
  echo "    LESSONS.md ($LESSONS):"; sed 's/^/      /' "$LESSONS" 2>/dev/null | head -20
fi

if python3 - "$RECUR" <<'PY'
import json, sys
try:
    d = json.load(open(sys.argv[1]))
except Exception as e:
    print(f"      no/invalid recurrence_counts.json: {e}"); sys.exit(1)
def leaves(x):
    if isinstance(x, dict):
        for v in x.values(): yield from leaves(v)
    elif isinstance(x, list):
        for v in x: yield from leaves(v)
    elif isinstance(x, (int, float)): yield x
vals = list(leaves(d)); m = max(vals) if vals else 0
print(f"      max recurrence count = {m} (need ≥3)")
sys.exit(0 if m >= 3 else 1)
PY
then ok "RECURRENCE GATE: cluster counted ≥3 occurrences"; else bad "RECURRENCE GATE: cluster count <3"; fi

echo "=== RESULT: $pass passed, $fail failed ==="
[ "$fail" -eq 0 ]
