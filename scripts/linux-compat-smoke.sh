#!/bin/sh
# End-to-end smoke for one Linux image (BZR-1438). scripts/linux-compat.sh runs
# it inside each container, with the binary under test mounted at /b/dreamd and
# this directory at /s:
#
#   docker run --rm --network none -e RSS_LIMIT_KIB=30720 \
#     -v <bin dir>:/b:ro -v <scripts dir>:/s:ro <image> sh /s/linux-compat-smoke.sh
#
# POSIX sh only: Alpine runs it under busybox. It never runs on the host; HOME
# and the project live under /tmp inside the container.
#
# Steps, each of which fails the script with a message:
#   1. `dreamd --version` exits 0 and starts with "dreamd ".
#   2. `dreamd init` exits 0.
#   3. `dreamd watch` creates its socket within 10 s; after a 2 s settle its
#      VmRSS is at most RSS_LIMIT_KIB; SIGTERM ends it with exit 0.
#   4. One in-process `dreamd mcp` session appends a record holding a unique
#      token; the "id":2 response has a result with an evt_ id.
#   5. A second session searches for the token; the "id":3 response holds it.
#   6. The episodic log has exactly one line.
#
# The last line on stdout is "linux-compat-smoke: ok" on success. The exit
# status is what the caller checks.
set -u

BIN=/b/dreamd
RSS_LIMIT_KIB="${RSS_LIMIT_KIB:-30720}"
HOME=/tmp/h
export HOME
PROJECT=/tmp/p
SOCK="$HOME/.agent/dreamd.sock"
LOG="$PROJECT/.agent/episodic/AGENT_LEARNINGS.jsonl"
FAILED=0
WATCH_PID=""

fail() {
    echo "linux-compat-smoke: FAIL: $*" >&2
    FAILED=1
    if [ -n "$WATCH_PID" ]; then
        kill "$WATCH_PID" 2>/dev/null || true
    fi
    exit 1
}

# wait_for <pattern> <file> <tries>: poll <file> every 0.2 s for a line that
# contains <pattern>. Returns non-zero when it never appears.
wait_for() {
    i=0
    while [ "$i" -lt "$3" ]; do
        if [ -f "$2" ] && grep -q "$1" "$2"; then
            return 0
        fi
        sleep 0.2
        i=$((i + 1))
    done
    return 1
}

mkdir -p "$HOME" "$PROJECT" || fail "cannot create $HOME and $PROJECT"
cd "$PROJECT" || fail "cannot cd to $PROJECT"
# `dreamd init` needs a project-root sentinel; an empty Cargo.toml is the
# lightest one (scripts/idle-rss.sh does the same).
: > Cargo.toml

# 1. --version
out=$("$BIN" --version 2>&1)
rc=$?
[ "$rc" -eq 0 ] || fail "step 1: dreamd --version exited $rc: $out"
case "$out" in
    "dreamd "*) echo "step 1 ok: $out" ;;
    *) fail "step 1: dreamd --version printed: $out" ;;
esac

# 2. init
out=$("$BIN" init 2>&1)
rc=$?
[ "$rc" -eq 0 ] || fail "step 2: dreamd init exited $rc: $out"
echo "step 2 ok: init"

# 3. watch, socket, idle VmRSS, SIGTERM
"$BIN" watch >/tmp/watch.log 2>&1 &
WATCH_PID=$!
i=0
while [ ! -S "$SOCK" ]; do
    if ! kill -0 "$WATCH_PID" 2>/dev/null; then
        cat /tmp/watch.log >&2
        WATCH_PID=""
        fail "step 3: dreamd watch exited before creating $SOCK"
    fi
    i=$((i + 1))
    if [ "$i" -gt 50 ]; then
        cat /tmp/watch.log >&2
        fail "step 3: $SOCK did not appear within 10 s"
    fi
    sleep 0.2
done
sleep 2
rss=$(sed -n 's/^VmRSS:[[:space:]]*\([0-9][0-9]*\).*/\1/p' "/proc/$WATCH_PID/status")
[ -n "$rss" ] || fail "step 3: no VmRSS in /proc/$WATCH_PID/status"
[ "$rss" -le "$RSS_LIMIT_KIB" ] || fail "step 3: idle VmRSS $rss kB is over $RSS_LIMIT_KIB kB"
kill -TERM "$WATCH_PID"
wait "$WATCH_PID"
rc=$?
WATCH_PID=""
if [ "$rc" -ne 0 ]; then
    cat /tmp/watch.log >&2
    fail "step 3: dreamd watch exited $rc after SIGTERM"
fi
echo "step 3 ok: socket up, idle VmRSS $rss kB, SIGTERM exit 0"

INIT='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"linux-compat","version":"1"}}}'
INITIALIZED='{"jsonrpc":"2.0","method":"notifications/initialized"}'
TOKEN="compat$$x$(date +%s)"

# mcp_session <request line> <response id> <out file>: run one in-process
# `dreamd mcp` session. The writer waits for each response before it sends the
# next line, and closes stdin only once the wanted response has arrived.
mcp_session() {
    : > "$3"
    # The writer reads the file the server writes, on purpose (SC2094).
    # shellcheck disable=SC2094
    (
        printf '%s\n' "$INIT"
        wait_for '"id":1' "$3" 50 || exit 0
        printf '%s\n' "$INITIALIZED"
        printf '%s\n' "$1"
        wait_for "\"id\":$2" "$3" 50 || exit 0
    ) | "$BIN" mcp > "$3" 2>/tmp/mcp.err
}

# 4. append_node
APPEND='{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"append_node","arguments":{"content":"linux compat smoke '"$TOKEN"'","source_harness":"linux-compat","skill_action":"shell::linux_compat","pain":5.0,"importance":5.0}}}'
mcp_session "$APPEND" 2 /tmp/mcp-append.out
resp=$(grep '"id":2' /tmp/mcp-append.out)
case "$resp" in
    *'"result"'*evt_*) echo "step 4 ok: append_node returned an evt_ id" ;;
    *)
        cat /tmp/mcp.err >&2
        fail "step 4: append_node response: $resp"
        ;;
esac

# 5. search_nodes in a second session
SEARCH='{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"search_nodes","arguments":{"query":"'"$TOKEN"'","k":5}}}'
mcp_session "$SEARCH" 3 /tmp/mcp-search.out
resp=$(grep '"id":3' /tmp/mcp-search.out)
case "$resp" in
    *"$TOKEN"*) echo "step 5 ok: search_nodes found the token" ;;
    *)
        cat /tmp/mcp.err >&2
        fail "step 5: search_nodes response: $resp"
        ;;
esac

# 6. exactly one line in the episodic log
[ -f "$LOG" ] || fail "step 6: $LOG does not exist"
lines=$(wc -l < "$LOG" | tr -d ' ')
[ "$lines" -eq 1 ] || fail "step 6: $LOG has $lines lines, want 1"
echo "step 6 ok: episodic log has 1 line"

[ "$FAILED" -eq 0 ] || exit 1
echo "linux-compat-smoke: ok"
