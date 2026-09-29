#!/usr/bin/env bash
# Walk the `dreamd memory` branch and bisect commands in a throwaway project.
#
# Uses the `dreamd` binary on PATH. Everything happens in a temp directory,
# which is removed on exit; the caller's repo is never touched. HOME is pointed
# at the temp directory too, so `dreamd init` writes its registry there and a
# `dreamd watch` running for the real user does not block checkout.
#
#   PATH="target/debug:$PATH" bash scripts/branch-demo.sh
#
# Does not start `dreamd watch` and does not write the episodic log.
set -eu

# Resolve before `cd`: a relative PATH entry like target/debug stops working
# once the script leaves the caller's directory.
bin=$(command -v dreamd) || { echo "dreamd not found on PATH" >&2; exit 1; }
case "$bin" in /*) ;; *) bin="$PWD/$bin" ;; esac
dreamd() { "$bin" "$@"; }

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

export HOME="$tmp/home"
mkdir -p "$HOME" "$tmp/project"
cd "$tmp/project"

# `dreamd init` needs a project sentinel; `.git` is one.
git init -q .

step() { printf '\n$ %s\n' "$*"; "$@"; }

step dreamd init
step dreamd memory branch main
# Ref timestamps have one-second resolution; bisect needs good strictly earlier.
sleep 1
step dreamd memory branch other
step dreamd memory branches
step dreamd memory checkout main
step dreamd memory delete other
step dreamd memory branches

# Each `memory branch` wrote one snap-* ref. They are adjacent (nothing
# between them), so `start` prints the bad ref and starts no search.
snaps=$(dreamd memory branches | sed -n 's/^[* ] \(snap-.*\)$/\1/p' | sort)
good=$(printf '%s\n' "$snaps" | sed -n 1p)
bad=$(printf '%s\n' "$snaps" | sed -n 2p)
if [ -z "$good" ] || [ -z "$bad" ] || [ "$(printf '%s\n' "$snaps" | wc -l)" -ne 2 ]; then
  echo "expected exactly two snap-* refs, got:" >&2
  printf '%s\n' "$snaps" >&2
  exit 1
fi

printf '\n$ dreamd memory bisect start --good %s --bad %s\n' "$good" "$bad"
line=$(dreamd memory bisect start --good "$good" --bad "$bad")
printf '%s\n' "$line"

if ! printf '%s\n' "$line" | grep -Eqx "$bad [0-9a-f]{64}"; then
  echo "expected one line '<bad ref> <64-hex id>', got: $line" >&2
  exit 1
fi

echo
echo "demo ok"
