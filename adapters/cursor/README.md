# dreamd — Cursor adapter

## What this does

Wires `dreamd-mcp` as a stdio MCP server that Cursor spawns automatically, and activates the `dreamd-recall` agent rule when you work in a project with a `.agent/` folder. No background daemon required.

## MCP config

Paste the contents of `.mcp.json.example` into your project's `.mcp.json` (or merge the `mcpServers` block into an existing one). Cursor picks this up automatically on next session start. Alternatively, add it via Cursor Settings → Tools & Integrations.

## Agent rule

Copy `.cursor/rules/dreamd-recall.mdc` into your project's `.cursor/rules/` directory. Cursor will offer it to the agent automatically when context matches.

## Reload

Restart Cursor or open a new agent session to confirm the `dreamd` server appears in the MCP tools list.

## Companion skill (Claude Code)

If you also use Claude Code on this project, the equivalent skill is at `.claude/skills/dreamd-recall/SKILL.md`. Both adapters write to the same `.agent/` folder — learnings are shared across harnesses.
