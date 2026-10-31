# dreamd-mcp

**Agent memory you can trace and undo.** Every lesson cites the events it came from. `blame` shows which memories drove an answer. `forget` removes a bad memory and the lesson state built on it, with a receipt. `git revert` rolls back a whole learning cycle.

dreamd is local-first, long-term memory for AI agents, served over MCP to Claude Code, Cursor, Cline and other MCP-aware harnesses. Memory lives in `.agent/` in your repo as JSONL and Markdown and is committed with your code, so you review what agents learned the way you review code.

Source, docs and issues: **[github.com/botzrDev/dreamd](https://github.com/botzrDev/dreamd)**

```sh
npx -y dreamd-mcp setup   # scaffold .agent/ and wire your harness
npx -y dreamd-mcp         # MCP server (stdio): what the harness spawns
```

## Trace and remove a bad memory

Say an agent keeps repeating advice you know is wrong.

```sh
# 1. Find the memories behind the answer (ids, timestamps, skill_action, BM25, salience, score)
npx -y dreamd-mcp blame "deploy without tests" --explain

# 2. Read the lesson and the events it cites, and check the provenance ledger
cat .agent/semantic/LESSONS.md
npx -y dreamd-mcp doctor --provenance

# 3. Preview, then remove the bad event. Stop `dreamd watch` and your
#    harness's MCP server first: forget refuses while another writer holds the project.
npx -y dreamd-mcp forget evt_... --dry-run
npx -y dreamd-mcp forget evt_... --proof forget-receipt.json

# 4. Review the change like code
git diff .agent/
```

| Step | What it does |
|---|---|
| Cite | Every lesson in `.agent/semantic/LESSONS.md` cites the episodic events it was promoted from. The provenance ledger records which event fed which lesson and index entry |
| `blame <query>` | Lists the stored memories behind a recall. `--explain` adds the salience factors for each hit; `--json` prints one array |
| `doctor --provenance` | Recomputes the ledger's Merkle root and lists orphan, missing and corrupt edges. Read-only |
| `forget <event-id>` | Removes the event and the lesson state that names it. `--dry-run` writes nothing; `--proof <path>` writes a `forget-receipt/1.0` file |
| `git revert` | Each learning cycle commits its output as `dreamd: cycle YYYY-MM-DD`. Revert that commit with the daemon stopped, then run `npx -y dreamd-mcp doctor --repair` to rebuild the recall index |

After a `forget`, `doctor --provenance` reports the removed event's old ledger edges as orphans and exits 1. That is expected: the ledger is append-only, so it keeps the record that the event once existed.

**What dreamd does not do.** It does not detect or block poisoned writes, and it does not filter injected lesson content ([SECURITY.md](https://github.com/botzrDev/dreamd/blob/main/SECURITY.md)). `source_harness` is asserted by the caller, not verified. The provenance ledger and the forget receipt are unsigned. dreamd makes bad memory traceable and removable; it does not make it impossible.

## How agents learn

Agents write episodes (what happened, how painful it was, how important) through the MCP tool `append_node` and recall them with `search_nodes`; each hit carries its source harness. A learning cycle (`dreamd dream`) clusters episodes by `skill_action`, promotes a recurring pattern into a lesson in `LESSONS.md` that cites its events, and commits the result to your project's git repo (skipped if either file has uncommitted edits). Recall ranks episodes and lessons with BM25 × salience (age decay, pain, importance, recurrence). Preview the next cycle with `npx -y dreamd-mcp dream --dry`.

## Install

Requires a project root sentinel (`.git/`, `Cargo.toml`, `package.json`, or `pyproject.toml`), Node 18 or later, and `tar` on `PATH`. No Rust needed.

```sh
npx -y dreamd-mcp setup   # 1. scaffold .agent/ and wire your harness's MCP config
npx -y dreamd-mcp watch   # 2. shared daemon, recommended when several agents write
                          # 3. reload the harness: it now spawns `npx -y dreamd-mcp` itself
```

`setup` prompts when it has a TTY; in scripts pass `--yes` with `--harness claude|cursor|both|none`. It rewrites each MCP config as 2-space pretty-printed JSON, so an existing `.mcp.json` or `.cursor/mcp.json` can come back reformatted; other MCP servers in the file are kept. `npx -y dreamd-mcp init` scaffolds the store only, same as `setup --no-write-mcp`. Adapter quickstarts: [Claude Code](https://github.com/botzrDev/dreamd/tree/main/adapters/claude-code) · [Cursor](https://github.com/botzrDev/dreamd/tree/main/adapters/cursor) · [Cline](https://github.com/botzrDev/dreamd/tree/main/adapters/cline)

**Several agents at once.** `npx -y dreamd-mcp` connects to a shared daemon if one is running, and otherwise runs a standalone in-process server. Sequential use across tools is safe. To point several agents at the same project simultaneously, start one daemon per machine with `npx -y dreamd-mcp watch` so every write goes through a single serialized writer.

**Platforms.** Prebuilt binaries cover Linux x86_64 and macOS x86_64/aarch64 (see `manifest.json`). The binary is downloaded once from the matching GitHub release into `~/.cache/dreamd-mcp/<version>/`, and its SHA-256 is checked against `manifest.json` before every run. On any other platform the shim exits 1. The prebuilt binaries serve MCP over stdio only; the opt-in Streamable HTTP transport needs a source build ([docs/mcp-transports.md](https://github.com/botzrDev/dreamd/blob/main/docs/mcp-transports.md)).

**Windows.** This shim ships no Windows binary: on native Windows `npx -y dreamd-mcp` exits 1 and points you at WSL2. A `dreamd.exe` built from source runs learn, the learning cycle and the Tantivy index; start `dreamd watch` before `dreamd mcp` ([docs/windows.md](https://github.com/botzrDev/dreamd/blob/main/docs/windows.md)).

> **Leave `npx dreamd-mcp` floating; don't pin.** On a fresh spawn, npx re-resolves the `latest` dist-tag, so a floating config always starts the current version. A running MCP server or `dreamd watch` daemon keeps the version it started with until you restart it, and an offline run falls back to the last-cached binary.

## Commands

`npx -y dreamd-mcp` with no arguments starts the MCP server over stdio. When the first argument is one of these, the whole argument list goes to the native `dreamd` binary unchanged:

```text
archive, blame, doctor, dream, forget, init, mcp, memory, migrate, recall,
reset, salience-drift, score, service, setup, status, uninstall, update,
vectors, version, watch
```

Any other first argument is passed to `dreamd mcp` (so `npx -y dreamd-mcp --project-root /abs/path` works). A leading `--help` / `-h` or `--version` / `-V` is answered by the shim itself; `npx -y dreamd-mcp version` prints the native binary's build info.

## Update

```sh
npx -y dreamd-mcp update   # or: dreamd update
```

`update` stops this `$HOME`'s local `dreamd mcp` / `dreamd watch` processes, removes the socket and clears `~/.cache/dreamd-mcp`. Then reload your MCP harness so it drops the old process, and re-run `npx -y dreamd-mcp` to fetch the new binary. It never relaunches anything for you. `--dry-run` prints the plan and changes nothing; `--restart` makes the stop step explicit; `--quiet` / `-q` trims output. It does not touch `~/.npm/_npx`, and it does not replace a binary you built with `cargo install`.

## Uninstall / reset

`dreamd-mcp` is never installed globally, so `npm uninstall -g dreamd-mcp` is a no-op. There is no `dreamd reset --all`; use `dreamd uninstall`.

```sh
npx -y dreamd-mcp uninstall          # prompts for y when stdin is a tty
npx -y dreamd-mcp uninstall --yes    # no prompt; the form to use in a script
```

It stops this `$HOME`'s local `dreamd mcp` / `dreamd watch` processes, removes the daemon socket, unregisters the current project from the registry (skipped with a note outside a project root), and clears `~/.cache/dreamd-mcp` plus the dreamd-mcp entries under `~/.npm/_npx`. Without a tty and without `--yes` it exits 2 and changes nothing. A second run succeeds with nothing left to do.

| Flag | Effect |
|---|---|
| `--yes` | Skip the confirmation prompt. Required when stdin is not a tty. `--quiet` does not imply it |
| `--keep-caches` | Skip the cache clears |
| `--all-npx` | Wipe the **entire** `~/.npm/_npx` (every npx-cached package, not just dreamd), after a warning |
| `--quiet` / `-q` | Suppress non-essential output |

Left in place: `~/.agent/registry.toml`, `~/.agent/dreamd.log`, every project's `.agent/` store, and the login service (remove that with `dreamd service uninstall`). To wipe a project's store, delete `.agent/` and re-run `npx -y dreamd-mcp init` ([troubleshooting](https://github.com/botzrDev/dreamd/blob/main/docs/troubleshooting.md#how-do-i-reset-or-clear-memory)); back it up first if it has value.

**Then remove the client config entry.** `uninstall` does not edit harness configs. Delete the `dreamd` block from `.mcp.json`, Cursor settings or Cline `cline_mcp_settings.json` and reload the client, or the harness respawns dreamd next session.

### Manual fallback

Prefer `npx -y dreamd-mcp uninstall`: its stop step signals only processes that belong to this `$HOME` and this `~/.cache/dreamd-mcp`. If you still want the steps by hand, run these, then remove the client config entry as above:

```sh
# 1. Stop your dreamd servers and remove the socket. `-u "$(id -u)"` keeps the
#    signal inside your account; a bare `pkill -f` hits every user's servers.
pkill -u "$(id -u)" -f 'dreamd mcp'   || true
pkill -u "$(id -u)" -f 'dreamd watch' || true
rm -f ~/.agent/dreamd.sock

# 2. Unregister the project (from the project root). The store stays.
dreamd init --uninstall-project

# 3. Native binary cache
rm -rf ~/.cache/dreamd-mcp                  # macOS/Linux
#   Windows: Remove-Item -Recurse "$env:LOCALAPPDATA\dreamd-mcp\cache"

# 4. npx cache: delete only _npx dirs whose package.json names dreamd-mcp
for d in ~/.npm/_npx/*/; do
  [ -f "$d/package.json" ] && grep -q '"dreamd-mcp"' "$d/package.json" && rm -rf "$d"
done
#   Windows / WSL with Windows Node: same loop under "$LOCALAPPDATA/npm-cache/_npx"
```

> **Warning:** `pkill -u` stops at the user boundary, not the home boundary, so it also stops dreamd servers you run under another `$HOME` on the same account (a sandbox, a devcontainer). `rm -rf ~/.npm/_npx` deletes every npx-cached package, not just dreamd.

## Override (development only)

Set `DREAMD_BIN=/path/to/dreamd` to use a local build instead of the cached release binary. This skips SHA-256 verification, so you must also set `DREAMD_BIN_ALLOW_UNVERIFIED=1`; `DREAMD_BIN` on its own is refused. Never point production MCP configs at an unverified binary.

```sh
cargo install --path crates/dreamd-cli
export DREAMD_BIN=~/.cargo/bin/dreamd
export DREAMD_BIN_ALLOW_UNVERIFIED=1
npx -y dreamd-mcp
```

## Official MCP Registry

`server.json` holds the metadata for the [official MCP Registry](https://registry.modelcontextprotocol.io) entry `io.github.botzrDev/dreamd`. The registry serves metadata only and points at the npm package, so the matching version must already be public on npm.

```sh
# from packages/dreamd-mcp: non-mutating check, anyone (including CI) can run it
mcp-publisher validate server.json

# owner-only: must authenticate as a GitHub identity authorized for botzrDev
mcp-publisher login github
mcp-publisher publish server.json
curl "https://registry.modelcontextprotocol.io/v0.1/servers?search=io.github.botzrDev%2Fdreamd"
```

Publishing is deliberately not automated; no workflow holds registry credentials.

## Learn more

- [GUIDE.md](https://github.com/botzrDev/dreamd/blob/main/GUIDE.md): 20-minute walkthrough, including inspect and undo
- [docs/provenance.md](https://github.com/botzrDev/dreamd/blob/main/docs/provenance.md): provenance ledger format and what `doctor --provenance` checks
- [docs/observability.md](https://github.com/botzrDev/dreamd/blob/main/docs/observability.md): recall citations, `blame`, counterfactual recall and salience drift
- [SECURITY.md](https://github.com/botzrDev/dreamd/blob/main/SECURITY.md): threat model, including the lesson-injection surface
- [SPEC.md](https://github.com/botzrDev/dreamd/blob/main/SPEC.md): on-disk `.agent/` contract
- [docs/troubleshooting.md](https://github.com/botzrDev/dreamd/blob/main/docs/troubleshooting.md): common failures

## License

Apache-2.0
