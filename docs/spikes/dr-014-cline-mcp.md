# DR-014: Cline-via-MCP spike (WEG-39)

*Status (2026-07-19): npm `latest` is `0.1.0-rc.3`; historical tables below unchanged.*

**Date:** 2026-07-07  
**Commit:** `5684105` (`main`)  
**Verifier:** Cursor agent (automated MCP layer) + pending human Cline UI pass  
**Linear:** [WEG-39](https://linear.app/wegetit/issue/WEG-39)

## Question

Does Cline actually surface dreamd's locked MCP tool pair (`search_nodes`, `append_node`) and render tool results without JSON errors — or is "Claude Code + Cursor + Cline" a config-file-shaped assumption?

## Verdict

**Recommendation: PROCEED with Cline in the v0.1 launch trio** — at the MCP protocol layer, Cline is indistinguishable from Cursor/Claude Code. No Cline-specific server changes are required.

**Two preconditions before claiming the trio is fully verified end-to-end:**

1. **Publish `dreamd-mcp@0.1.0-rc.2` to npm** — GitHub release `v0.1.0-rc.2` exists (2026-07-06) but npm `latest` is still `0.1.0-rc.1` as of this spike. Documented Cline config (`npx dreamd-mcp@0.1.0-rc.2`) fails with `ETARGET`.
2. **Human Cline UI pass (~15 min)** — no Cline/VS Code install was available on the agent machine (WSL2, no `code` CLI, no `cline_mcp_settings.json` on disk). Protocol-level verification is complete; UI surfacing and conversation rendering are not.

Do **not** substitute OpenCode or drop Cline from the trio based on this spike.

---

## What was tested (automated)

All commands run on Linux/WSL2 against `target/debug/dreamd` at `5684105`.

### 1. Alpha plumbing suite — PASS

```text
scripts/alpha/alpha-suite.sh → 7 passed, 0 failed
```

Cross-harness recall (claude-code ↔ cursor) on Phase 1 and Phase 2 paths is green. Cline uses the same MCP stdio surface; no Cline-specific code path exists in dreamd.

### 2. Cline harness MCP round-trip — PASS

Simulated Cline via `mcp_driver.py` with `source_harness: "cline"`:

```bash
# append_node → search_nodes on fresh .agent/ store
append: evt_01KWYJQBQA339PJXD8MPZXE990
search: 1 hit, source_harness=cline, skill_action=rust::testing::cline_spike
```

Both calls returned `result.isError: false`.

### 3. `tools/list` — PASS

Exactly two tools, names locked per AGENTS.md:

| Tool | Description (truncated) |
|------|-------------------------|
| `search_nodes` | Search episodic memory for past learnings — use when: recall… |
| `append_node` | Append a new learning to episodic memory — use when: note that… |

### 4. Tool result JSON shape — PASS (MCP wire)

Cline (like all MCP clients) receives `CallToolResult` with `content[].type = "text"` and **parseable JSON** in `content[].text`:

**`append_node` success:**

```json
{
  "id": "evt_01KWYJQBQA339PJXD8MPZXE990",
  "timestamp": "2026-07-07T15:21:40.842792296+00:00",
  "deduplicated": false
}
```

**`search_nodes` success:**

```json
{
  "results": [
    {
      "score": 0.818,
      "bm25": 0.863,
      "salience": 0.948,
      "source": "episodic",
      "content": "…",
      "metadata": {
        "timestamp_sec": 1783437700,
        "pain": 7.0,
        "importance": 8.0,
        "recurrence": 1,
        "skill_action": "rust::testing::cline_spike",
        "source_harness": "cline"
      }
    }
  ]
}
```

No nested `isError` inside the text payload on success (the MCP envelope carries `isError: false`). This matches the shape Cursor and Claude Code already consume; no "malformed JSON" risk at the server.

---

## What was NOT tested (human-only)

| Check | Status | Owner |
|-------|--------|-------|
| `cline_mcp_settings.json` install | Not run | Austin |
| Tools appear in Cline MCP Servers panel | Not run | Austin |
| Cline agent invokes tools from conversation | Not run | Austin |
| Tool results render in Cline chat (no UI JSON error) | Not run | Austin |
| Phase 2 stderr (`Phase 2 (Remote backend)`) in Cline output channel | Not run | Austin |

---

## Cline MCP config

Cline uses its **own** settings file — separate from VS Code's `.vscode/mcp.json` and from Cursor's `.cursor/mcp.json`.

### VS Code extension (launch path)

| OS | Path |
|----|------|
| **Linux** | `~/.config/Code/User/globalStorage/saoudrizwan.claude-dev/settings/cline_mcp_settings.json` |
| **macOS** | `~/Library/Application Support/Code/User/globalStorage/saoudrizwan.claude-dev/settings/cline_mcp_settings.json` |
| **Windows** | `%APPDATA%\Code\User\globalStorage\saoudrizwan.claude-dev\settings\cline_mcp_settings.json` |

For VS Code Insiders, replace `Code` with `Code - Insiders`.

Open via Cline sidebar → MCP Servers (stacked-server icon) → **Configure MCP Servers**.

### Published npm config (blocked until rc.2 on npm)

```json
{
  "mcpServers": {
    "dreamd": {
      "command": "npx",
      "args": ["-y", "dreamd-mcp@0.1.0-rc.2"],
      "disabled": false,
      "autoApprove": []
    }
  }
}
```

Stub: [`adapters/cline/.mcp.json.example`](../../adapters/cline/.mcp.json.example)

### Workaround for spike / dev (local binary)

Until npm rc.2 ships, pin the project root explicitly:

```json
{
  "mcpServers": {
    "dreamd": {
      "command": "/absolute/path/to/dreamd",
      "args": ["mcp", "--project-root", "/absolute/path/to/your-project"],
      "disabled": false,
      "autoApprove": []
    }
  }
}
```

Prerequisites: `cd your-project && dreamd init` (project must have `.agent/`).

### Cline CLI (out of v0.1 launch scope)

The Cline **CLI** reads `~/.cline/data/settings/cline_mcp_settings.json` — a different path from the VS Code extension. v0.1 targets the VS Code extension path only.

---

## npm install findings

| Command | Result |
|---------|--------|
| `npm view dreamd-mcp versions` | `["0.1.0-rc.1"]` only — **no rc.2** |
| `npx dreamd-mcp@0.1.0-rc.2 version` | `ETARGET: No matching version` |
| `npx dreamd-mcp@0.1.0-rc.1 version` | Routes to `dreamd mcp version` → clap error (rc.1 shim lacks top-level `--version` handling; fixed in rc.2 shim) |
| `npx dreamd-mcp@0.1.0-rc.1` (bare) | Downloads rc.1 binary; MCP stdio server starts |

**Gap:** [WEG-292](https://linear.app/wegetit/issue/WEG-292) is Done (GitHub `v0.1.0-rc.2` published 2026-07-06) but npm publish step appears incomplete. This blocks the documented Cline quickstart and WEG-265 gate item 2 (`npx dreamd-mcp` clean install).

---

## Gotchas

1. **Project must have `.agent/`** — `append_node` without a registered store returns `coordinator unavailable: no agent root found`. Run `dreamd init` in the project Cline opens.
2. **`source_harness: "cline"`** — required on every `append_node`; reserved value per SPEC.md.
3. **`skill_action` format** — `rust::error_handling`, not dotted paths. Invalid values are rejected at the MCP boundary.
4. **Cline config is not Cursor config** — adapters are per-harness; copy from `adapters/cline/`, not `adapters/cursor/`.
5. **npm rc.2 missing** — use local binary workaround until publish lands.

---

## Human verification runbook (~15 min)

For Austin on a machine with Cline + VS Code installed:

1. `cd ~/path/to/dreamd && dreamd init` (or any project with `.agent/`)
2. Merge config into `cline_mcp_settings.json` (local-binary workaround above if npm rc.2 unavailable)
3. Reload VS Code window (Cmd/Ctrl+Shift+P → Developer: Reload Window)
4. Cline sidebar → MCP Servers → confirm `dreamd` connected, tools `search_nodes` + `append_node` listed
5. Prompt: *"Use dreamd to search memory for anything about testing in this repo."*  
   **Expect:** `search_nodes` call, JSON results array (possibly empty)
6. Prompt: *"Remember: we run cargo test --workspace before every PR."*  
   **Expect:** `append_node` with `source_harness: "cline"`, returns `evt_…` id
7. Re-run step 5 — **Expect:** the lesson from step 6 ranks in results
8. Check Cline output channel for `Phase 1 fallback` or `Phase 2 (Remote backend)` (optional: `dreamd watch` first)

Record pass/fail in this doc's Human verification table and close WEG-39 UI gap.

---

## Unblocks

| Ticket | Effect |
|--------|--------|
| **WEG-94** | Private build review can cite trio with protocol evidence; npm + UI caveats noted |
| **WEG-129** | Cline adapter formalization (example + smoke test) — proceed if human UI pass is green |
| **WEG-266 / WEG-95** | Launch trio claim no longer paper-only at MCP layer |

---

## References

- [`adapters/cline/README.md`](../../adapters/cline/README.md)
- [`scripts/alpha/mcp_driver.py`](../../scripts/alpha/mcp_driver.py) — harness simulation
- [Cline MCP docs (IONOS)](https://docs.ionos.com/cloud/ai/mcp-server/connect-to-an-ai-client/cline) — config path confirmation
- [Cline CLI config path issue #11671](https://github.com/cline/cline/issues/11671) — CLI vs extension paths
