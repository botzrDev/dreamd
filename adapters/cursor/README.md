# dreamd — Cursor adapter

Quickstart for wiring `dreamd-mcp` into Cursor with the optional recall agent rule.

## 1. Set up the project

```bash
cd ~/your-project
npx -y dreamd-mcp setup --harness cursor
```

Scaffolds `.agent/` (via `init`) and writes the dreamd block into `.cursor/mcp.json` in the project. Add `--yes` for non-interactive shells. `npx -y dreamd-mcp init` is the scaffold-only primitive if you'd rather wire MCP by hand.

## 2. Start the daemon (recommended)

```bash
npx -y dreamd-mcp watch &
```

Without a daemon, MCP runs in-process. That works for single queries but can struggle on rapid consecutive `search_nodes` calls.

## 3. MCP config

`setup --harness cursor` already wrote the project-level config — skip to step 4. Wire it by hand only if you ran `--no-write-mcp` / `--harness none`, or want the global config.

**Project-level:** copy [`.mcp.json.example`](./.mcp.json.example) into `.cursor/mcp.json`.

**Global (`~/.cursor/mcp.json`):** use [`.mcp.json.global.example`](./.mcp.json.global.example) — adds `--project-root` for non-project CWD launches. `setup` only writes inside the project, so the global file is always a manual step.

Or: Cursor Settings → Tools & Integrations → add MCP server.

## 4. Agent rule (optional)

Copy [`.cursor/rules/dreamd-recall.mdc`](./.cursor/rules/dreamd-recall.mdc) to your project's `.cursor/rules/`. Cursor offers it when context matches.

## 5. Reload Cursor

Open a new agent session. Confirm `dreamd` in the MCP tools list with `append_node` and `search_nodes`.

Stderr from the MCP server should show `dreamd mcp: daemon reachable at … — serving Remote (daemon proxy)` when the daemon is running. If no daemon is running there is no default-stderr fallback line (`DREAMD_LOG=debug` logs `daemon not found … running in-process`).

## 6. Verify

Ask the agent:

> What has dreamd remembered about this codebase?

**Expect:** `search_nodes` with your task as the query; results include `score`, `content`, and per-hit `metadata.skill_action` + `metadata.source_harness` (the harness that authored each learning).

To append:

> Log a learning: we pin dependency versions in the workspace `Cargo.toml`.

**Expect:** `append_node` with `source_harness: "cursor"` (required — omitting it causes a deserialization error).

## Companion docs

- [`../../docs/adapters.md`](../../docs/adapters.md) — authoring hub (MCP-first + doc-first patterns)
- [`SKILL.md`](../../SKILL.md) — shared conventions with Claude Code
- [`GUIDE.md`](../../GUIDE.md) — full walkthrough including multi-harness
- [`../claude-code/README.md`](../claude-code/README.md) — same `.agent/` folder, different harness
