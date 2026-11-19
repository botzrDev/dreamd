# Troubleshooting

Symptom → cause → fix → prevention. For deeper reference see [http-api.md](./http-api.md), [configuration.md](./configuration.md), and [SECURITY.md](../SECURITY.md).

Commands below use `dreamd`. On the npm path the shim does **not** put `dreamd` on `PATH`. Use `npx -y dreamd-mcp <cmd>`. From 1.2.1 that forwards every command. The 1.2.0 shim did not forward `status`, `recall`, `score`, `archive`, or `migrate` (an unlisted first token was passed to `dreamd mcp` and failed there).

---

## Daemon won't start

**Symptom:** `dreamd watch` exits 1 right after starting. The stderr line names the cause:

| stderr | Cause |
|---|---|
| `dreamd watch: supervisor: UDS bind failed: socket already bound by another writer-process` | Another `dreamd watch` (yours, or the one `dreamd service install` supervises) is already serving `~/.agent/dreamd.sock`. There is one daemon per user. |
| `dreamd watch: project writer lock is held by pid N (kind)` | Another dreamd process writes this project, and `kind` says which: `watch` (a second `dreamd watch` started from the same project), `mcp` (a standalone in-process `dreamd mcp` that a harness spawned before the daemon was up), or a CLI writer that is still running (`dream`, `forget`, `archive`, `checkout`, `bisect`, `repair`, `setup`). `N` is its pid. |
| `dreamd watch: the index writer lock is held by a dreamd process that predates the project writer lock (1.0.x); stop it first` | A dreamd from before the project writer lock (1.0.x) is writing this project. It holds only the index lock, so its pid is not known. |
| `dreamd watch: supervisor: UDS bind failed: UDS bind failed: path must be shorter than SUN_LEN` | `$HOME/.agent/dreamd.sock` is longer than a Unix socket path may be (under 108 bytes on Linux, 104 on macOS). Typical in deeply nested sandbox or CI homes. |

**Fix:**

```bash
# Is a dreamd already running?
pgrep -a dreamd
dreamd status                 # "daemon: running" means the socket answers

# Already-bound: use the running daemon, or stop it first (Ctrl-C / SIGTERM, or `dreamd service restart`).
# Writer lock held by pid N (mcp): close the harness session(s) holding that in-process
#           `dreamd mcp`, start `dreamd watch`, then reload the harness so its MCP server
#           bridges to the daemon. Any other kind: wait for pid N to finish, or stop it.
# Older dreamd (1.0.x): stop that process (`pgrep -a dreamd`) and upgrade the harnesses that
#           spawn it, then start `dreamd watch`.
# SUN_LEN: run under a shorter $HOME.
```

A stale socket file left by a crashed daemon (`kill -9`) is **not** one of these: the next `dreamd watch` probes it, finds nothing listening, unlinks it and binds. `dreamd doctor` reports it as `orphaned_uds: … [WARNING: socket file exists but nothing is listening; …]`, and `dreamd doctor --repair` (or `rm -f ~/.agent/dreamd.sock`) removes it without starting a daemon. (`--repair` also rebuilds the index, and refuses outright while a daemon is live.)

**Prevention:** Start `dreamd watch` before the harnesses that use it, and let it shut down via SIGINT/SIGTERM — a graceful stop unlinks the socket.

---

## Watch shows only its start line

**Symptom:** `dreamd watch` prints `dreamd watch: serving on …` and then nothing while harnesses use it.

**Cause:** By default the console shows the daemon's start and stop lines, warnings and errors, but not one line per request. The per-request `http request` lines go to `~/.agent/dreamd.log`, which is JSON.

**Fix:**

```bash
tail -f ~/.agent/dreamd.log        # one JSON line per request
DREAMD_LOG=info dreamd watch       # request lines (and library INFO) on the console too
DREAMD_LOG_FORMAT=json dreamd watch  # console lines as JSON
```

---

## MCP server won't connect

**Symptom:** Harness shows dreamd MCP disconnected, or `dreamd mcp` exits right after starting.

**Cause (common):**

1. `npx dreamd-mcp` not installed / wrong package name (`dreamd-mcp`, not scoped)
2. No `.agent/` in the project — server boots empty backend
3. Daemon expected but not running (Remote / daemon proxy)
4. A `DREAMD_SOCK` the daemon was not started with. `dreamd watch` and every client resolve it the same way, so the daemon and the MCP client need the same value, or neither sets it (then both use `~/.agent/dreamd.sock`). A relative value is refused: `dreamd mcp: DREAMD_SOCK is not an absolute path: …`, exit 1, and `dreamd watch` exits 1 the same way
5. `dream_cycle_mode = "auto"` in `config.toml` — `dreamd mcp` and `dreamd watch` both exit 1 on it (see [configuration.md](./configuration.md#dream_cycle_mode))
6. Native Windows — the npx shim has no Windows binary and exits 1. Build `dreamd` and start `dreamd watch` first (see [windows.md](./windows.md))
7. A daemon of another dreamd version, such as a 1.2.x `dreamd watch` that `setup` started before an upgrade. The tool call fails with `version mismatch: client …, daemon …`, or `daemon unknown` for a daemon from a release without this check. Under `npx -y dreamd-mcp`, run `npx -y dreamd-mcp update` to stop it, then `npx -y dreamd-mcp watch`, then reload the harness. With a `dreamd` binary, stop it and run `dreamd watch`. If a `dreamd service` unit runs the daemon, follow `update` with `npx -y dreamd-mcp service install` (add `--force` on macOS) and `npx -y dreamd-mcp service restart` in place of `watch`, because the unit names the old version's binary. Or point `DREAMD_SOCK` at a daemon of this version
8. A resumed Claude Code session from another folder. Claude Code reads a project's `.mcp.json` from the folder the session started in. A session that was started in another folder and is then resumed does not load this repo's `.mcp.json`, so dreamd's tools are missing from it. Start a new session from the repo root.

**Fix:**

```bash
npx -y dreamd-mcp init         # creates .agent/ + registry entry
npx -y dreamd-mcp watch &      # if you want the daemon proxy
npx -y dreamd-mcp              # test stdio server manually
npx -y dreamd-mcp doctor       # verify store health
```

Check MCP stderr for `dreamd mcp: daemon reachable at … — serving Remote (daemon proxy)`. If no daemon is running there is no default-stderr fallback line; `DREAMD_LOG=debug` logs `daemon not found … running in-process`.

**Prevention:** Run `npx -y dreamd-mcp init` (or `dreamd init`) before first MCP session. For multi-agent setups, start `npx -y dreamd-mcp watch` once per user login.

---

## SIGKILL mid dream cycle — what happens?

**Symptom:** Daemon killed during `dreamd dream` or `POST /api/v1/dream`; store may look inconsistent.

**Cause:** a crash left `.dreamd/stage/` and/or `dream_in_progress.wal`. A cycle that fails with an error, such as a `500` from `POST /api/v1/dream`, leaves neither: it sets `failed` before it returns, and the next cycle runs. Before 1.3.1 the daemon answered every later `POST /api/v1/dream` for that project with `409` until `dreamd watch` restarted.

**Fix:** Restart the daemon or run any command that triggers WAL recovery:

```bash
npx -y dreamd-mcp watch    # recovers on startup before serving
npx -y dreamd-mcp status   # last_dream_cycle
# or: cat .agent/.dreamd/state.json
npx -y dreamd-mcp doctor   # dream-cycle mode + index health; with the daemon stopped, --repair rebuilds a stale index
```

Recovery depends on how far the cycle got. A `dream_in_progress.wal` means the cycle reached its commit point: recovery rolls it forward, applying the rest of its staged outputs, and sets `state.json` → `complete`. With no WAL, the cycle crashed before its commit point: the stage dir is wiped, the store is exactly as it was before that cycle, and `state.json` → `failed`. A WAL written by a version before 1.1.0 (an `intents` list) is recovered the old way. See [examples/crash-recovery/](../examples/crash-recovery/).

A WAL whose `format` this version does not know, or one that names a path outside `.agent/`, makes every writer refuse to start (`wal recovery: …`). This is deliberate: dreamd will not guess at a store it cannot recover.

A WAL that is not JSON, whose `format` this version does not know, or that names a path outside the store, is not recovered by restarting. `dreamd doctor` prints `dream_cycle_wal:` with the file, the reason, and the command. With the daemon stopped:

```bash
dreamd doctor --repair
```

That moves `.agent/.dreamd/dream_in_progress.wal` to `dream_in_progress.wal.corrupt-YYYY-MM-DD` (a later one the same day gets `-2`, `-3`, …), deletes nothing, wipes `.agent/.dreamd/stage/`, and sets `state.json` to `failed`. It does not read the WAL for meaning. A `redo/1` WAL this version can apply is still rolled forward; `--repair` does not quarantine that one. After a quarantine, the repair output includes `store invariants:` — `ok`, or the violations and the quarantined file, because a WAL corrupted after its commit point can leave a half-applied cycle. The Linux user unit (`~/.config/systemd/user/dreamd.service`, `Restart=on-failure`, `RestartSec=5`) starts `dreamd watch` again after every exit 1, and `watch` exits 1 again until this repair has run, so the journal repeats the two-line message. Stop the unit, run the repair, then start it:

```bash
systemctl --user stop dreamd.service
dreamd doctor --repair
systemctl --user start dreamd.service
```

Recovery keys off the WAL file (`.agent/.dreamd/dream_in_progress.wal`). It runs whenever a process takes the project writer lock: `dreamd watch` at start, an in-process `dreamd mcp`, and the CLI writers (`dreamd dream` without a daemon or with `--no-commit`, `forget`, `archive --force-unpin`, `memory checkout`, `bisect`, `doctor --repair`). Taking the lock also resets a `state.json` left at `in_progress` with no WAL to `failed`. An interrupted cycle therefore needs no hand edit: the next writer recovers it and then runs. While a cycle is live, a second one is refused: HTTP 409 through the daemon, or the writer-lock refusal (`project writer lock is held by pid N (kind)`, exit 1) from the CLI.

**Prevention:** Don't run overlapping dream cycles (HTTP 409 guards concurrent cycles). Use one daemon writer per machine.

---

## Two agents writing simultaneously

**Symptom:** Torn JSONL lines, interleaved records, or lost appends.

**Cause:** Two **standalone** in-process MCP servers writing to the same `.agent/` without a shared daemon.

**Fix:**

```bash
# One serialized writer for the machine
npx -y dreamd-mcp watch
```

Point all harnesses at MCP — they auto-bridge to the daemon proxy when the socket is up.

**Prevention:** Never run two `npx -y dreamd-mcp` / `dreamd mcp` processes against one project without a shared `watch` daemon. See [GUIDE.md](../GUIDE.md) §6.

---

## `dreamd dream --no-commit` while `watch` is running

**Symptom:** `dreamd dream --no-commit` exits 1 with `dreamd: dream cycle error: project writer lock is held by pid N (watch)` while `dreamd watch` is up. Nothing was written.

**Cause:** `--no-commit` skips the git commit and also skips the daemon proxy, so the cycle runs in this process. It takes the project writer lock first and found another writer holding it: `dreamd watch`, or an in-process `dreamd mcp` (`(mcp)`).

**Fix:** Stop `dreamd watch`, then run the cycle. Or omit `--no-commit` and let the running daemon run it (`dream cycle complete (via daemon)`).

---

## `git clean -fdX` deleted `.agent/.dreamd/`

**Symptom:** you ran `git clean -fdX` (or `-fdx`), or deleted `.agent/` and ran `dreamd setup` or `dreamd init` again, while dreamd was running.

**What was deleted:** `git clean -fdX` removes ignored files, and `.agent/.dreamd/` is gitignored. That folder holds:

- the search index, rebuilt from the log;
- `state.json`, the record of the last dream cycle;
- `config.toml`, this project's settings;
- `snapshots/`, the decay archive: events a dream cycle moved out of the log;
- `branches/`, memory branches and autosnaps;
- `provenance/ledger.jsonl`, which records where each lesson came from.

The episodic log (`.agent/episodic/`) and `LESSONS.md` are not ignored, so they stay.

**What happens next:**

- **`dreamd watch`** notices at the next learn, recall or dream for that project, opens the project again with a new store folder, indexes the log, and lets the request through: nothing to restart. `/health` and the salience report do not check, so until one of those three requests arrives they answer from the old store. If another dreamd command took the new store first, requests answer `project writer lock is held by pid N (…)` until that command finishes.
- **An in-process `dreamd mcp`** (no `watch` running) stops writing, and every tool call answers that the store was deleted or replaced. Reload your MCP harness.

**What can't come back:** an edited `config.toml`, the decay archive, memory branches, the ledger's lesson edges, and `state.json`. Without `state.json`'s record of what the last cycle wrote, a `LESSONS.md` that differs from the one git last committed reads as a hand edit, and the next dream cycle holds its git commit until you commit that file. dreamd rebuilds the index and its ledger edges from the log; nothing else. If you need the rest, stop `dreamd watch`, restore `.agent/.dreamd/` from a backup, then start `dreamd watch` again.

---

## `git clean -fd` deleted the episodic log

**Symptom:** `dreamd doctor` prints `episodic_log: missing  [WARNING: …]` or `episodic_log: no records  [WARNING: …]`, and its `index_freshness` line says the index holds events the episodic log does not.

**Why:** `git clean -fd` deletes untracked files, and `.agent/` stays untracked until you commit it. `git reset --hard` deletes a log that is only staged. Only `.agent/.dreamd/` is gitignored; the episodic log and `LESSONS.md` are meant to be committed. `dreamd setup` prints the commit command, and `dreamd doctor` and `dreamd status` print `memory_in_git: not committed` until git has a commit of the log.

**What to do:**

1. Stop `dreamd watch` and your agents. The next dreamd writer starts a new, empty log, and after one new event dreamd's next index check deletes the old events from the index when there are fewer than 64 of them.
2. Restore `.agent/episodic/AGENT_LEARNINGS.jsonl` from a backup or your editor's local history. If an earlier commit holds it, `git log --all -- .agent/episodic/AGENT_LEARNINGS.jsonl` finds that commit.
3. Run `dreamd doctor` again. The warning goes once the log holds the events.

If no copy is left, the events are lost: the index holds their text, but dreamd cannot write them back to the log. `dreamd doctor --repair` refuses while it would delete them from the index, and `dreamd doctor --repair --allow-empty-log` rebuilds an empty index.

**Next time:** commit the store after setup:

```bash
git add .agent .gitignore && git commit -m "Add dreamd memory" -- .agent .gitignore
```

Or add `.agent/` to `.gitignore` to keep memory out of git; doctor then prints `memory_in_git: ignored`. `git clean -fdx` (lowercase `x`) deletes ignored files too, so keep a backup in that case.

---

## How do I reset or clear memory?

| Goal | Command |
|---|---|
| Clear session scratchpad | `dreamd reset workspace` (asks first; `--yes` skips the prompt and is required when stdin is not a tty) |
| Remove one memory | `dreamd forget <event id>` — stop the daemon first; `--dry-run` previews. Find the id with `dreamd recall <query>` |
| Uninstall dreamd (stop servers, unregister project, clear caches; keeps `.agent/` stores) | `dreamd uninstall` — flags and details: [packages/dreamd-mcp/README.md](../packages/dreamd-mcp/README.md#uninstall--reset) |
| Update to the latest release | `dreamd update`, then re-run `npx -y dreamd-mcp` to fetch the new binary |
| Remove project from daemon registry only (keep everything else) | `dreamd init --uninstall-project`. The store stays. Run `dreamd init` again to put the registry entry back. |
| Remove the login service (systemd unit / LaunchAgent / scheduled task) | `dreamd service uninstall` — see [install.md](./install.md#removing-the-service) |
| Wipe episodic log | Stop the daemon, truncate `.agent/episodic/AGENT_LEARNINGS.jsonl`, then rebuild the index with `dreamd doctor --repair --allow-empty-log`; `--repair` alone refuses while the index holds events the log does not (or use *Full fresh store* below for a clean slate) |
| Full fresh store | Delete `.agent/` and re-run `dreamd init` |

**Warning:** Deleting `.agent/` is destructive. Commit or back up first if the store has value. There is no `dreamd reset --all`.

---

## Socket permission denied

**Symptom:** `403 forbidden: peer UID does not match daemon owner` or cannot connect to socket.

**Cause:** Connecting process runs as a different Unix user than the daemon owner. Socket is `0600` — owner only.

**Fix:**

- Start the daemon and MCP client as the **same user**
- Don't use `sudo dreamd watch` unless MCP also runs as root (don't)
- Check `ls -l ~/.agent/dreamd.sock` — should be your user, mode `srw-------`

**Prevention:** Run `dreamd watch` from your normal login session, not a system service account. `dreamd service install` keeps that property: it registers a per-user systemd unit / LaunchAgent that runs `watch` as you, never a system service account — see [install.md](./install.md).

---

## `dreamd init` exits 2 — no project root found

**Symptom:** `dreamd init` exits 2 and prints:

```text
dreamd: error — no project root found. Run from inside a project directory (must contain .git/, Cargo.toml, package.json, or pyproject.toml). If this is a new folder, run `git init` first.
```

**Cause:** `dreamd init` walks up from the current directory looking for a project-root sentinel — `.git`, `Cargo.toml`, `package.json`, or `pyproject.toml` — and refuses to scaffold if none is found (e.g. a bare empty folder or `mktemp -d`).

**Fix:**

```bash
# Option A: make this folder a project
git init
dreamd init

# Option B: run inside a real project
cd ~/your-project    # must contain .git/, Cargo.toml, package.json, or pyproject.toml
dreamd init
```

**Prevention:** Run `dreamd init` from inside the repo that should own the `.agent/` store; `git init` brand-new folders first.

---

## No `.agent/` directory found

**Symptom:** `dreamd: no .agent/ store found in … or any parent directory — run \`dreamd init\` first.` (`doctor`, `dream`), `dreamd: error — no .agent/ directory found. Run \`dreamd init\` first.` (`recall`, `migrate`, `service install`, …), `dreamd watch: no project root found from …`, or, from both MCP tools, `this folder is not set up for dreamd: there is no .agent/ store in … or a folder above it. Run \`dreamd setup\` in the project folder, then restart this MCP server.` (`npx -y dreamd-mcp setup` under the npm shim). All but the MCP one exit 2.

**Cause:** Project never initialized, or CWD is not inside a project with `.agent/`. A store in `~/.agent/` does not count: that is the daemon home, and the walk up skips it, so a folder under your home with no store of its own finds none. If doctor or status also prints a `daemon_home_store:` line, see [A project store in the daemon home](#a-project-store-in-the-daemon-home).

**Fix:**

```bash
cd ~/your-project    # must contain .git/, Cargo.toml, package.json, or pyproject.toml
dreamd init
```

For Cursor global MCP config, pass `--project-root /absolute/path/to/project` (see [adapters/cursor/README.md](../adapters/cursor/README.md)).

**Prevention:** Run `dreamd init` once per repo; commit `.agent/` (except `.agent/.dreamd/` which is gitignored). After `dreamd setup` (or `init`) in a folder your harness already had open, restart its MCP server: one that started with no store keeps answering "not set up" until it starts again.

---

## A project store in the daemon home

**Symptom:** `dreamd doctor` (which then exits 1) and `dreamd status` print:

```text
daemon_home_store: /home/you/.agent holds a project store (events: N)  [WARNING: …]
```

and `dreamd service uninstall --purge` exits 2 with `refusing to purge /home/you/.agent: it holds a project store (…)` or `… it holds the store of the registered project /home/you …`.

**Cause:** `~/.agent/` is the daemon home: it holds `registry.toml`, the socket, `dreamd.log` and, on Windows, `auth.json`. It is never a project store. A dreamd before 1.3.3 could still use it as one: a folder under your home with no store of its own bound to your home directory, so an MCP append landed in `~/.agent/episodic/`, and `dreamd init` in a git repo at your home directory registered it. dreamd now skips that store. Nothing reads it, nothing moves it, and `--purge` will not delete it for you.

**Fix with events (`events: N`, N > 0):** move the store into the project it belongs to, one that has no `.agent/` yet. Stop the daemon first: `systemctl --user stop dreamd.service`, or end `dreamd watch`. There is no `dreamd service stop`.

```bash
mkdir ~/your-project/.agent
# Move the ones that exist:
mv ~/.agent/episodic ~/.agent/semantic ~/.agent/personal ~/.agent/working ~/your-project/.agent/
rm -rf ~/.agent/.dreamd          # derived state; the index rebuilds from the log
cd ~/your-project
dreamd init                      # registers the store it finds (npx -y dreamd-mcp init)
grep -qxF '/.agent/.dreamd/' .gitignore || echo '/.agent/.dreamd/' >> .gitignore
git add .agent .gitignore && git commit -m "Move dreamd memory into the project"
```

**Fix with `events: 0`:** nothing to keep. Remove it:

```bash
rm -rf ~/.agent/episodic ~/.agent/semantic ~/.agent/personal ~/.agent/working ~/.agent/.dreamd
```

**If your home directory is registered** (the purge refusal names `the registered project /home/you`), drop that entry from your home directory: `cd ~ && dreamd init --uninstall-project`.

None of these touches `registry.toml`, the socket, `dreamd.log` or `auth.json`. Run `dreamd doctor` again: the `daemon_home_store:` line is gone.

**Prevention:** Run `dreamd setup` or `dreamd init` in a project folder, never in your home directory; both now refuse there.

---

## Recall returns no results

**Symptom:** `search_nodes` or `GET /api/v1/recall` returns `{"results":[]}`.

**Causes:**

| Cause | Check |
|---|---|
| Empty store | `wc -l .agent/episodic/AGENT_LEARNINGS.jsonl` |
| Read-after-write window | `search_nodes` and `GET /api/v1/recall` see an append as soon as it returns, waiting at most 1 s for the index. `dreamd recall` reads the index on disk and can trail an append by up to 5 s (index commit cadence) |
| Query mismatch | Try broader terms from known `content` |
| Wrong project | Verify `X-Agent-Root` / MCP project discovery points at this repo |
| Index stale | `npx -y dreamd-mcp doctor`; rebuild with `dreamd doctor --repair` (cargo-installed `dreamd`, or `npx -y dreamd-mcp doctor --repair`). Stop the daemon first — `--repair` refuses while one is live |

**Fix:**

```bash
tail .agent/episodic/AGENT_LEARNINGS.jsonl
curl --unix-socket ~/.agent/dreamd.sock \
  -H "X-Agent-Root: $(pwd)" \
  "http://localhost/api/v1/recall?q=known+phrase&k=10"
```

**Prevention:** Use `dreamd watch` for consistent index state; see [GUIDE.md](../GUIDE.md) §3.

---

## `dreamd doctor` warns about a hand-edited line

`AGENT_LEARNINGS.jsonl` and `~/.agent/registry.toml` are plain files you may edit. The write paths (`append_node`, `POST /api/v1/learn`, `dreamd init`) refuse what these warnings describe; a line written by hand skips those checks. The readers keep working, and `dreamd doctor` tells you what they are doing with the line. Each of these warnings makes `dreamd doctor` exit 1. Line numbers are lines of the file, blank and malformed lines included; doctor lists the first five.

The index keeps the values it indexed, so after fixing a line in `AGENT_LEARNINGS.jsonl` rebuild it: stop `dreamd watch`, run `dreamd doctor --repair`, start `dreamd watch` again.

### `score_range: N line(s) …`

**What the reader does:** The line is a record and is indexed with the stored value. Recall clamps `pain` and `importance` to `0.0..=10.0` when it scores, so a value above 10 scores as 10, and a negative value scores as 0, which makes the record's salience 0.0. `dreamd recall --explain` shows the clamped factor.

**Fix:** Set the value on that line to a number from `0.0` to `10.0`, then rebuild the index as above.

### `duplicate_id: N line(s) …`

**What the reader does:** Readers of the log keep every copy, but the index holds one document per id, so recall returns one of them. An index that already holds the id keeps the copy it indexed, and a copy added later by hand is never indexed. A rebuilt index holds the last copy in the file. `dreamd forget <id>` removes every copy, and a pin applies to every copy.

**Fix:** Keep one line per id and delete the other copies. If a copy holds a different learning you want to keep, delete it and add it again with `append_node` or `POST /api/v1/learn`, which gives it a new id; do not write an id by hand. Then rebuild the index as above, so that it holds the copy you kept.

### `future_timestamp: N line(s) …`

**What the reader does:** Recall floors the record's age at 0, so it scores as if it had just been written until the clock reaches its timestamp. Until then it counts in the dream cycle's 7- and 30-day recurrence windows, and decay does not archive it until 90 days after that timestamp.

**Fix:** Set `timestamp` to when the learning happened, in RFC 3339 UTC (for example `2026-10-31T09:30:00Z`), then rebuild the index as above. Doctor flags a timestamp more than 24 hours ahead of this machine's clock; if it flags every recent line, check the clock first.

### `registry: parse error at line N`

**Symptom:** Besides the doctor line, `dreamd status` prints `(not registered)` for a project you registered, and the daemon answers every request with `500 {"error":"registry read failed"}`. `dreamd mcp` in daemon mode reports that as `daemon error 500 Internal Server Error: {"error":"registry read failed"}`. `registry: error  [WARNING: could not read …]` is the same failure for a file that is not UTF-8 or cannot be read.

**What the reader does:** `dreamd status` treats a registry it cannot parse as having no entries and does not print the error. The daemon re-reads `registry.toml` when the file changes; while it does not parse, every project lookup fails, nothing is cached, and the daemon serves no project. `dreamd init` and `dreamd init --uninstall-project` refuse and print the parse error with its line and column.

**Fix:** Edit the line doctor names. Each entry is a `[[projects]]` line followed by `root = "/absolute/path/to/project"`. Or delete `~/.agent/registry.toml` and run `dreamd init` in each project; `init` registers an existing `.agent/` without changing it. The daemon reads the fixed file on its next request; no restart is needed.

**Prevention:** Write learnings through `append_node` or `POST /api/v1/learn`, and change the registry with `dreamd init` and `dreamd init --uninstall-project`.

---

## Still stuck?

1. `npx -y dreamd-mcp doctor` — dream-cycle mode and index health; `dreamd status` for last cycle (cargo-installed binary)
2. [docs/ci.md](./ci.md) — reproduce CI gates locally
3. [GitHub Discussions](https://github.com/botzrDev/dreamd/discussions) for usage questions
4. [SECURITY.md](../SECURITY.md) for vulnerability reports (not public issues)
