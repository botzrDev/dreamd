# dreamd — Claude Code adapter

## What this does

Wires `@dataprime1/dreamd-mcp` as a stdio MCP server that Claude Code spawns automatically. No background daemon required.

## Project-level config (recommended)

For per-project memory, paste the contents of `.mcp.json.example` into `.mcp.json` at your project root. Claude Code picks this up automatically on next session start.

## User-level config

For memory shared across all projects, paste the `mcpServers` block into `~/.claude/settings.json`. Create the file if it doesn't exist; merge into the existing `mcpServers` object if it does.

## Reload

Restart Claude Code or run `/mcp` to confirm the `dreamd` server appears as connected.

## Companion skill

Usage conventions for `append_node`, `search_nodes`, `skill_action` naming, required fields, and session activation are in `.claude/skills/dreamd-recall/SKILL.md`.
