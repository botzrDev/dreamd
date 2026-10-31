# dreamd glass-box demo: trace a bad memory with blame, remove it with forget.
# Every command and every line of output on screen is live dreamd v1.2.0.
#
# Hidden setup (not shown): blame-forget-setup.sh builds a throwaway store at
# /tmp/dreamd-demo/my-api through dreamd's own MCP append_node tool and runs
# one deterministic dream cycle (no LLM, no network). The hidden lines also
# set DREAMD_LOG=warn to keep index log lines off screen, alias `dreamd` to
# `npx -y dreamd-mcp` if no dreamd binary is on PATH, and bind Ctrl+T to
# insert the bad event's id, the way you would paste it from the blame table.
# Afterwards the project is unregistered from ~/.agent/registry.toml.
#
# Render from the repo root, without uploading:
#   env -u VHS_PUBLISH vhs docs/assets/blame-forget.tape
# Needs vhs, ttyd, git, jq and Node 18+ (or dreamd on PATH).
Output "docs/assets/blame-forget.gif"
Set Shell "bash"
Set FontSize 15
Set Width 1720
Set Height 470
Set Padding 24
Set Theme "Catppuccin Mocha"
Set TypingSpeed 28ms
Set PlaybackSpeed 1.5

Hide
Type "export PS1='\[\e[1;35m\]my-api\[\e[0m\] $ ' DREAMD_LOG=warn" Enter
Type "command -v dreamd >/dev/null || alias dreamd='npx -y dreamd-mcp'" Enter
Type `EVT=$(bash docs/assets/blame-forget-setup.sh) && bind "\"\C-t\": \"$EVT\"" && cd /tmp/dreamd-demo/my-api` Enter
Wait+Line@120s /my-api \$\s*$/
Type "clear" Enter
Sleep 500ms
Show

Type "# The agent keeps saying to skip tests. Which memory says so?" Enter
Sleep 400ms
Type `dreamd blame "deploy without tests" -k 2` Enter
Wait+Line /my-api \$\s*$/
Sleep 3s

Type "# One bad event, and the lesson built from it. Remove the event." Enter
Sleep 400ms
Type "dreamd forget "
Ctrl+T
Type " --proof forget-receipt.json"
Sleep 300ms
Enter
Wait+Line /my-api \$\s*$/
Sleep 1500ms

Type "jq '{event_id, removed, lesson}' forget-receipt.json" Enter
Wait+Line /my-api \$\s*$/
Sleep 2s

Type "# Ask again." Enter
Sleep 300ms
Type `dreamd blame "deploy without tests" -k 2` Enter
Wait+Line /my-api \$\s*$/
Sleep 4s

Hide
Type "dreamd init --uninstall-project >/dev/null 2>&1; clear" Enter
Sleep 1s
