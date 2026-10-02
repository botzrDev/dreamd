# Aider + dreamd (documentation pattern)

> **Status:** Documentation pattern only — Aider does not speak MCP in this integration. Durable writes require a running dreamd daemon (`dreamd watch`). Unix only: the `curl --unix-socket` path below does not exist on Windows.

## 60-second setup

1. **Init the project store**
   ```bash
   cd ~/your-project
   npx -y dreamd-mcp init
   ```
   > First run prints a local-only privacy disclosure.
   This scaffolds `.agent/` in your project root.

2. **Start the daemon**
   ```bash
   npx -y dreamd-mcp watch
   ```
   Leave it running (it stays in the foreground; use a second terminal). An MCP session from another harness (Claude Code, Cursor, Cline) is **not** a substitute: on Linux and macOS, without `dreamd watch` those sessions run in-process and open no socket for `curl` to reach. On Windows they exit 2 until `dreamd watch` is up. Once `watch` is up, they and Aider share the one daemon and the one `.agent/` folder.

3. **Paste the CONVENTIONS template**
   Copy [`CONVENTIONS.md.template`](./CONVENTIONS.md.template) into your project's `CONVENTIONS.md` (or append it to an existing one). Aider does not pick the file up on its own: start it with `aider --read CONVENTIONS.md`, or add `read: CONVENTIONS.md` to `.aider.conf.yml`.

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
   `PROJECT` must be the absolute path of the directory you ran `init` in. Expect HTTP `201 Created` with `{"id":"evt_…","timestamp":"…","deduplicated":false}` (add `-i` to see the status line). The `id`, `timestamp` and `schema_version` in the body are placeholders: the daemon mints its own and ignores what you send, so they may also be left out.

## Caveats

- **No MCP tools inside Aider.** Aider cannot call `search_nodes` or `append_node` natively. Recall works by reading files (`/read`); append works by shelling out to `curl`.
- **`LESSONS.md` appears only after a dream cycle promotes something.** On a fresh store `.agent/semantic/LESSONS.md` does not exist yet, so the `/read` in the template has nothing to load until `dreamd dream` has consolidated a recurring cluster. The episodic log is there from the first append.
- **Never hand-edit `AGENT_LEARNINGS.jsonl`.** The daemon owns the episodic log. Always use `POST /api/v1/learn` for durable writes.
- **Daemon required for append.** Without `dreamd watch` there is no socket and the `curl` fails to connect (curl exit code 7). This is an honest limitation of the documentation pattern.

## Companion docs

- [`../../docs/adapters.md`](../../docs/adapters.md) — authoring hub (MCP-first + doc-first patterns)
- [`../../docs/http-api.md`](../../docs/http-api.md) — learn / recall / health / dream curl reference
- [`../../SPEC.md`](../../SPEC.md) — on-disk contract (`.agent/` layout, JSON schema, scoring)
