#!/usr/bin/env bash
# Hidden setup for docs/assets/blame-forget.tape. Builds a throwaway demo
# store at /tmp/dreamd-demo/my-api and writes its memories through dreamd's
# own MCP append_node tool, then runs one deterministic dream cycle (no LLM,
# no network) so a lesson exists that cites its source event.
#
# One of the three deploy memories is bad advice written by a second agent.
# It carries high importance and pain scores, so the dream cycle promotes it
# into LESSONS.md. That is the memory the clip traces and removes.
set -euo pipefail

if command -v dreamd >/dev/null 2>&1; then D=(dreamd); else D=(npx -y dreamd-mcp); fi
export DREAMD_LOG=warn
DEMO=/tmp/dreamd-demo/my-api

rm -rf /tmp/dreamd-demo
mkdir -p "$DEMO"
cd "$DEMO"
git init -q
git -c user.name="dreamd demo" -c user.email="demo@dreamd.dev" commit -q --allow-empty -m init
"${D[@]}" init --quiet >/dev/null

call() { # id harness skill_action importance pain content
  printf '{"jsonrpc":"2.0","id":%s,"method":"tools/call","params":{"name":"append_node","arguments":{"source_harness":"%s","skill_action":"%s","importance":%s,"pain":%s,"content":"%s"}}}\n' "$@"
}
{
  printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"demo-setup","version":"0"}}}'
  printf '%s\n' '{"jsonrpc":"2.0","method":"notifications/initialized"}'
  call 2 claude-code deploy::release 6 6 "Run cargo test before every deploy."
  sleep 0.3
  call 3 cursor deploy::release 9 9 "Deploy without tests on Fridays to save time."
  sleep 0.3
  call 4 claude-code deploy::release 6 5 "Tag a release only after CI is green on main."
  sleep 0.3
  call 5 claude-code rust::axum 7 7 "axum handlers that unwrap() crash the server. Implement IntoResponse on Error and use ?."
  sleep 1
} | "${D[@]}" mcp >/dev/null 2>&1

git add -A
git -c user.name="dreamd demo" -c user.email="demo@dreamd.dev" commit -q -m "seed demo memories"
"${D[@]}" dream --no-llm >/dev/null 2>&1

# Print the bad event's id for the tape's readline macro.
jq -r 'select(.source_harness == "cursor") | .id' .agent/episodic/AGENT_LEARNINGS.jsonl
