# dreamd — Claude Code adapter

## What this does

Wires `dreamd-mcp` as a stdio MCP server that Claude Code spawns automatically.

## Project-level config (recommended)

For per-project memory, paste the contents of `.mcp.json.example` into `.mcp.json` at your project root. Claude Code picks this up automatically on next session start.

## User-level config

For memory shared across all projects, paste the `mcpServers` block into `~/.claude/settings.json`. Create the file if it doesn't exist; merge into the existing `mcpServers` object if it does.

## Multi-agent setups

For a single agent — or several agents used one at a time — the standalone MCP server is safe. If you point **several agents at the same project simultaneously**, start one shared daemon per machine:

```bash
dreamd watch
# or: npx dreamd-mcp@0.1.0-rc.1 watch
```

## Reload

Restart Claude Code or run `/mcp` to confirm the `dreamd` server appears as connected.

## Companion skill

Usage conventions for `append_node`, `search_nodes`, `skill_action` naming, required fields, and session activation are in [`../../SKILL.md`](../../SKILL.md).
