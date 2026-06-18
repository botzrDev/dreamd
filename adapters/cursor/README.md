# dreamd — Cursor adapter

## What this does

Wires `dreamd-mcp` as a stdio MCP server that Cursor spawns automatically, and activates the `dreamd-recall` agent rule when you work in a project with a `.agent/` folder.

## Recommended setup: run the daemon first

Start `dreamd watch` in your project before opening Cursor. The daemon handles back-to-back searches correctly and keeps the index fresh. Without it, the MCP server runs in Phase 1 in-process mode, which works for single queries but can fail on rapid consecutive `search_nodes` calls.

```bash
cd ~/your-project
dreamd watch &
```

Then open Cursor. The MCP server detects the running daemon automatically and routes through it.

## MCP config

**Project-level (recommended):** paste the contents of `.mcp.json.example` into your project's `.cursor/mcp.json` (or merge the `mcpServers` block into an existing one). Cursor picks this up automatically on next session start.

**Global config (`~/.cursor/mcp.json`):** add `--project-root` so the MCP server knows which project's `.agent/` to use regardless of the CWD Cursor sets at launch:

```json
{
  "mcpServers": {
    "dreamd": {
      "command": "npx",
      "args": ["-y", "dreamd-mcp", "--project-root", "/absolute/path/to/your-project"]
    }
  }
}
```

Alternatively, add it via Cursor Settings → Tools & Integrations.

## Agent rule

Copy `.cursor/rules/dreamd-recall.mdc` into your project's `.cursor/rules/` directory. Cursor will offer it to the agent automatically when context matches.

## Reload

Restart Cursor or open a new agent session to confirm the `dreamd` server appears in the MCP tools list with `append_node` and `search_nodes` tools visible.

## Companion skill (Claude Code)

If you also use Claude Code on this project, the equivalent skill is at `.claude/skills/dreamd-recall/SKILL.md`. Both adapters write to the same `.agent/` folder — learnings are shared across harnesses.
