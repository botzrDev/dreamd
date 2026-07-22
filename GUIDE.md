# dreamd — guided walkthrough

A linear tutorial (~20 minutes) that walks through every major workflow: install, first learning, recall, dream cycle, daemon mode, multi-harness, and crash recovery.

For reference docs see [docs/README.md](./docs/README.md). For the on-disk contract see [SPEC.md](./SPEC.md).

---

## 1. Setup

### Install

Pick one path:

```bash
# npm (no Rust required)
npx -y dreamd-mcp --version

# cargo (from a clone)
cargo install --path crates/dreamd-cli
dreamd version
```

### Init a project store

```bash
cd ~/your-project    # must contain .git/, Cargo.toml, package.json, or pyproject.toml
dreamd init
```

**What just happened:** dreamd scaffolded `<project>/.agent/` (episodic, semantic, personal, working), wrote a commented config template to `.agent/.dreamd/config.toml`, registered the project in `~/.agent/registry.toml`, and appended `/.agent/.dreamd/` to `.gitignore`.

Verify:

```bash
ls -la .agent/
dreamd doctor
```

`doctor` prints dream-cycle mode, last cycle status, and index health.

---

## 2. First learning

Append a learning via the HTTP API (or let your agent call `append_node` over MCP — same shape).

With the daemon running (section 5), or in-process via `dreamd mcp`:

```bash
PROJECT=$(pwd)

curl --unix-socket ~/.agent/dreamd.sock \
  -X POST \
  -H "X-Agent-Root: $PROJECT" \
  -H "Content-Type: application/json" \
  -d '{
    "schema_version": "1.0.0",
    "id": "evt_01ARZ3NDEKTSV4RRFFQ69G5FAV",
    "timestamp": "2026-06-23T12:00:00Z",
    "pain": 7.0,
    "importance": 8.0,
    "skill_action": "rust::error_handling::axum_rejection",
    "source_harness": "cursor",
    "content": "Axum route handlers must return impl IntoResponse; unwrapping panics."
  }' \
  http://localhost/api/v1/learn
```

**What just happened:** The coordinator minted a real `evt_…` ID, redacted secrets (if enabled), appended one JSONL line to `.agent/episodic/AGENT_LEARNINGS.jsonl`, and queued a Tantivy index update.

Inspect the durable record:

```bash
tail -1 .agent/episodic/AGENT_LEARNINGS.jsonl | jq .
```

Note the daemon-overwritten `id` and `schema_version: "1.0.0"`.

---

## 3. Search (recall)

```bash
curl --unix-socket ~/.agent/dreamd.sock \
  -H "X-Agent-Root: $PROJECT" \
  "http://localhost/api/v1/recall?q=axum+unwrap&k=5" | jq .
```

**What just happened:** Tantivy ran BM25 over `content`, then the salience collector reweighted each hit:

```
salience = exp(-age_days/14) × (pain/10) × (importance/10) × (1 + ln(1 + recurrence))
final_score = bm25 × salience
```

Each result includes `score`, `bm25`, `salience`, and `metadata` (timestamp, pain, importance, recurrence, plus `skill_action` and `source_harness` — each hit's cluster key and authoring harness, so recall is cross-harness-attributable).

> **Timing:** A learning appended seconds ago may not appear until the next index commit (5 s cadence in v0.1). Wait briefly and retry if results are empty.

---

## 4. The dream cycle

Seed a few related learnings (or use the MCP `append_node` tool three times with the same `skill_action` prefix). Then consolidate:

```bash
dreamd dream
```

**What just happened:**

1. WAL written (`dream_in_progress.wal`) before any destructive step
2. Episodic events clustered by `skill_action`
3. Clusters with ≥3 recurrences in the window promote to `.agent/semantic/LESSONS.md`
4. Promoted exemplar events get `pinned: true` in JSONL
5. Decay pruner archives stale unpinned events to `.agent/.dreamd/snapshots/`
6. `recurrence_counts.json` updated; WAL committed

Inspect outputs:

```bash
cat .agent/semantic/LESSONS.md
cat .agent/semantic/recurrence_counts.json | jq .
grep '"pinned": true' .agent/episodic/AGENT_LEARNINGS.jsonl
```

Re-run is idempotent on identical input (byte-identical `LESSONS.md`).

---

## 5. Daemon mode

Terminal 1 — start the shared writer:

```bash
cd ~/your-project
dreamd watch
```

**What just happened:** The daemon bound `~/.agent/dreamd.sock` (`0600`), booted the coordinator + pinned Tantivy handle for this project, and blocks until SIGINT/SIGTERM.

Terminal 2 — confirm the socket and hit the API:

```bash
ls -l ~/.agent/dreamd.sock
curl --unix-socket ~/.agent/dreamd.sock \
  -H "X-Agent-Root: $(pwd)" \
  "http://localhost/api/v1/recall?q=axum&k=3"
```

Terminal 3 — MCP bridges to the daemon automatically:

```bash
npx -y dreamd-mcp
```

Stderr should show `Phase 2 (Remote backend)` when the daemon is reachable.

---

## 6. Multi-harness

Point two agents at the same project:

| Harness | Config |
|---|---|
| Claude Code | [adapters/claude-code/README.md](./adapters/claude-code/README.md) |
| Cursor | [adapters/cursor/README.md](./adapters/cursor/README.md) |
| Cline | [adapters/cline/README.md](./adapters/cline/README.md) |

With `dreamd watch` running, both harnesses share one coordinator. A learning appended in Cursor is recallable in Claude Code after the commit cadence.

Try it:

1. In Cursor: ask the agent to `append_node` a lesson with `source_harness: "cursor"`.
2. In Claude Code: ask it to `search_nodes` for the same topic.
3. Confirm the JSONL line shows both harnesses' provenance over time.

See also the pre-built fixture: [examples/multi-harness/](./examples/multi-harness/).

---

## 7. Crash recovery

Simulate a mid-cycle kill:

```bash
# Start a cycle, then SIGKILL the daemon mid-write (or use the fixture)
kill -9 $(pgrep -f 'dreamd watch')
```

Or study the static fixture: [examples/crash-recovery/](./examples/crash-recovery/).

Restart:

```bash
dreamd watch
```

**What just happened:** On startup, if `dream_in_progress.wal` exists, recovery runs before serving traffic — temp files from incomplete intents are cleaned up, WAL deleted, `state.json` marked `failed`. The store is never left half-promoted.

Verify:

```bash
dreamd doctor
ls .agent/.dreamd/dream_in_progress.wal   # should be absent after recovery
```

---

## Next steps

| Topic | Doc |
|---|---|
| HTTP API details | [docs/http-api.md](./docs/http-api.md) |
| Configuration | [docs/configuration.md](./docs/configuration.md) |
| Architecture | [ARCHITECTURE.md](./ARCHITECTURE.md) |
| Troubleshooting | [docs/troubleshooting.md](./docs/troubleshooting.md) |
| Runnable fixtures | [examples/README.md](./examples/README.md) |
