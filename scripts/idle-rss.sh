#!/usr/bin/env bash
# Idle-RSS gate for the dreamd daemon (NFR-1: resident memory < 30 MB at idle).
#
# Spawns the real `dreamd watch` daemon in a throwaway workspace, waits for its
# UDS to appear, lets allocations settle, then reads VmRSS from
# /proc/<daemon_pid>/status and fails if it exceeds LIMIT_MB. See WEG-53 / DR-808.
#
# Linux-only: VmRSS is the metric. macOS phys_footprint counts shared dylib
# pages and is not comparable to a 30 MB Linux threshold (deferred ticket).
#
# Usage:
#   scripts/idle-rss.sh                 # gate at 30 MB
#   LIMIT_MB=30 scripts/idle-rss.sh     # explicit limit (MB)
#   SETTLE_SECS=2 scripts/idle-rss.sh   # seconds to settle before sampling
#
# Prints the measured MB to stdout (machine-readable); a human summary and any
# errors go to stderr. Assumes the release binary is already built at
# target/release/dreamd (CI builds it in a prior step; locally run
# `cargo build --release -p dreamd` first).
set -euo pipefail

LIMIT_MB="${LIMIT_MB:-30}"
SETTLE_SECS="${SETTLE_SECS:-2}"

REPO_ROOT="$(git rev-parse --show-toplevel)"
BIN="$REPO_ROOT/target/release/dreamd"
SOCK="$HOME/.agent/dreamd.sock"

if [[ ! -x "$BIN" ]]; then
    echo "ERROR: release binary not found at $BIN" >&2
    echo "Build it first: cargo build --release -p dreamd" >&2
    exit 2
fi

WORKDIR="$(mktemp -d)"
DAEMON_PID=""

# Kill the daemon even if an earlier step fails, so no orphan survives the
# script. After this script exits, `pgrep -f "dreamd watch"` must be empty.
cleanup() {
    if [[ -n "$DAEMON_PID" ]] && kill -0 "$DAEMON_PID" 2>/dev/null; then
        kill "$DAEMON_PID" 2>/dev/null || true
        wait "$DAEMON_PID" 2>/dev/null || true
    fi
    rm -rf "$WORKDIR"
}
trap cleanup EXIT

# Hygiene: clear a socket left from a prior run so readiness polling can't race
# on a stale file. bind_api_socket recovers stale sockets, so this is belt-only.
rm -f "$SOCK"

cd "$WORKDIR"

# `dreamd init` requires a project-root sentinel in/above cwd (init.rs
# ROOT_SENTINELS: .git, Cargo.toml, package.json, pyproject.toml) and exits
# NoProjectRoot otherwise. A bare mktemp dir has none, so create the lightest
# one — an empty Cargo.toml (existence-checked, never parsed by init or watch).
# This also pins init's root resolution to WORKDIR instead of letting it walk
# up into TMPDIR's ancestors.
: > Cargo.toml

# The daemon refuses to start without an initialized workspace
# (AgentRoot::discover -> WatchError::NoProjectRoot). init is non-interactive;
# it scaffolds .agent/ (which `dreamd watch` then discovers). Suppress its
# stdout so only the measured MB lands on our stdout.
"$BIN" init >/dev/null

# CRITICAL: start the daemon directly — NOT inside a ( ... ) & subshell. A
# subshell wrapper makes $! the subshell PID, so /proc/$!/status would measure
# the wrong process. Redirect the daemon's stdout to stderr: this script's
# stdout is captured by the CI step (`MB=$(scripts/idle-rss.sh)`) and the
# background child inherits that fd.
"$BIN" watch >&2 &
DAEMON_PID=$!

# Readiness: poll up to ~10 s for the UDS to appear. A blind sleep is flaky on
# slow runners. The tracing "serving on" line only emits with a subscriber, so
# the socket file is the reliable signal.
ready=0
for _ in $(seq 1 100); do
    if [[ -S "$SOCK" ]]; then
        ready=1
        break
    fi
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
        echo "ERROR: daemon (pid $DAEMON_PID) exited before binding $SOCK" >&2
        exit 2
    fi
    sleep 0.1
done

if [[ "$ready" -ne 1 ]]; then
    echo "ERROR: daemon socket $SOCK did not appear within 10 s" >&2
    exit 2
fi

# Let allocations settle before sampling resident memory.
sleep "$SETTLE_SECS"

# Measure VmRSS of the DAEMON child (KiB) — NOT /proc/self, that is this shell.
# The 50 MB WRITER_HEAP_BYTES tantivy arena inflates VmSize, not VmRSS, until
# written; gating on VmRSS is what makes 30 MB achievable.
RSS_KIB="$(awk '/^VmRSS:/{print $2}' "/proc/$DAEMON_PID/status")"
COMM="$(ps -o comm= -p "$DAEMON_PID" 2>/dev/null || echo '?')"
RSS_MB="$(echo "scale=2; $RSS_KIB / 1024" | bc)"

# Human summary on stderr; machine-readable MB on stdout.
echo "idle-rss: pid=$DAEMON_PID comm=$COMM VmRSS=${RSS_MB} MB (limit ${LIMIT_MB} MB)" >&2
echo "$RSS_MB"

if [[ "$(echo "$RSS_MB > $LIMIT_MB" | bc)" -eq 1 ]]; then
    echo "ERROR: idle VmRSS ${RSS_MB} MB exceeds ${LIMIT_MB} MB limit (NFR-1)" >&2
    exit 1
fi
