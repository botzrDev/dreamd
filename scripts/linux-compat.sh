#!/usr/bin/env bash
# Linux compatibility check for the dreamd release binary (BZR-1438).
#
# The npx shim downloads the `linux-x86_64` release asset, which is the static
# musl build. This script checks that a binary is static, that it runs the
# end-to-end smoke (scripts/linux-compat-smoke.sh) in old and new glibc images
# and in Alpine, and that the shim runs it, sha-verified, with no network.
#
# Usage:
#   scripts/linux-compat.sh <path-to-dreamd>
#
# Environment (defaults):
#   IMAGES="ubuntu:20.04 ubuntu:22.04 debian:12 alpine:3.20"
#   SHIM_IMAGES="ubuntu:22.04 debian:12"
#   RSS_LIMIT_KIB=30720   NFR-1's 30 MB, in kB as /proc reports VmRSS
#
# Needs docker, readelf, sha256sum and node on the host. Every container runs
# with --network none; images are pulled first. The script writes only to a
# mktemp directory, which it removes on exit.
set -euo pipefail

IMAGES="${IMAGES:-ubuntu:20.04 ubuntu:22.04 debian:12 alpine:3.20}"
SHIM_IMAGES="${SHIM_IMAGES:-ubuntu:22.04 debian:12}"
RSS_LIMIT_KIB="${RSS_LIMIT_KIB:-30720}"

if [[ $# -ne 1 ]]; then
    echo "usage: $0 <path-to-dreamd>" >&2
    exit 2
fi
if [[ ! -f "$1" ]]; then
    echo "linux-compat: FAIL: no binary at $1" >&2
    exit 2
fi

BIN="$(readlink -f "$1")"
SCRIPTS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(dirname "$SCRIPTS_DIR")"
SHIM_SRC="$REPO_ROOT/packages/dreamd-mcp"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

fail() {
    echo "linux-compat: FAIL: $*" >&2
    exit 1
}

# The containers see the binary as /b/dreamd, whatever it is called here.
mkdir -p "$TMP/b"
cp "$BIN" "$TMP/b/dreamd"
chmod 0755 "$TMP/b/dreamd"
SHA="$(sha256sum "$BIN" | cut -d' ' -f1)"
echo "linux-compat: binary $BIN"
echo "linux-compat: sha256 $SHA"

# 1. Static. Any binary that is not static also fails a container in step 2;
#    this check comes first so the message names the cause.
if command -v file >/dev/null 2>&1; then
    echo "linux-compat: file: $(file -b "$BIN")"
fi
interp="$(readelf -lW "$BIN" | grep 'Requesting program interpreter' || true)"
needed="$(readelf -dW "$BIN" | grep '(NEEDED)' || true)"
if [[ -n "$interp" || -n "$needed" ]]; then
    [[ -n "$interp" ]] && echo "linux-compat: $interp" >&2
    if [[ -n "$needed" ]]; then
        while IFS= read -r line; do echo "linux-compat: $line" >&2; done <<<"$needed"
    fi
    fail "static: the binary is dynamically linked"
fi
echo "linux-compat: ok static (no program interpreter, no NEEDED entries)"

# 2. The smoke in each image.
for image in $IMAGES; do
    docker pull -q "$image" >/dev/null
    log="$TMP/smoke-${image//[:\/]/_}.log"
    set +e
    docker run --rm --network none \
        -e RSS_LIMIT_KIB="$RSS_LIMIT_KIB" \
        -v "$TMP/b:/b:ro" \
        -v "$SCRIPTS_DIR:/s:ro" \
        "$image" sh /s/linux-compat-smoke.sh >"$log" 2>&1
    rc=$?
    set -e
    if [[ $rc -ne 0 ]] || [[ "$(tail -n 1 "$log")" != "linux-compat-smoke: ok" ]]; then
        sed 's/^/    /' "$log" >&2
        fail "smoke in $image (exit $rc)"
    fi
    echo "linux-compat: ok smoke in $image ($(grep '^step 3 ok' "$log" | sed 's/^step 3 ok: //'))"
done

# 3. The shim, sha-verified against a manifest copy, in each SHIM_IMAGES image.
NODE_DIR="$(dirname "$(dirname "$(readlink -f "$(command -v node)")")")"
cp -R "$SHIM_SRC" "$TMP/shim"
node -e '
const fs = require("fs");
const [file, sha] = process.argv.slice(1);
const m = JSON.parse(fs.readFileSync(file, "utf8"));
m.binaries["linux-x86_64"].sha256 = sha;
fs.writeFileSync(file, JSON.stringify(m, null, 2) + "\n");
' "$TMP/shim/manifest.json" "$SHA"
VERSION="$(node -p 'require(process.argv[1]).version' "$TMP/shim/package.json")"
mkdir -p "$TMP/h/.cache/dreamd-mcp/$VERSION"
cp "$TMP/b/dreamd" "$TMP/h/.cache/dreamd-mcp/$VERSION/dreamd"

for image in $SHIM_IMAGES; do
    docker pull -q "$image" >/dev/null
    log="$TMP/shim-${image//[:\/]/_}.log"
    set +e
    docker run --rm --network none \
        -v "$TMP/shim:/shim:ro" \
        -v "$TMP/h:/h:ro" -e HOME=/h \
        -v "$NODE_DIR:/node:ro" \
        "$image" /node/bin/node /shim/bin/dreamd-mcp.js version >"$log" 2>&1
    rc=$?
    set -e
    first="$(head -n 1 "$log")"
    if [[ $rc -ne 0 ]] || [[ "$first" != "dreamd "* ]]; then
        sed 's/^/    /' "$log" >&2
        fail "shim in $image (exit $rc)"
    fi
    echo "linux-compat: ok shim in $image: $first"
done

echo "linux-compat: all checks passed"
