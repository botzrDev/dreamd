#!/usr/bin/env bash
# Fail if user-facing docs or .mcp.json.example hard-pin dreamd-mcp@<version>.
# Floating form is npx -y dreamd-mcp (see AGENTS.md npm-dreamd-mcp-unscoped).
#
# Excludes: docs/spikes/ (historical), CHANGELOG.md (release notes), RELEASING.md
# (maintainer release procedure with intentional @X.Y.Z pins).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

fail=0

check() {
  local label="$1"
  shift
  local matches
  matches="$(rg -n "$@" 2>/dev/null || true)"
  if [[ -n "$matches" ]]; then
    echo "ERROR: hard-pinned dreamd-mcp@ found in ${label}:"
    echo "$matches"
    fail=1
  fi
}

# MCP adapter configs must use floating "dreamd-mcp", never "dreamd-mcp@…".
check ".mcp.json.example files" \
  'dreamd-mcp@[0-9]' \
  adapters/ \
  --glob '.mcp.json*.example'

# Copy-paste npx one-liners and JSON args arrays in user-facing docs.
check "user-facing docs" \
  '(npx[^[:alnum:]_-].*dreamd-mcp@|"dreamd-mcp@|\['\''-y'\'', "dreamd-mcp@|\["-y", "dreamd-mcp@)' \
  README.md GUIDE.md AGENTS.md partner-one-pager.md \
  adapters/ docs/ packages/dreamd-mcp/README.md \
  --glob '*.md' --glob '*.json.example' --glob '*.template' \
  --glob '!docs/spikes/**' \
  --glob '!CHANGELOG.md' \
  --glob '!RELEASING.md'

if [[ "$fail" -ne 0 ]]; then
  echo
  echo "Use the floating form: npx -y dreamd-mcp (or [\"-y\", \"dreamd-mcp\"] in MCP configs)."
  echo "Pin dreamd-mcp@<version> only in RELEASING.md or docs/spikes/ — never in user-facing examples."
  exit 1
fi

echo "check-npx-floating: OK — no hard-pinned dreamd-mcp@ in user-facing docs."
