# Aider + dreamd (documentation pattern)

> **Status:** Documentation pattern only — Aider does not speak MCP in this integration. Durable writes require a running dreamd daemon (`dreamd watch` / equivalent).

## 60-second setup

1. **Init the project store**
   ```bash
   cd ~/your-project
   npx dreamd-mcp init
   ```
   This scaffolds `.agent/` in your project root.

2. **Start the daemon**
   ```bash
   dreamd watch
   ```
   Or keep another harness's MCP session running (Claude Code, Cursor, Cline) — they all share the same daemon and `.agent/` folder.

3. **Paste the CONVENTIONS template**
   Copy [`CONVENTIONS.md.template`](./CONVENTIONS.md.template) into your project's `CONVENTIONS.md` (or append it to an existing one). Aider reads `CONVENTIONS.md` at session start.

4. **Smoke-test the append path**
   ```bash
   PROJECT=/home/you/your-project
   curl --unix-socket ~/.agent/dreamd.sock \
     -X POST \
     -H "X-Agent-Root: $PROJECT" \
     -H "Content-Type: application/json" \
     -d '{
       "schema_version": "1.0.0",
       "id": "evt_01ARZ3NDEKTSV4RRFFQ69G5FAV",
       "timestamp": "2026-07-19T12:00:00Z",
       "pain": 1.0,
       "importance": 1.0,
       "skill_action": "aider::smoke_test",
       "source_harness": "aider",
       "content": "Aider dreamd integration smoke test."
     }' \
     http://localhost/api/v1/learn
   ```
   Expect HTTP `201 Created`.

## Caveats

- **No MCP tools inside Aider.** Aider cannot call `search_nodes` or `append_node` natively. Recall works by reading files (`/read`); append works by shelling out to `curl`.
- **Never hand-edit `AGENT_LEARNINGS.jsonl`.** The daemon owns the episodic log. Always use `POST /api/v1/learn` for durable writes.
- **Daemon required for append.** Without `dreamd watch` (or a live MCP session from another harness), the `curl` fails with `connection refused`. This is an honest limitation of the documentation pattern.

## Companion docs

- [`../../docs/adapters.md`](../../docs/adapters.md) — authoring hub (MCP-first + doc-first patterns)
- [`../../docs/http-api.md`](../../docs/http-api.md) — learn / recall / health / dream curl reference
- [`../../SPEC.md`](../../SPEC.md) — on-disk contract (`.agent/` layout, JSON schema, scoring)
