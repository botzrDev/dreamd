#!/usr/bin/env bash
# Regenerate packages/dreamd-mcp/manifest.json sha256 entries from release tarballs.
#
# The shim verifies the *extracted dreamd binary* (not the .tar.gz). Each
# tarball must contain a single `dreamd` (or `dreamd.exe`) at its root, matching
# .github/workflows/release.yml packaging.
#
# Usage:
#   scripts/update-mcp-manifest.sh <version> <dist-dir>
#
# Example (after release workflow artifacts are in dist/):
#   scripts/update-mcp-manifest.sh 0.1.0-rc.2 dist
set -euo pipefail

VERSION="${1:?usage: $0 <version> <dist-dir>}"
DIST_DIR="${2:?usage: $0 <version> <dist-dir>}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
MANIFEST="$ROOT/packages/dreamd-mcp/manifest.json"

PLATFORMS=(linux-x86_64 darwin-x86_64 darwin-aarch64)

find_tarball() {
  local platform="$1"
  local candidate
  for candidate in \
    "$DIST_DIR/${platform}.tar.gz" \
    "$DIST_DIR/${platform}.tar.gz/${platform}.tar.gz"; do
    if [[ -f "$candidate" ]]; then
      printf '%s' "$candidate"
      return 0
    fi
  done
  echo "error: missing tarball for ${platform} under ${DIST_DIR}" >&2
  return 1
}

sha256_binary_in_tarball() {
  local archive="$1"
  local work bin
  work="$(mktemp -d)"
  trap 'rm -rf "$work"' RETURN
  tar -xzf "$archive" -C "$work"
  bin="$work/dreamd"
  if [[ ! -f "$bin" ]]; then
    echo "error: ${archive} did not contain dreamd at archive root" >&2
    return 1
  fi
  sha256sum "$bin" | awk '{print $1}'
}

TMP_JSON="$(mktemp)"
{
  echo '{'
  echo "  \"version\": \"${VERSION}\","
  echo '  "binaries": {'
  first=1
  for platform in "${PLATFORMS[@]}"; do
    archive="$(find_tarball "$platform")"
    hash="$(sha256_binary_in_tarball "$archive")"
    if [[ "$first" -ne 1 ]]; then
      echo ','
    fi
    first=0
    echo "    \"${platform}\": {"
    echo "      \"sha256\": \"${hash}\""
    echo -n '    }'
  done
  echo
  echo '  }'
  echo '}'
} >"$TMP_JSON"

python3 -m json.tool "$TMP_JSON" >"$MANIFEST"
rm -f "$TMP_JSON"
echo "wrote $MANIFEST"
