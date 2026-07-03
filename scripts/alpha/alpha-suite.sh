#!/usr/bin/env bash
# dreamd alpha suite — automated cross-harness append->recall smoke test.
#
# Reconstructs the core of the 2026-06-18 alpha suite (which kept living in
# ephemeral scratchpads). Proves the demo-critical claim: a learning appended by
# one harness (source_harness=claude-code) is recalled by an independent second
# harness process (cursor), on BOTH the daemon path (Phase 2) and the no-daemon
# path (Phase 1).
#
# Fully sandboxed: HOME is redirected to a temp dir, so the real ~/.agent daemon,
# registry, and memory are never touched. Cleans up the daemon + temp dir on exit.
#
# Usage: scripts/alpha/alpha-suite.sh   (run from repo root; needs target/debug/dreamd)
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN="$REPO/target/debug/dreamd"
DRIVER="$REPO/scripts/alpha/mcp_driver.py"

SANDBOX="$(mktemp -d)"
export HOME="$SANDBOX"          # redirect ~/.agent into the sandbox
PROJ="$SANDBOX/proj"
DAEMON_PID=""

pass=0; fail=0
ok()   { echo "  ✅ $1"; pass=$((pass+1)); }
bad()  { echo "  ❌ $1"; fail=$((fail+1)); }

cleanup() {
  [ -n "$DAEMON_PID" ] && kill "$DAEMON_PID" 2>/dev/null
  wait "$DAEMON_PID" 2>/dev/null
  rm -rf "$SANDBOX"
}
trap cleanup EXIT

[ -x "$BIN" ] || { echo "FATAL: $BIN not built (run: cargo build -p dreamd)"; exit 1; }

echo "=== dreamd alpha suite (sandbox HOME=$SANDBOX) ==="
"$BIN" version 2>/dev/null | head -3 || true

# --- Setup: a real project with a root sentinel + registered store ------------
mkdir -p "$PROJ"
( cd "$PROJ" && git init -q )                 # .git = ROOT_SENTINEL for `init`
( cd "$PROJ" && "$BIN" init ) >/dev/null 2>&1 \
  || { echo "FATAL: dreamd init failed"; exit 1; }

# Distinctive, lexically-unique payloads so BM25 recall is unambiguous.
CC_CONTENT="capybara telemetry pipeline backpressure lesson from claude-code"
CC_QUERY="capybara telemetry backpressure"
CUR_CONTENT="wombat retry jitter budget lesson authored under cursor"
CUR_QUERY="wombat retry jitter"

append_call() { # <content> <harness> <skill_action>
  printf '[{"name":"append_node","arguments":{"content":"%s","source_harness":"%s","skill_action":"%s","pain":7.0,"importance":8.0}}]' "$1" "$2" "$3"
}
search_call() { # <query>
  printf '[{"name":"search_nodes","arguments":{"query":"%s","k":5}}]' "$1"
}

# =============================================================================
# PHASE 2 — daemon up: harness A appends, independent harness B recalls
# =============================================================================
echo "--- Phase 2 (daemon / shared index) ---"
"$BIN" watch >"$SANDBOX/daemon.log" 2>&1 &
DAEMON_PID=$!
for i in $(seq 1 20); do [ -S "$SANDBOX/.agent/dreamd.sock" ] && break; sleep 0.5; done
if [ -S "$SANDBOX/.agent/dreamd.sock" ]; then ok "daemon bound socket"; else bad "daemon never bound socket"; fi

# Harness A (claude-code) appends.
A_RESP="$(append_call "$CC_CONTENT" "claude-code" "alpha::cross_harness" | python3 "$DRIVER" "$BIN" "$PROJ")"
if echo "$A_RESP" | grep -q '"isError": *true\|"error"'; then bad "phase2 append errored: $A_RESP"
elif echo "$A_RESP" | grep -q 'evt_'; then ok "phase2 append (claude-code) accepted"
else bad "phase2 append: no event id minted: $A_RESP"; fi

# Harness B (cursor) — a *separate* process — searches. Poll for the index
# commit (daemon cadence ~5s); tolerate lag up to ~18s.
found=""
for i in $(seq 1 6); do
  sleep 3
  B_RESP="$(search_call "$CC_QUERY" | python3 "$DRIVER" "$BIN" "$PROJ")"
  if echo "$B_RESP" | grep -q "capybara telemetry pipeline"; then found=1; break; fi
done
if [ -n "$found" ]; then ok "phase2 cross-harness recall: cursor saw claude-code's write (after ${i}x3s)"
else bad "phase2 cross-harness recall FAILED — cursor did not surface claude-code's write"; echo "    last: $B_RESP"; fi

kill "$DAEMON_PID" 2>/dev/null; wait "$DAEMON_PID" 2>/dev/null; DAEMON_PID=""
for i in $(seq 1 10); do [ -S "$SANDBOX/.agent/dreamd.sock" ] || break; sleep 0.3; done

# =============================================================================
# PHASE 1 — no daemon: in-process append (harness A), JSONL-replay recall (B)
# =============================================================================
echo "--- Phase 1 (no daemon / JSONL replay) ---"
[ -S "$SANDBOX/.agent/dreamd.sock" ] && bad "socket still present; not a true Phase 1" || ok "daemon stopped (Phase 1 path active)"

# Harness A appends in-process (durable JSONL write), process exits.
A1_RESP="$(append_call "$CUR_CONTENT" "cursor" "alpha::phase1" | python3 "$DRIVER" "$BIN" "$PROJ")"
if echo "$A1_RESP" | grep -q '"isError": *true\|"error"'; then bad "phase1 append errored: $A1_RESP"
elif echo "$A1_RESP" | grep -q 'evt_'; then ok "phase1 append (cursor) accepted"
else bad "phase1 append: no event id minted: $A1_RESP"; fi

# Harness B — fresh process — replays the JSONL and searches.
B1_RESP="$(search_call "$CUR_QUERY" | python3 "$DRIVER" "$BIN" "$PROJ")"
if echo "$B1_RESP" | grep -q "wombat retry jitter budget"; then ok "phase1 cross-harness recall: replay surfaced the write"
else bad "phase1 recall FAILED"; echo "    last: $B1_RESP"; fi

# Phase 1 must ALSO still see the Phase 2 write (same JSONL, WEG-378 read path).
B2_RESP="$(search_call "$CC_QUERY" | python3 "$DRIVER" "$BIN" "$PROJ")"
if echo "$B2_RESP" | grep -q "capybara telemetry pipeline"; then ok "phase1 replay also recalls the earlier claude-code write (WEG-378 read path)"
else bad "phase1 did NOT recall the earlier write — possible WEG-378 read-path regression"; echo "    last: $B2_RESP"; fi

echo "=== RESULT: $pass passed, $fail failed ==="
[ "$fail" -eq 0 ]
