# dreamd README demo: real Claude Code write, real OpenCode recall.
# The hidden aliases only strip the recorder's personal config, pre-approve
# the dreamd MCP tool and pick the model; every output on screen is live.
# Playback is sped up 1.5x.
#
# Setup before recording (not shown): a fresh `git init` project at
# /tmp/dreamd-demo/my-api, `npx -y dreamd-mcp setup --yes --harness claude`,
# an opencode.json wiring the same `npx -y dreamd-mcp` server, and
# `npx -y dreamd-mcp watch` running as the shared daemon.
# Render: vhs docs/assets/demo.tape (then crop the empty bottom).
Output "dreamd-demo.gif"
Set Shell "bash"
Set FontSize 16
Set Width 1400
Set Height 720
Set Padding 24
Set Theme "Catppuccin Mocha"
Set TypingSpeed 30ms
Set PlaybackSpeed 1.5

Hide
Type "cd /tmp/dreamd-demo/my-api && export PS1='\[\e[1;35m\]my-api\[\e[0m\] $ '" Enter
Type "alias claude='claude --setting-sources project --mcp-config .mcp.json --strict-mcp-config --allowedTools mcp__dreamd__append_node --model claude-sonnet-5-5'" Enter
Type `opencode() { XDG_CONFIG_HOME=/tmp/dreamd-demo/xdg command opencode "$1" --standalone -m deepseek/deepseek-flash "${@:2}"; }` Enter
Type "clear" Enter
Sleep 500ms
Show

Type "# Monday. Claude Code learns something the hard way." Enter
Sleep 600ms
Type `claude -p "axum handlers that unwrap() crash the server. Fix: IntoResponse on our Error type, use ?. Save that to dreamd memory. Reply in one line."` Enter
Wait+Line@120s /my-api \$\s*$/
Sleep 1500ms

Type "# The memory is a plain file in the repo." Enter
Sleep 600ms
Type "jq -c '{source_harness, skill_action}' .agent/episodic/AGENT_LEARNINGS.jsonl" Enter
Sleep 2500ms

Type "# Friday. A different agent, a fresh session." Enter
Sleep 600ms
Type `opencode run "Search dreamd memory for: axum handler crash. Give me the fix in two sentences, using only what memory says."` Enter
Wait+Line@120s /my-api \$\s*$/
Sleep 5s
