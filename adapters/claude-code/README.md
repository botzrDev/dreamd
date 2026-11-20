# dreamd — Claude Code adapter

Quickstart for wiring `dreamd-mcp` into Claude Code.

## 1. Set up the project

```bash
cd ~/your-project
npx -y dreamd-mcp setup --harness claude
```

Scaffolds `.agent/` (via `init`) and writes the dreamd block into `.mcp.json` at the project root. It also merges a `<!-- dreamd:start -->` block that tells the next session to search memory and quote a hit. Without an `AGENTS.md` at the project root, the block goes into `CLAUDE.md`. With one, the block goes into `AGENTS.md`: once a `CLAUDE.md` exists, Claude Code loads `AGENTS.md` only through a line `@AGENTS.md` in it. A missing `CLAUDE.md`, an empty one, or one that holds only a dreamd block becomes that one line. A `CLAUDE.md` that already imports `AGENTS.md` loses only its dreamd block. Any other `CLAUDE.md` keeps its block, `AGENTS.md` is not touched, and the card says how to load `AGENTS.md`. Add `--yes` for non-interactive shells. `npx -y dreamd-mcp init` is the scaffold-only primitive if you'd rather wire MCP by hand.

## 2. MCP config

`setup --harness claude` already wrote the project-level config — skip to step 3. Wire it by hand only if you ran `--no-write-mcp` / `--harness none`, or want dreamd in every project.

**Project-level:** copy [`.mcp.json.example`](./.mcp.json.example) to `.mcp.json` at your project root.

**User-level (all projects):** register it at user scope with `claude mcp add --scope user dreamd -- npx -y dreamd-mcp` — `setup` only writes inside the project. Each project still needs its own `.agent/` (`npx -y dreamd-mcp init`).

The block in `.mcp.json.example`:

```json
{
  "mcpServers": {
    "dreamd": {
      "command": "npx",
      "args": ["-y", "dreamd-mcp"]
    }
  }
}
```

**Resumed sessions:** Claude Code reads a project's `.mcp.json` from the folder the session started in. A session that was started in another folder and is then resumed does not load this repo's `.mcp.json`, so dreamd's tools are missing from it. Start a new session from the repo root.

## 3. Start the daemon (multi-agent setups)

For a single agent on Linux or macOS, the in-process MCP server is sufficient. On Windows, start `dreamd watch` first; there is no in-process server. If **several agents write to the same project simultaneously**, start one shared daemon:

```bash
npx -y dreamd-mcp watch
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

- Authoring an adapter (MCP-first + doc-first patterns): [`../../docs/adapters.md`](../../docs/adapters.md)
- Tool naming and `skill_action` conventions: [`SKILL.md`](../../SKILL.md)
- Full tutorial: [`GUIDE.md`](../../GUIDE.md)
- Cursor adapter (same `.agent/` folder): [`../cursor/README.md`](../cursor/README.md)
