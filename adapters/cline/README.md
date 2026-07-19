# dreamd — Cline adapter

Quickstart for wiring `dreamd-mcp` into [Cline](https://github.com/cline/cline) (VS Code extension).

> **Status:** Round-trip works in both Phase 1 (in-process) and Phase 2 (daemon) as of v0.1.0-rc.3.

## 1. Init the project store

```bash
cd ~/your-project
dreamd init
# or: npx dreamd-mcp@0.1.0-rc.3 init
```

Cline must open a project that already has `.agent/`. Without it, `append_node` errors with `coordinator unavailable: no agent root found`.

## 2. MCP config

Copy [`.mcp.json.example`](./.mcp.json.example) into Cline's MCP settings file.

| OS | Path |
|----|------|
| **Linux** | `~/.config/Code/User/globalStorage/saoudrizwan.claude-dev/settings/cline_mcp_settings.json` |
| **macOS** | `~/Library/Application Support/Code/User/globalStorage/saoudrizwan.claude-dev/settings/cline_mcp_settings.json` |
| **Windows** | `%APPDATA%\Code\User\globalStorage\saoudrizwan.claude-dev\settings\cline_mcp_settings.json` |

For VS Code Insiders, replace `Code` with `Code - Insiders`. Open via Cline sidebar → MCP Servers → **Configure MCP Servers**.

> **npm note (2026-07-19):** `dreamd-mcp@0.1.0-rc.3` is live on npm — `npx dreamd-mcp` resolves directly. The local-binary config below is for local development only.

**Published npm path:**

```json
{
  "mcpServers": {
    "dreamd": {
      "command": "npx",
      "args": ["-y", "dreamd-mcp@0.1.0-rc.3"],
      "disabled": false,
      "autoApprove": []
    }
  }
}
```

**Local binary (development):**

```json
{
  "mcpServers": {
    "dreamd": {
      "command": "/absolute/path/to/target/release/dreamd",
      "args": ["mcp"],
      "disabled": false,
      "autoApprove": []
    }
  }
}
```

## 3. Daemon (optional)

Phase 1 (no daemon) is enough to validate the append → search round-trip. For multi-agent or high-frequency recall, start:

```bash
dreamd watch
```

Check Cline's output channel for:

| Stderr line | Meaning |
|---|---|
| `Phase 2 (Remote backend)` | Daemon connected |
| `Phase 1 fallback` | In-process server |

## 4. Reload Cline

Restart the Cline extension or VS Code. Open the MCP Servers panel and confirm `search_nodes` and `append_node` are listed.

## 5. Verify

Ask Cline:

> Use dreamd to search memory for anything about testing in this repo.

**Expect:** `search_nodes` call with JSON results array.

Then:

> Remember: we run `cargo test --workspace` before every PR.

**Expect:** `append_node` with `source_harness: "cline"` returning `evt_…` id.

## Tool parameters

| Tool | Required params |
|---|---|
| `search_nodes` | `query` |
| `append_node` | `content`, `source_harness` (`"cline"`), `skill_action` |

See [`SKILL.md`](../../SKILL.md) for `skill_action` naming (`rust::error_handling`, not dotted paths).

## Companion docs

- [`../../docs/adapters.md`](../../docs/adapters.md) — authoring hub (MCP-first + doc-first patterns)
- [`GUIDE.md`](../../GUIDE.md) — section 6 (multi-harness)
