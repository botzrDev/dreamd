# dreamd — Cursor adapter

## What this does

Wires `dreamd-mcp` as a stdio MCP server that Cursor spawns automatically, and activates the `dreamd-recall` agent rule when you work in a project with a `.agent/` folder.

## Recommended setup: run the daemon first

Start `dreamd watch` in your project before opening Cursor. The daemon handles back-to-back searches correctly and keeps the index fresh. Without it, the MCP server runs in Phase 1 in-process mode, which works for single queries but can fail on rapid consecutive `search_nodes` calls.

If you point **several agents at the same project simultaneously**, start one shared daemon per machine so every agent routes through a single serialized writer:

```bash
cd ~/your-project
dreamd watch &
# or: npx dreamd-mcp@0.1.0-rc.1 watch &
```

Then open Cursor. The MCP server detects the running daemon automatically and routes through it.

## MCP config

**Project-level (recommended):** paste the contents of `.mcp.json.example` into your project's `.cursor/mcp.json` (or merge the `mcpServers` block into an existing one). Cursor picks this up automatically on next session start.

**Global config (`~/.cursor/mcp.json`):** use `.mcp.json.global.example` — it adds `--project-root` so the MCP server knows which project's `.agent/` to use regardless of the CWD Cursor sets at launch.

Alternatively, add it via Cursor Settings → Tools & Integrations.

## Agent rule

Copy `.cursor/rules/dreamd-recall.mdc` into your project's `.cursor/rules/` directory. Cursor will offer it to the agent automatically when context matches.

## Reload

Restart Cursor or open a new agent session to confirm the `dreamd` server appears in the MCP tools list with `append_node` and `search_nodes` tools visible.

## Companion skill

Usage conventions for `append_node`, `search_nodes`, `skill_action` naming, and session activation are in [`../../SKILL.md`](../../SKILL.md). Both adapters write to the same `.agent/` folder — learnings are shared across harnesses.
