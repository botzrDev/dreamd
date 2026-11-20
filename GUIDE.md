# dreamd — guided walkthrough

A linear tutorial (~20 minutes) that walks through every major workflow: install, first learning, recall, dream cycle, daemon mode, multi-harness, crash recovery, and the commands for inspecting and undoing memory.

For reference docs see [docs/README.md](./docs/README.md). For the on-disk contract see [SPEC.md](./SPEC.md).

---

## 1. Setup

### Install

Pick one path. Commands after this section use the npm form (`npx -y dreamd-mcp <cmd>`). The npm shim does **not** put `dreamd` on `PATH`. If you cargo-installed, run `dreamd <cmd>` instead.

This guide describes dreamd 1.4.0. If `npx -y dreamd-mcp --version` prints a version older than 1.0.0, section 8's commands are not in that package. The 1.2.0 npm shim did not forward `status`, `recall`, `score`, `archive` and `migrate`; from 1.2.1 every command here runs as `npx -y dreamd-mcp <cmd>`. The curl examples use a Unix socket, so they are for Linux and macOS (WSL2 on Windows).

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

**What just happened:** `setup` scaffolded `<project>/.agent/` (episodic, semantic, personal, working) by calling `init` — an empty `episodic/AGENT_LEARNINGS.jsonl`, a `working/WORKSPACE.md` scratch file, a commented config template at `.agent/.dreamd/config.toml`, the project registered in `~/.agent/registry.toml`, `/.agent/.dreamd/` appended to `.gitignore` — then wrote the dreamd MCP block (`npx -y dreamd-mcp`) into the config for the harness you picked: `.mcp.json` for Claude Code, `.cursor/mcp.json` for Cursor. For Claude Code it also merges a `<!-- dreamd:start -->` block. With an `AGENTS.md` at the project root, the block goes there, and a missing `CLAUDE.md` becomes the one line `@AGENTS.md`, which Claude Code needs to keep loading `AGENTS.md`; a `CLAUDE.md` of your own that does not import `AGENTS.md` keeps the block instead. Without an `AGENTS.md`, the block goes into `CLAUDE.md`. For Cursor it writes `.cursor/rules/dreamd-recall.mdc`. The card it prints at the end lists each of these, with an Undo line for each.

`setup` prompts when it has a TTY; pass `--yes` (with `--harness claude|cursor|both|none`) in scripts. `npx -y dreamd-mcp init` is the lower-level scaffold primitive — it creates the store and writes no harness config, which is also what `npx -y dreamd-mcp setup --no-write-mcp` gives you. A second `init` where `.agent/` already exists and the project is already registered prints `already initialized` and returns before the `.gitignore` append. If `.agent/` exists but the registry entry is missing — after `dreamd init --uninstall-project`, a lost `registry.toml`, or a moved `HOME` — `dreamd init` registers it and does not rewrite the store. `dreamd init --uninstall-project` removes only the registry entry; run `dreamd init` again to put it back.

Then commit the store. `setup` prints this command while git has no commit of `.agent/`:

```bash
git add .agent .gitignore && git commit -m "Add dreamd memory" -- .agent .gitignore
```

Only `.agent/.dreamd/` is gitignored. The episodic log and `LESSONS.md` are meant to be committed, and until they are, `git clean -fd` deletes them. The `--` paths keep anything else you had staged out of this commit. To keep memory out of git instead, add `.agent/` to `.gitignore`.

Verify:

```bash
ls -la .agent/
npx -y dreamd-mcp doctor
```

`doctor` prints dream-cycle mode and index health, one `name: value` line per check, and exits 0 when they pass. Its `memory_in_git:` line says whether git has a commit of the episodic log, and warns while git has none and the log holds events. Last cycle status is `npx -y dreamd-mcp status` (1.2.1 and later; the 1.2.0 shim did not forward `status`); it exits 1 while no daemon is running.

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

> **Socket path:** there is one daemon socket per user, at `~/.agent/dreamd.sock` by default, and the daemon routes each request to its project by `X-Agent-Root` through `~/.agent/registry.toml`. `dreamd watch` and every client resolve [`DREAMD_SOCK`](./docs/configuration.md#dreamd_sock) the same way, so set it on both when one user needs a second daemon. A client and a daemon of different versions refuse each other with `version mismatch: client …, daemon …` and the fix: stop the other daemon with `npx -y dreamd-mcp update`, start this one with `npx -y dreamd-mcp watch`, then reload your harness. A `dreamd` binary names `dreamd watch`.

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
salience = exp(-age_days/14) × (pain/10) × (importance/10) × (1 + ln(1 + d(recurrence)))
final_score = bm25 × salience

d(0) = 0
d(r) = 1.5 - 0.5 / r    for r >= 1
```

> See [docs/salience.md](./docs/salience.md) for the factor-by-factor derivation.

Each result includes `id` (an event's `evt_…` id, or a lesson's `lsn_…` id), `score`, `bm25`, `salience`, `source` (`episodic` for a raw event, `semantic` for a `LESSONS.md` lesson), `content`, and `metadata` (timestamp, pain, importance, recurrence, plus `skill_action` and `source_harness` — each hit's cluster key and authoring harness, so recall is cross-harness-attributable).

> **Timing:** `search_nodes` and `GET /api/v1/recall` see every learning whose `append_node` or `POST /api/v1/learn` returned before the search. When the index has not committed one yet, the search asks for that commit first and waits at most one second; past that it answers from what is already committed. `dreamd recall` and the other CLI commands read the index on disk, so they can trail an append by up to 5 s (the index commit cadence) while `dreamd watch` or `dreamd mcp` is running.

On 1.0.0 and later, add `&explain=1` to get a `citations` array beside `results` (id, score components, layer, age, pain, importance, recurrence per hit). See [docs/observability.md](./docs/observability.md). With a cargo-installed binary, `dreamd recall "axum unwrap" --explain` prints the same ranking as a table with the factor breakdown.

---

## 4. The dream cycle

Seed a few related learnings: repeat the section 2 `curl` twice more with different `content` and the same `skill_action` (or use the MCP `append_node` tool), so the cluster has three events. Preview, then consolidate:

```bash
npx -y dreamd-mcp dream --dry   # prints the LESSONS.md it would write; writes nothing
npx -y dreamd-mcp dream
```

With the daemon running the cycle is proxied to it and prints `dream cycle complete (via daemon)`. With no API key configured the cycle is deterministic and makes no network call.

> **Git:** after a successful cycle dreamd commits `.agent/semantic/LESSONS.md` and `.agent/episodic/AGENT_LEARNINGS.jsonl` to your project's git repo as `dreamd <noreply@dreamd.dev>` with the message `dreamd: cycle YYYY-MM-DD`. It does not commit while HEAD is detached or while a merge, rebase, `git am`, cherry-pick, revert or bisect is in progress. The cycle still runs, its changes stay in the working tree, and `dreamd dream` says what to finish; the next cycle commits them. Pass `--no-commit` to skip that commit. The commit is also skipped when either file held a change no dream cycle wrote when the cycle started, such as a hand edit or `dreamd forget`; appends and a cycle's own rewrites never hold it. `dreamd dream` then prints why and which files, and once you commit those files with git the next cycle commits again. A cycle also keeps a `LESSONS.md` that changed outside a dream cycle, instead of rewriting it, and says so. It writes the lesson again once you commit or delete the file. Only the lesson is held: pins and decay still change the log. `--no-commit` also skips the daemon proxy and runs the cycle in this process. Do not use it while `dreamd watch` is serving the same project: it rewrites the log under the daemon and then fails on the index lock the daemon holds. Stop `watch` first, or run `dreamd dream` with no flag so the daemon stays the writer.

**What just happened:**

1. Every output is staged under `.agent/.dreamd/stage/` first; nothing live changes until one redo WAL (`dream_in_progress.wal`) commits them all
2. Episodic events clustered by `skill_action`
3. A key promotes when at least 3 of its events that no deeper key has claimed fall in the last 30 days, deepest key first, and a one-segment key such as `rust` counts only events filed as exactly `rust`. The cycle writes **one** lesson to `.agent/semantic/LESSONS.md` — the highest-salience exemplar (salience, then pain, then importance, then EventId) from the top cluster (summed salience, then more events, then the newest event, then the key ascending)
4. Promoted exemplar events get `pinned: true` in JSONL
5. Decay pruner archives stale unpinned events to `.agent/.dreamd/snapshots/`
6. `recurrence_counts.json` updated; the redo WAL committed, its renames applied, and the WAL deleted
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

Stderr should show `dreamd mcp: daemon reachable at … — serving Remote (daemon proxy)` when the daemon is reachable. On Linux and macOS, if no daemon is running, MCP falls back to in-process with no default-stderr line (`DREAMD_LOG=debug` logs `daemon not found … running in-process`). On Windows there is no in-process server: start `dreamd watch` first or `dreamd mcp` exits 2. Once that daemon is up, learn, the dream cycle, and recall run on it.

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

With `npx -y dreamd-mcp watch` running, both harnesses share one coordinator. A learning appended in Cursor is recallable in Claude Code as soon as its append returns.

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

**What just happened:** The restart replaces the stale socket the killed daemon left behind. Recovery runs before serving traffic. If `dream_in_progress.wal` exists, the cycle had reached its commit point: recovery rolls it forward, applying the rest of its staged outputs, and marks `state.json` `complete`. If only staged outputs exist (`.agent/.dreamd/stage/`), the cycle crashed before its commit point: recovery wipes them and marks `state.json` `failed`, and the store is exactly as it was before that cycle. The store is never left half-promoted. Killing an idle daemon leaves no WAL, so there is nothing to recover and the last cycle status is unchanged.

Verify:

```bash
npx -y dreamd-mcp doctor
ls .agent/.dreamd/dream_in_progress.wal   # should be absent after recovery
```

---

## 8. Inspect and undo

These need dreamd 1.0.0 or later. `blame`, `salience-drift`, and `forget` are forwarded by the npm shim (1.0.0 and later); `recall` and `score` are forwarded from 1.2.1.

See why a query ranked the way it did:

```bash
dreamd recall "axum error" --explain   # table + per-hit salience factors
dreamd blame "axum error"              # id, timestamp, skill_action, content, bm25, salience, total, layer
```

After section 4 the lesson shows up as its own row with an `lsn_evt_…` id and layer `semantic`, next to the episodic events it was promoted from.

To keep a copy of memory before an experiment, commit `.agent/` and branch it with git. The `dreamd memory` commands were deprecated in 1.2.0 (there is no 1.1.0 tag) and will be removed in 2.0. They still run. [docs/branching-guide.md](./docs/branching-guide.md) describes them for an install that already uses them.

Remove one event (stop the daemon first — `forget` refuses while it is running):

```bash
dreamd forget evt_… --dry-run                  # prints `would_remove=true lesson=…`; writes nothing
dreamd forget evt_… --proof forget-receipt.json
dreamd doctor --provenance
```

`forget` removes the event from the JSONL and drops or unlinks the lesson that names it. `--proof` writes a `forget-receipt/1.0` JSON file — a record of what was removed, not a signed proof. The provenance ledger under `.agent/.dreamd/provenance/` is append-only, so `doctor --provenance` afterwards lists the forgotten event's old edges as orphans and exits 1; after a `forget` that is expected. Format: [docs/provenance.md](./docs/provenance.md).

---

## 9. Clones and worktrees

A clone or a `git worktree` of a project whose `.agent/` is committed has that memory. Part of `.agent/` travels with git, and part belongs to each checkout.

- **Shared through git:** the committed files under `.agent/`, such as the episodic log (`episodic/AGENT_LEARNINGS.jsonl`) and `semantic/LESSONS.md`.
- **Each checkout's own:** `.agent/.dreamd/`, which git ignores. It holds the index, `writer.lock`, the provenance ledger and the project `config.toml`. A clone starts without it: the first open builds the index from the log, and the project's own `config.toml` is missing until you copy it, so the user config (`~/.config/dreamd/config.toml` on Linux; [configuration](docs/configuration.md#config-file-locations) gives the macOS and Windows paths) and the built-in defaults apply. Git keeps no empty folder, so a clone has no `personal/`, and no `semantic/` until a dream cycle has written `LESSONS.md`.
- **With a daemon running:** run `dreamd init` (or `npx -y dreamd-mcp init`) once in each clone and worktree. Until then every call answers with the folder and the command:

  ```
  the store in /path/to/clone is not registered with the running daemon. Run `dreamd init` in /path/to/clone once: it registers the existing store without changing its files (a clone or a worktree is not registered until then), and the next call works without a restart.
  ```

  `init` registers the store, re-creates the missing folders and changes no file. In-process mode needs nothing for the tools to work; `init` still re-creates the folders `dreamd doctor` checks.
- **Merging two checkouts:** each checkout appends to its own copy of the log, so a merge usually conflicts at the end of `AGENT_LEARNINGS.jsonl`. Keep both sides' new lines: each one is an event with its own id. A line that was in the log before the checkouts split is still one event when a side changed it (a dream cycle pins the events its lessons cite, and `dreamd archive --force-unpin` unpins one), so keep only the changed copy. A line one side removed (`dreamd forget`, or decay in a dream cycle) stays removed. If both sides ran a dream cycle, `semantic/LESSONS.md` conflicts as well; keep one side's file whole. Then run `dreamd doctor`: `duplicate_id` lists any id left on two lines.

---

## Next steps

| Topic | Doc |
|---|---|
| HTTP API details | [docs/http-api.md](./docs/http-api.md) |
| Configuration | [docs/configuration.md](./docs/configuration.md) |
| Memory branches (deprecated; removed in 2.0) | [docs/branching-guide.md](./docs/branching-guide.md) |
| Recall citations and salience drift | [docs/observability.md](./docs/observability.md) |
| Architecture | [ARCHITECTURE.md](./ARCHITECTURE.md) |
| CLI reference | [docs/cli.md](./docs/cli.md) |
| Troubleshooting | [docs/troubleshooting.md](./docs/troubleshooting.md) |
| Runnable fixtures | [examples/README.md](./examples/README.md) |
