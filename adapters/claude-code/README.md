# dreamd — Claude Code adapter

Quickstart for wiring `dreamd-mcp` into Claude Code.

## 1. Init the project store

```bash
cd ~/your-project
npx dreamd-mcp@0.1.0-rc.2 init
```

## 2. MCP config

**Project-level (recommended):** copy [`.mcp.json.example`](./.mcp.json.example) to `.mcp.json` at your project root.

**User-level (all projects):** merge the `mcpServers` block into `~/.claude/settings.json`.

```json
{
  "mcpServers": {
    "dreamd": {
      "command": "npx",
      "args": ["dreamd-mcp@0.1.0-rc.2"]
    }
  }
}
```

## 3. Start the daemon (multi-agent setups)

For a single agent, the in-process MCP server is sufficient. If **several agents write to the same project simultaneously**, start one shared daemon:

```bash
dreamd watch
# or: npx dreamd-mcp@0.1.0-rc.2 watch
```

## 4. Reload Claude Code

Restart Claude Code or run `/mcp`. Confirm `dreamd` appears connected with `search_nodes` and `append_node`.

## 5. Verify

In Claude Code, ask:

> Search dreamd memory for anything about error handling in this project.

**Expect:** The agent calls `search_nodes` and returns ranked results (or an empty list on a fresh store).

To log a learning:

> Remember that we use `thiserror` for library errors and `anyhow` only in binaries.

**Expect:** `append_node` returns `{"id":"evt_…","timestamp":"…","deduplicated":false}` and a new line in `.agent/episodic/AGENT_LEARNINGS.jsonl`.

## Companion docs

- Tool naming and `skill_action` conventions: [`SKILL.md`](../../SKILL.md)
- Full tutorial: [`GUIDE.md`](../../GUIDE.md)
- Cursor adapter (same `.agent/` folder): [`../cursor/README.md`](../cursor/README.md)
