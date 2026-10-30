# Troubleshooting

Symptom → cause → fix → prevention. For deeper reference see [http-api.md](./http-api.md), [configuration.md](./configuration.md), and [SECURITY.md](../SECURITY.md).

Commands below use the cargo-installed `dreamd` binary. On the npm path the shim does **not** put `dreamd` on `PATH` — use `npx -y dreamd-mcp <cmd>` for `init`, `setup`, `watch`, `doctor`, `dream`, `reset`, `service`, `uninstall`, `update`, `version`, `mcp`, `blame`, `salience-drift`, `memory`, `forget`, and `vectors`. The shim does **not** forward `status`, `recall`, `score`, `archive`, or `migrate` (an unlisted first token is passed to `dreamd mcp` as an argument and fails there); those need `cargo install --path crates/dreamd-cli`.

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

## MCP server won't connect

**Symptom:** Harness shows dreamd MCP disconnected, or `dreamd mcp` exits right after starting.

**Cause (common):**

1. `npx dreamd-mcp` not installed / wrong package name (`dreamd-mcp`, not scoped)
2. No `.agent/` in the project — server boots empty backend
3. Daemon expected but not running (Remote / daemon proxy)
4. Wrong `DREAMD_SOCK` override pointing at a dead path (`dreamd watch` itself ignores `DREAMD_SOCK` and always binds `$HOME/.agent/dreamd.sock`). A relative value is refused: `dreamd mcp: DREAMD_SOCK is not an absolute path: …`, exit 1
5. `dream_cycle_mode = "auto"` in `config.toml` — `dreamd mcp` and `dreamd watch` both exit 1 on it (see [configuration.md](./configuration.md#dream_cycle_mode))
6. Native Windows — the npx shim has no Windows binary and exits 1. Build `dreamd` and start `dreamd watch` first (see [windows.md](./windows.md))

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

**Cause:** a crash left `.dreamd/stage/` and/or `dream_in_progress.wal`.

**Fix:** Restart the daemon or run any command that triggers WAL recovery:

```bash
npx -y dreamd-mcp watch    # recovers on startup before serving
dreamd status              # last_dream_cycle (cargo-installed `dreamd`; npm shim does not forward `status`)
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

## How do I reset or clear memory?

| Goal | Command |
|---|---|
| Clear session scratchpad | `dreamd reset workspace` (asks first; `--yes` skips the prompt and is required when stdin is not a tty) |
| Remove one memory | `dreamd forget <event id>` — stop the daemon first; `--dry-run` previews. Find the id with `dreamd recall <query>` |
| Uninstall dreamd (stop servers, unregister project, clear caches; keeps `.agent/` stores) | `dreamd uninstall` — flags and details: [packages/dreamd-mcp/README.md](../packages/dreamd-mcp/README.md#uninstall--reset) |
| Update to the latest release | `dreamd update`, then re-run `npx -y dreamd-mcp` to fetch the new binary |
| Remove project from daemon registry only (keep everything else) | `dreamd init --uninstall-project`. The store stays. Run `dreamd init` again to put the registry entry back. |
| Remove the login service (systemd unit / LaunchAgent / scheduled task) | `dreamd service uninstall` — see [install.md](./install.md#removing-the-service) |
| Wipe episodic log | Stop the daemon, truncate `.agent/episodic/AGENT_LEARNINGS.jsonl`, then rebuild the index with `dreamd doctor --repair` (or use *Full fresh store* below for a clean slate) |
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

**Symptom:** `dreamd: no .agent/ store found in … or any parent directory — run \`dreamd init\` first.` (`doctor`, `dream`), `dreamd: error — no .agent/ directory found. Run \`dreamd init\` first.` (`recall`, `migrate`, `service install`, …), `dreamd watch: no project root found from …`, or MCP `coordinator unavailable: no agent root found`. All but the MCP one exit 2.

**Cause:** Project never initialized, or CWD is not inside a project with `.agent/`.

**Fix:**

```bash
cd ~/your-project    # must contain .git/, Cargo.toml, package.json, or pyproject.toml
dreamd init
```

For Cursor global MCP config, pass `--project-root /absolute/path/to/project` (see [adapters/cursor/README.md](../adapters/cursor/README.md)).

**Prevention:** Run `dreamd init` once per repo; commit `.agent/` (except `.agent/.dreamd/` which is gitignored).

---

## Recall returns no results

**Symptom:** `search_nodes` or `GET /api/v1/recall` returns `{"results":[]}`.

**Causes:**

| Cause | Check |
|---|---|
| Empty store | `wc -l .agent/episodic/AGENT_LEARNINGS.jsonl` |
| Read-after-write window | Wait up to 5 s after append (index commit cadence) |
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

## Still stuck?

1. `npx -y dreamd-mcp doctor` — dream-cycle mode and index health; `dreamd status` for last cycle (cargo-installed binary)
2. [docs/ci.md](./ci.md) — reproduce CI gates locally
3. [GitHub Discussions](https://github.com/botzrDev/dreamd/discussions) for usage questions
4. [SECURITY.md](../SECURITY.md) for vulnerability reports (not public issues)
