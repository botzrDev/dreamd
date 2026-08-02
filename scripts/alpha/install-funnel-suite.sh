#!/usr/bin/env bash
# dreamd install-funnel suite — sandboxed end-to-end gate for the install front
# door (AILAB-555).
#
# Covers what the unit tests cannot: the real binary, on a real filesystem,
# walking the funnel a first-time user walks — `dreamd setup` on a cold project,
# `dreamd doctor` on what it scaffolded, a second (idempotent) setup, the
# `dreamd update --dry-run` restart contract, and the full ratified AILAB-548 §3
# / §6 conflict taxonomy against pre-existing MCP config files.
#
# Fully sandboxed: HOME is redirected to a temp dir, so the real ~/.agent daemon,
# registry, and memory are never touched (removed on exit). Nothing long-lived
# is started — no daemon, no `dreamd watch`, no network.
#
# Usage: scripts/alpha/install-funnel-suite.sh   (run from repo root; needs
#        target/debug/dreamd — build it with `cargo build -p dreamd`)
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN="$REPO/target/debug/dreamd"

SANDBOX="$(mktemp -d)"
export HOME="$SANDBOX"          # redirect ~/.agent into the sandbox
# A developer-exported socket override would aim setup's liveness probe and
# `doctor` at a daemon outside the sandbox. Clear it the way `setup_wizard.rs`
# does. (`run_watch` ignoring $DREAMD_SOCK is known, pre-existing, and out of
# scope here — this suite never starts a daemon.)
unset DREAMD_SOCK

OUT="$SANDBOX/last.out"
ERR="$SANDBOX/last.err"
RC=0

pass=0; fail=0
ok()   { echo "  ✅ $1"; pass=$((pass+1)); }
bad()  { echo "  ❌ $1"; fail=$((fail+1)); }

cleanup() { rm -rf "$SANDBOX"; }
trap cleanup EXIT

[ -x "$BIN" ] || { echo "FATAL: $BIN not built (run: cargo build -p dreamd)"; exit 1; }
command -v python3 >/dev/null || { echo "FATAL: python3 required (JSON assertions)"; exit 1; }

# --- helpers -----------------------------------------------------------------

# Run the CLI inside <dir>; stdout/stderr land in $OUT/$ERR and the exit code in
# $RC. stdin is /dev/null on purpose: `setup` refuses to prompt off a TTY
# (AILAB-551), so a forgotten `--yes` fails fast with exit 2 here instead of
# blocking forever on a developer's terminal.
run_in() { # <dir> <args...>
  local dir="$1"; shift
  ( cd "$dir" || exit 127; "$BIN" "$@" ) >"$OUT" 2>"$ERR" </dev/null
  RC=$?
}

expect_rc() { # <expected> <label>
  if [ "$RC" -eq "$1" ]; then ok "$2 (exit $RC)"; return 0; fi
  bad "$2 — expected exit $1, got $RC"
  sed 's/^/      /' "$ERR" | head -5
  return 1
}

# A fresh project dir with `.git` as the root sentinel `setup` walks up to find.
new_project() { # <name> -> absolute path
  local dir="$SANDBOX/$1"
  mkdir -p "$dir"
  ( cd "$dir" && git init -q ) >/dev/null 2>&1
  printf '%s' "$dir"
}

snap_of()  { printf '%s/snap.%s' "$SANDBOX" "${1//\//_}"; }
snapshot() { cp "$1" "$(snap_of "$1")"; }
unchanged() { cmp -s "$1" "$(snap_of "$1")"; }

# Floating-pin assertion — parsed, never grepped. `setup` renders the block with
# serde_json, so key order (`args` may sort before `command`) is not part of the
# contract; the parsed shape is. Prints the reason and exits 1 on failure.
is_floating() { # <path>
  python3 - "$1" <<'PY'
import json, sys

try:
    with open(sys.argv[1]) as f:
        doc = json.load(f)
except Exception as e:
    print("unparseable JSON: %s" % e)
    sys.exit(1)
block = doc.get("mcpServers", {}).get("dreamd")
if not isinstance(block, dict):
    print("no mcpServers.dreamd object (found: %r)" % (block,))
    sys.exit(1)
if block.get("command") != "npx":
    print("command is %r, expected 'npx'" % (block.get("command"),))
    sys.exit(1)
args = block.get("args") or []
if "dreamd-mcp" not in args:
    print("args do not run the floating dreamd-mcp package: %r" % (args,))
    sys.exit(1)
pinned = [a for a in args if isinstance(a, str) and a.startswith("dreamd-mcp@")]
if pinned:
    print("hard-pinned arg present: %r" % (pinned,))
    sys.exit(1)
PY
}

# Compact, key-sorted JSON for one `mcpServers` entry, so a third-party block can
# be compared across a reformatting rewrite.
server_json() { # <path> <name>
  python3 - "$1" "$2" <<'PY'
import json, sys

with open(sys.argv[1]) as f:
    doc = json.load(f)
print(json.dumps(doc.get("mcpServers", {}).get(sys.argv[2]), sort_keys=True))
PY
}

echo "=== dreamd install-funnel suite (sandbox HOME=$SANDBOX) ==="
"$BIN" version 2>/dev/null | head -3 || true

# =============================================================================
# 1. Cold install — the first thing a new user runs
# =============================================================================
echo "--- cold setup (--yes --harness claude) ---"
COLD="$(new_project cold)"
run_in "$COLD" setup --yes --harness claude
expect_rc 0 "cold setup"
if [ -d "$COLD/.agent" ]; then ok "scaffolded .agent/"; else bad "no .agent/ after setup"; fi
if reason="$(is_floating "$COLD/.mcp.json")"; then
  ok ".mcp.json wired to the floating npx pin"
else
  bad ".mcp.json is not a floating pin: $reason"
fi

# =============================================================================
# 2. doctor on what setup just made — diagnostic only.
#    Never `doctor --repair` in a suite: it can hang (AILAB-561).
# =============================================================================
echo "--- doctor on the fresh scaffold ---"
run_in "$COLD" doctor
expect_rc 0 "doctor on the fresh scaffold"

# =============================================================================
# 3. Re-running setup is a no-op, not a rewrite (§3 "already wired")
# =============================================================================
echo "--- second setup is idempotent ---"
snapshot "$COLD/.mcp.json"
run_in "$COLD" setup --yes --harness claude
expect_rc 0 "second setup"
if unchanged "$COLD/.mcp.json"; then
  ok ".mcp.json byte-identical after re-run (already-wired no-op)"
else
  bad ".mcp.json was rewritten by the second setup"
fi

# =============================================================================
# 4. `update --dry-run` — prints the restart contract, touches nothing, and
#    needs no network (the Rust side never downloads; that is the Node shim).
# =============================================================================
echo "--- update --dry-run ---"
run_in "$COLD" update --dry-run
expect_rc 0 "update --dry-run"
if grep -q "restart contract" "$OUT"; then
  ok "update --dry-run prints the restart contract"
else
  bad "update --dry-run did not print the restart contract"
  sed 's/^/      /' "$OUT" | head -10
fi

# =============================================================================
# 5. Conflict taxonomy — every row of AILAB-548 §3, plus §6A/§6C
# =============================================================================
echo "--- conflict taxonomy (AILAB-548 §3 / §6) ---"

# Row: hard-pinned dreamd block. Refuse without --force; with --force rewrite to
# floating while every other server survives.
PINNED="$(new_project conflict-pinned)"
cat > "$PINNED/.mcp.json" <<'JSON'
{
  "mcpServers": {
    "other": { "command": "node", "args": ["other-server.js"] },
    "dreamd": { "command": "npx", "args": ["-y", "dreamd-mcp@1.2.3"] }
  }
}
JSON
OTHER_BEFORE="$(server_json "$PINNED/.mcp.json" other)"
snapshot "$PINNED/.mcp.json"
run_in "$PINNED" setup --yes --harness claude
expect_rc 1 "[pinned] setup without --force refuses"
if unchanged "$PINNED/.mcp.json"; then
  ok "[pinned] nothing written on refusal"
else
  bad "[pinned] file changed despite the refusal"
fi
run_in "$PINNED" setup --yes --harness claude --force
expect_rc 0 "[pinned] setup --force"
if reason="$(is_floating "$PINNED/.mcp.json")"; then
  ok "[pinned] --force rewrote it to the floating pin"
else
  bad "[pinned] --force did not produce a floating pin: $reason"
fi
if [ "$(server_json "$PINNED/.mcp.json" other)" = "$OTHER_BEFORE" ]; then
  ok "[pinned] third-party \"other\" server preserved"
else
  bad "[pinned] third-party \"other\" server was lost or altered"
fi

# Row: `command` is a local build, not npx — a contributor's own binary. Refuse;
# silently clobbering it is the worst outcome in the set.
LOCALCMD="$(new_project conflict-local-command)"
cat > "$LOCALCMD/.mcp.json" <<'JSON'
{
  "mcpServers": {
    "dreamd": { "command": "dreamd", "args": ["mcp"] }
  }
}
JSON
snapshot "$LOCALCMD/.mcp.json"
run_in "$LOCALCMD" setup --yes --harness claude
expect_rc 1 "[command=dreamd] setup without --force refuses"
if unchanged "$LOCALCMD/.mcp.json"; then
  ok "[command=dreamd] local-build entry left intact"
else
  bad "[command=dreamd] local-build entry was clobbered"
fi

# Row: floating package plus an extra `--project-root` arg — our own documented
# global example. Compatible: exit 0 and the file is left byte-identical. Written
# with 4-space indent and no trailing newline so any rewrite is visible.
COMPAT="$(new_project compatible-project-root)"
printf '%s' '{
    "mcpServers": {
        "dreamd": {"command": "npx", "args": ["-y", "dreamd-mcp", "--project-root", "/somewhere/else"]}
    }
}' > "$COMPAT/.mcp.json"
snapshot "$COMPAT/.mcp.json"
run_in "$COMPAT" setup --yes --harness claude
expect_rc 0 "[--project-root] compatible block accepted"
if unchanged "$COMPAT/.mcp.json"; then
  ok "[--project-root] file byte-identical (no rewrite)"
else
  bad "[--project-root] compatible file was rewritten"
fi

# Row: malformed JSON. Exit 1, and --force explicitly cannot override it (§6C) —
# we never overwrite a file we could not read.
MALFORMED="$(new_project conflict-malformed)"
printf '%s\n' '{ "mcpServers": { "dreamd": ' > "$MALFORMED/.mcp.json"
snapshot "$MALFORMED/.mcp.json"
run_in "$MALFORMED" setup --yes --harness claude
expect_rc 1 "[malformed] setup refuses"
run_in "$MALFORMED" setup --yes --harness claude --force
expect_rc 1 "[malformed] --force still refuses (§6C)"
if unchanged "$MALFORMED/.mcp.json"; then
  ok "[malformed] unreadable file left untouched"
else
  bad "[malformed] unreadable file was overwritten"
fi

# Row: `--harness both` is atomic (§6A). Only .cursor/mcp.json conflicts, so
# .mcp.json — a clean create — must not be written at all.
ATOMIC="$(new_project conflict-both-atomic)"
mkdir -p "$ATOMIC/.cursor"
cat > "$ATOMIC/.cursor/mcp.json" <<'JSON'
{
  "mcpServers": {
    "dreamd": { "command": "npx", "args": ["-y", "dreamd-mcp@9.9.9"] }
  }
}
JSON
snapshot "$ATOMIC/.cursor/mcp.json"
run_in "$ATOMIC" setup --yes --harness both
expect_rc 1 "[both] atomic pre-flight refuses"
if [ -e "$ATOMIC/.mcp.json" ]; then
  bad "[both] .mcp.json was created despite the .cursor conflict — not atomic"
else
  ok "[both] .mcp.json never created (all-or-nothing)"
fi
if unchanged "$ATOMIC/.cursor/mcp.json"; then
  ok "[both] conflicting .cursor/mcp.json untouched"
else
  bad "[both] conflicting .cursor/mcp.json was modified"
fi

echo "=== RESULT: $pass passed, $fail failed ==="
[ "$fail" -eq 0 ]
