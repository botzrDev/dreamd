# dreamd — Cursor adapter

Quickstart for wiring `dreamd-mcp` into Cursor with the optional recall agent rule.

## 1. Init the project store

```bash
cd ~/your-project
npx dreamd-mcp@0.1.0-rc.2 init
```

## 2. Start the daemon (recommended)

```bash
dreamd watch &
# or: npx dreamd-mcp@0.1.0-rc.2 watch &
```

Without a daemon, MCP runs in-process (Phase 1). That works for single queries but can struggle on rapid consecutive `search_nodes` calls.

## 3. MCP config

**Project-level (recommended):** copy [`.mcp.json.example`](./.mcp.json.example) into `.cursor/mcp.json`.

**Global (`~/.cursor/mcp.json`):** use [`.mcp.json.global.example`](./.mcp.json.global.example) — adds `--project-root` for non-project CWD launches.

Or: Cursor Settings → Tools & Integrations → add MCP server.

## 4. Agent rule (optional)

Copy [`.cursor/rules/dreamd-recall.mdc`](./.cursor/rules/dreamd-recall.mdc) to your project's `.cursor/rules/`. Cursor offers it when context matches.

## 5. Reload Cursor

Open a new agent session. Confirm `dreamd` in the MCP tools list with `append_node` and `search_nodes`.

Stderr from the MCP server should show `Phase 2 (Remote backend)` when the daemon is running.

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
