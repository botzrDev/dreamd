# dreamd — guided walkthrough

A linear tutorial (~20 minutes) that walks through every major workflow: install, first learning, recall, dream cycle, daemon mode, multi-harness, crash recovery, and the commands for inspecting and undoing memory.

For reference docs see [docs/README.md](./docs/README.md). For the on-disk contract see [SPEC.md](./SPEC.md).

---

## 1. Setup

### Install

Pick one path. Commands after this section use the npm form (`npx -y dreamd-mcp <cmd>`). The npm shim does **not** put `dreamd` on `PATH`. If you cargo-installed, run `dreamd <cmd>` instead.

This guide describes dreamd 1.0.0. That is the published npm package (`latest` and `next`). If `npx -y dreamd-mcp --version` prints an older version, section 8's commands are not in that package. A few commands are never forwarded by the npm shim and always need the `dreamd` binary: `status`, `recall`, `score`, `archive`, `migrate`. The curl examples use a Unix socket, so they are for Linux and macOS (WSL2 on Windows).

```bash
# npm (no Rust required)
npx -y dreamd-mcp --version

# cargo (from a clone) — installs the `dreamd` binary
cargo install --path crates/dreamd-cli
dreamd version
```

### Set up a project

```bash
cd ~/your-project    # must contain .git/, Cargo.toml, package.json, or pyproject.toml
npx -y dreamd-mcp setup
```

**What just happened:** `setup` scaffolded `<project>/.agent/` (episodic, semantic, personal, working) by calling `init` — an empty `episodic/AGENT_LEARNINGS.jsonl`, a `working/WORKSPACE.md` scratch file, a commented config template at `.agent/.dreamd/config.toml`, the project registered in `~/.agent/registry.toml`, `/.agent/.dreamd/` appended to `.gitignore` — then wrote the dreamd MCP block (`npx -y dreamd-mcp`) into the config for the harness you picked: `.mcp.json` for Claude Code, `.cursor/mcp.json` for Cursor.

`setup` prompts when it has a TTY; pass `--yes` (with `--harness claude|cursor|both|none`) in scripts. `npx -y dreamd-mcp init` is the lower-level scaffold primitive — it creates the store and writes no harness config, which is also what `npx -y dreamd-mcp setup --no-write-mcp` gives you. A second `init` where `.agent/` already exists prints `already initialized` and returns before registration and before the `.gitignore` append. A clone that already contains `.agent/` is not registered, and the daemon answers that root with `404`.

Verify:

```bash
ls -la .agent/
npx -y dreamd-mcp doctor
```

`doctor` prints dream-cycle mode and index health, one `name: value` line per check, and exits 0 when they pass. Last cycle status is `dreamd status` (requires a cargo-installed `dreamd` binary; the npm shim does not forward `status`); it exits 1 while no daemon is running.

---

## 2. First learning

Append a learning via the HTTP API (or let your agent call `append_node` over MCP — same shape).

The HTTP API only exists while the daemon is running, so start it first in a second terminal and leave it running (section 5 explains what it does):

```bash
cd ~/your-project
npx -y dreamd-mcp watch
```

Then, back in the first terminal:

```bash
PROJECT=$(pwd)

curl --unix-socket ~/.agent/dreamd.sock \
  -X POST \
  -H "X-Agent-Root: $PROJECT" \
  -H "Content-Type: application/json" \
  -d '{
    "pain": 7.0,
    "importance": 8.0,
    "skill_action": "rust::error_handling::axum_rejection",
    "source_harness": "cursor",
    "content": "Axum route handlers must return impl IntoResponse; unwrapping panics."
  }' \
  http://localhost/api/v1/learn
```

> **Socket path:** the default is `~/.agent/dreamd.sock`. Clients can override it with [`DREAMD_SOCK`](./docs/configuration.md#dreamd_sock); `dreamd watch` always binds `$HOME/.agent/dreamd.sock`.

The response is `201` with the minted id: `{"id":"evt_…","timestamp":"…","deduplicated":false}`.

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

> See [docs/salience.md](./docs/salience.md) for the factor-by-factor derivation.

Each result includes `score`, `bm25`, `salience`, `source` (`episodic` for a raw event, `semantic` for a `LESSONS.md` lesson), `content`, and `metadata` (timestamp, pain, importance, recurrence, plus `skill_action` and `source_harness` — each hit's cluster key and authoring harness, so recall is cross-harness-attributable).

> **Timing:** A learning appended seconds ago may not appear until the next index commit (5 s cadence). An empty `{"results": []}` right after an append is normal; wait a few seconds and retry.

On 1.0.0, add `&explain=1` to get a `citations` array beside `results` (id, score components, layer, age, pain, importance, recurrence per hit) — see [docs/observability.md](./docs/observability.md). With a cargo-installed binary, `dreamd recall "axum unwrap" --explain` prints the same ranking as a table with the factor breakdown.

---

## 4. The dream cycle

Seed a few related learnings: repeat the section 2 `curl` twice more with different `content` and the same `skill_action` (or use the MCP `append_node` tool), so the cluster has three events. Preview, then consolidate:

```bash
npx -y dreamd-mcp dream --dry   # prints the LESSONS.md it would write; writes nothing
npx -y dreamd-mcp dream
```

With the daemon running the cycle is proxied to it and prints `dream cycle complete (via daemon)`. With no API key configured the cycle is deterministic and makes no network call.

> **Git:** after a successful cycle dreamd commits `.agent/semantic/LESSONS.md` and `.agent/episodic/AGENT_LEARNINGS.jsonl` to your project's git repo as `dreamd <noreply@dreamd.dev>` with the message `dreamd: cycle YYYY-MM-DD`. Pass `--no-commit` to skip that commit. The commit is also skipped (with a WARN log line) when either file had uncommitted changes when the cycle started — which includes a fresh `.agent/` you have not committed yet. `--no-commit` also skips the daemon proxy and runs the cycle in this process. Do not use it while `dreamd watch` is serving the same project: it rewrites the log under the daemon and then fails on the index lock the daemon holds. Stop `watch` first, or run `dreamd dream` with no flag so the daemon stays the writer.

**What just happened:**

1. WAL written (`dream_in_progress.wal`) before any destructive step
2. Episodic events clustered by `skill_action`
3. Clusters with ≥3 events in a 7- or 30-day window are promotion candidates; the cycle writes **one** lesson to `.agent/semantic/LESSONS.md` — the highest-salience exemplar from the top cluster (salience, then pain, then importance, then EventId)
4. Promoted exemplar events get `pinned: true` in JSONL
5. Decay pruner archives stale unpinned events to `.agent/.dreamd/snapshots/`
6. `recurrence_counts.json` updated; WAL committed
7. (1.0.0) The memory files are snapshotted under `.agent/.dreamd/branches/` and named `snap-<UTC timestamp>` (section 8 uses these)

Inspect outputs:

```bash
cat .agent/semantic/LESSONS.md
cat .agent/semantic/recurrence_counts.json | jq .
grep '"pinned":true' .agent/episodic/AGENT_LEARNINGS.jsonl
```

Re-run is idempotent on identical input **and the same `now_sec`**. The CLI uses wall clock unless `SOURCE_DATE_EPOCH` is set, so two bare runs seconds apart are not byte-identical (`last_updated` changes).

---

## 5. Daemon mode

You started the shared writer in section 2:

```bash
cd ~/your-project
npx -y dreamd-mcp watch
```

**What that did:** The daemon bound `~/.agent/dreamd.sock` (`0600`), booted the coordinator + pinned Tantivy handle for this project, and blocks until SIGINT/SIGTERM.

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

Stderr should show `dreamd mcp: daemon reachable at … — serving Remote (daemon proxy)` when the daemon is reachable. If no daemon is running, MCP falls back to in-process with no default-stderr line (`DREAMD_LOG=debug` logs `daemon not found … running in-process`).

To have the daemon start at login instead of in a terminal, `dreamd service install` writes a per-user systemd unit (Linux) or LaunchAgent (macOS) that supervises this same foreground `watch` — no `sudo`, no second logger. See [docs/install.md](./docs/install.md).

---

## 6. Multi-harness

Point two agents at the same project:

| Harness | Config |
|---|---|
| Claude Code | [adapters/claude-code/README.md](./adapters/claude-code/README.md) |
| Cursor | [adapters/cursor/README.md](./adapters/cursor/README.md) |
| Cline | [adapters/cline/README.md](./adapters/cline/README.md) |
| Aider | [adapters/aider/README.md](./adapters/aider/README.md) |

With `npx -y dreamd-mcp watch` running, both harnesses share one coordinator. A learning appended in Cursor is recallable in Claude Code after the commit cadence.

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
kill -9 $(pgrep -u "$(id -u)" -f 'dreamd watch')   # matches the native daemon on both the cargo and npm paths
```

Or study the static fixture: [examples/crash-recovery/](./examples/crash-recovery/).

Restart:

```bash
npx -y dreamd-mcp watch
```

**What just happened:** The restart replaces the stale socket the killed daemon left behind. If `dream_in_progress.wal` exists, recovery runs before serving traffic — temp files from incomplete intents are cleaned up, WAL deleted, `state.json` marked `failed`. The store is never left half-promoted. Killing an idle daemon leaves no WAL, so there is nothing to recover and the last cycle status is unchanged.

Verify:

```bash
npx -y dreamd-mcp doctor
ls .agent/.dreamd/dream_in_progress.wal   # should be absent after recovery
```

---

## 8. Inspect and undo

These need dreamd 1.0.0 or later. `blame`, `salience-drift`, `memory`, and `forget` are forwarded by the 1.0.0 npm shim; `recall` and `score` need the cargo-installed `dreamd`.

See why a query ranked the way it did:

```bash
dreamd recall "axum error" --explain   # table + per-hit salience factors
dreamd blame "axum error"              # id, timestamp, skill_action, content, bm25, salience, total, layer
```

After section 4 the lesson shows up as its own row with an `lsn_evt_…` id and layer `semantic`, next to the episodic events it was promoted from.

Branch the memory files before an experiment, then compare:

```bash
dreamd memory branch before-experiment   # snapshots the live files, prints the object id
dreamd memory branches                   # `*` marks the current branch; snap-* are the automatic ones
dreamd memory diff <older-ref> <newer-ref>
```

`memory checkout <name>` replaces the live memory files with that snapshot. It and `memory bisect` refuse while `dreamd watch` is running, so stop the daemon first. Checkout does not rebuild the recall index; `dreamd recall` can keep returning the store you left until `dreamd doctor --repair`. Full walkthrough: [docs/branching-guide.md](./docs/branching-guide.md). Bisect is in that guide too; there is no `memory bisect reset`.

Remove one event (stop the daemon first — `forget` refuses while it is running):

```bash
dreamd forget evt_… --dry-run                  # prints `dry-run removed=true lesson=…`; writes nothing
dreamd forget evt_… --proof forget-receipt.json
dreamd doctor --provenance
```

`forget` removes the event from the JSONL and drops or unlinks the lesson that names it. `--proof` writes a `forget-receipt/1.0` JSON file — a record of what was removed, not a signed proof. The provenance ledger under `.agent/.dreamd/provenance/` is append-only, so `doctor --provenance` afterwards lists the forgotten event's old edges as orphans and exits 1; after a `forget` that is expected. Format: [docs/provenance.md](./docs/provenance.md).

---

## Next steps

| Topic | Doc |
|---|---|
| HTTP API details | [docs/http-api.md](./docs/http-api.md) |
| Configuration | [docs/configuration.md](./docs/configuration.md) |
| Memory branches | [docs/branching-guide.md](./docs/branching-guide.md) |
| Recall citations and salience drift | [docs/observability.md](./docs/observability.md) |
| Architecture | [ARCHITECTURE.md](./ARCHITECTURE.md) |
| CLI reference | [docs/cli.md](./docs/cli.md) |
| Troubleshooting | [docs/troubleshooting.md](./docs/troubleshooting.md) |
| Runnable fixtures | [examples/README.md](./examples/README.md) |
