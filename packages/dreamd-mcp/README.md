# dreamd-mcp

**Agent memory you can audit. When a memory goes bad, trace it to its source, see what it fed, and remove it.**

dreamd is local-first, long-term memory for AI agents, served over MCP to Claude Code, Cursor, Cline and other MCP-aware harnesses. Every memory is a line in a file in your repo, every lesson your agents learn cites the events it came from, and each dream cycle commits its lessons to git. When an agent acts on something wrong, you can find the memory that caused it, see which lessons it fed, and take it out, with a receipt.

Source, docs and issues: **[github.com/botzrDev/dreamd](https://github.com/botzrDev/dreamd)**

This package is the Node shim for the dreamd MCP server. It downloads the right prebuilt binary for your OS and architecture, checks its SHA-256, and starts the MCP server over stdio.

```sh
npx -y dreamd-mcp setup   # scaffold .agent/ and wire your harness
npx -y dreamd-mcp         # MCP server (stdio): what the harness spawns
```

## What you can audit

| You need to | dreamd gives you |
|---|---|
| See what drove an answer | `npx -y dreamd-mcp blame <query>` lists the stored memories behind a recall, with ids, timestamps, `skill_action`, BM25, salience and total score. `--explain` prints the salience factors for each hit |
| Know where a lesson came from | Every lesson in `.agent/semantic/LESSONS.md` cites the episodic events it was promoted from, and the provenance ledger records which event fed which lesson and index entry |
| Check the record is consistent | `npx -y dreamd-mcp doctor --provenance` recomputes the ledger's Merkle root and lists orphan, missing and corrupt edges. Read-only |
| Remove a bad memory | `npx -y dreamd-mcp forget <event-id>` removes the event and the lesson state that names it. `--dry-run` shows what would change; `--proof <path>` writes a `forget-receipt/1.0` file |
| Review what agents learned | `.agent/` is committed with your code, and each dream cycle commits its output as `dreamd: cycle YYYY-MM-DD`. Review new lessons with `git diff` or in a pull request, and undo a cycle with `git revert` |
| Read everything yourself | Plain JSONL and Markdown. `cat`, `grep` and `git log` work on every byte |

**What dreamd does not do.** It does not detect or block poisoned writes, and it does not filter injected lesson content ([SECURITY.md](https://github.com/botzrDev/dreamd/blob/main/SECURITY.md)). `source_harness` is asserted by the caller, not verified. The provenance ledger and the forget receipt are unsigned. dreamd makes bad memory traceable and removable; it does not make it impossible.

## The audit loop

Say an agent keeps repeating advice you know is wrong.

```sh
# 1. Find the memories behind it
npx -y dreamd-mcp blame "deploy without tests" --explain

# 2. Check the lesson it came from and what that lesson cites
cat .agent/semantic/LESSONS.md
npx -y dreamd-mcp doctor --provenance

# 3. Preview, then remove the bad event (stop `dreamd watch` first; forget refuses while it runs)
npx -y dreamd-mcp forget evt_... --dry-run
npx -y dreamd-mcp forget evt_... --proof forget-receipt.json

# 4. Review the change like code
git diff .agent/
```

After a `forget`, `doctor --provenance` reports the removed event's old ledger edges as orphans and exits 1. That is expected: the ledger is append-only, so it keeps the record that the event once existed.

To undo a whole dream cycle instead, `git revert` its `dreamd: cycle` commit with the daemon stopped, then run `npx -y dreamd-mcp doctor --repair` to rebuild the recall index.

## Install

Requires a project root sentinel (`.git/`, `Cargo.toml`, `package.json`, or `pyproject.toml`).

```sh
# 1. Scaffold .agent/ and wire your harness's MCP config
npx -y dreamd-mcp setup

# 2. Start a shared daemon (recommended when multiple agents write)
npx -y dreamd-mcp watch

# 3. Reload the harness: it now spawns the MCP server itself
npx -y dreamd-mcp
```

The prebuilt binaries this shim downloads serve MCP over stdio only. The opt-in Streamable HTTP transport (`dreamd mcp --bind`, unauthenticated) needs a source build with `--features mcp-http`; see [docs/mcp-transports.md](https://github.com/botzrDev/dreamd/blob/main/docs/mcp-transports.md).

`setup` prompts when it has a TTY; in scripts pass `--yes` with `--harness claude|cursor|both|none`. It writes each MCP config as 2-space pretty-printed JSON with a trailing newline and does **not** preserve your original formatting, so an existing `.mcp.json` or `.cursor/mcp.json` can come back reformatted. Other MCP servers in the file are kept. `npx -y dreamd-mcp init` remains the scaffold-only primitive (store, no harness config), same as `setup --no-write-mcp`.

> **Leave `npx dreamd-mcp` floating; don't pin.** On a fresh spawn, npx
> re-resolves the `latest` dist-tag from the registry, so a floating config always
> starts the current version. Two caveats: a **running** MCP server or `dreamd watch`
> daemon keeps the version it started with until you restart it, and an **offline**
> run falls back to the last-cached binary. A hard version pin
> (`dreamd-mcp@<version>`) is the one form that never picks up new releases.

No Rust installation required; the shim needs Node 18 or later and `tar` on `PATH`. Prebuilt binaries are available for **Linux x86_64** and **macOS x86_64/aarch64** (see `manifest.json`). The binary is downloaded once from the matching GitHub release into `~/.cache/dreamd-mcp/<version>/` and its SHA-256 is checked against `manifest.json` before every run. On any other platform the shim exits 1.

**This shim ships no Windows binary.** On native Windows `npx -y dreamd-mcp` exits 1 and points you at WSL2. A `dreamd.exe` built from source (or taken from the `windows-x86_64.tar.gz` asset of a GitHub release, when that release has one) runs learn, the dream cycle, and the Tantivy index. `dreamd watch` serves loopback TCP with a bearer token. Start that daemon before `dreamd mcp`. The directory flush follows the Unix call order and is not a power-loss proof ([docs/windows.md](https://github.com/botzrDev/dreamd/blob/main/docs/windows.md)).

## How agents learn

Agents write episodes (what happened, how painful it was, how important) through the MCP tool `append_node`, and recall them with `search_nodes`; each hit carries its source harness. The **dream cycle** clusters episodes by `skill_action` and promotes a recurring pattern into a lesson in `LESSONS.md` that cites the events it came from. Recall ranks episodes and lessons with BM25 × salience (age decay, pain, importance, recurrence). Preview the next cycle with `npx -y dreamd-mcp dream --dry`.

## Commands

`npx -y dreamd-mcp` with no arguments starts the MCP server over stdio. When the first argument is one of the commands below, the whole argument list is forwarded to the native `dreamd` binary unchanged:

```text
archive, blame, doctor, dream, forget, init, mcp, memory, migrate, recall,
reset, salience-drift, score, service, setup, status, uninstall, update,
vectors, version, watch
```

That is every `dreamd` subcommand. Any other first argument is passed to `dreamd mcp` (so `npx -y dreamd-mcp --project-root /abs/path` works). A leading `--help` / `-h` or `--version` / `-V` is answered by the shim itself; `npx -y dreamd-mcp version` prints the native binary's build info.

Adapter quickstarts: [Claude Code](https://github.com/botzrDev/dreamd/tree/main/adapters/claude-code) · [Cursor](https://github.com/botzrDev/dreamd/tree/main/adapters/cursor) · [Cline](https://github.com/botzrDev/dreamd/tree/main/adapters/cline)

## Running several agents at once

`npx -y dreamd-mcp` auto-connects to a shared daemon if one is running, and otherwise runs a standalone in-process server. Sequential use across tools is safe. If you point **several agents at the same project simultaneously**, start one shared daemon per machine with `npx -y dreamd-mcp watch` (or the native `dreamd watch`) so every agent routes through a single serialized writer. See the [project README](https://github.com/botzrDev/dreamd#readme) for the full footprint and crash-safety notes.

## Learn more

- [GUIDE.md](https://github.com/botzrDev/dreamd/blob/main/GUIDE.md): 20-minute walkthrough, including inspect and undo
- [docs/provenance.md](https://github.com/botzrDev/dreamd/blob/main/docs/provenance.md): provenance ledger format and what `doctor --provenance` checks
- [docs/observability.md](https://github.com/botzrDev/dreamd/blob/main/docs/observability.md): recall citations, `blame`, counterfactual recall and salience drift
- [SECURITY.md](https://github.com/botzrDev/dreamd/blob/main/SECURITY.md): threat model, including the lesson-injection surface
- [SPEC.md](https://github.com/botzrDev/dreamd/blob/main/SPEC.md): on-disk `.agent/` contract
- [docs/troubleshooting.md](https://github.com/botzrDev/dreamd/blob/main/docs/troubleshooting.md): common failures

## Official MCP Registry

`server.json` holds the metadata for the [official MCP Registry](https://registry.modelcontextprotocol.io) entry `io.github.botzrDev/dreamd`. The registry serves metadata only. It points at the npm package, so the matching version must already be public on npm before publishing.

Anyone (including CI) can check the metadata. Validation is non-mutating: it neither authenticates nor writes to the registry.

```sh
# from packages/dreamd-mcp
mcp-publisher validate server.json
```

Publication is owner-only. `mcp-publisher login github` must authenticate as a GitHub identity authorized for the `botzrDev` namespace. The registry derives the `io.github.botzrDev/*` namespace from that identity and rejects the publish otherwise.

```sh
mcp-publisher login github
mcp-publisher publish server.json
curl "https://registry.modelcontextprotocol.io/v0.1/servers?search=io.github.botzrDev%2Fdreamd"
```

The `curl` query is read-only and confirms the entry is live. Publishing is deliberately not automated in CI; no workflow holds registry credentials.

## Override (development only)

Set `DREAMD_BIN=/path/to/dreamd` to skip download and use a local build instead of the cached release binary. Because this bypasses sha256 verification, you must also set `DREAMD_BIN_ALLOW_UNVERIFIED=1` to confirm. `DREAMD_BIN` on its own is refused.

**Warning:** when `DREAMD_BIN` is set, sha256 verification is skipped. Use this only for local development. Never point production MCP configs at an unverified binary.

Build from source (Linux, macOS, or Windows):

```sh
cargo install --path crates/dreamd-cli
export DREAMD_BIN=~/.cargo/bin/dreamd
export DREAMD_BIN_ALLOW_UNVERIFIED=1
npx -y dreamd-mcp
```

> First run prints a local-only privacy disclosure. `setup` prompts for harness choice when it has a TTY.

## Uninstall / reset

`dreamd-mcp` is never installed globally. It runs straight from the npx cache and
downloads the native binary into a per-version cache, so `npm uninstall -g dreamd-mcp`
is a no-op. There is no `dreamd reset --all`; use `dreamd uninstall`.

> **Order:** run uninstall (or the manual cleanup steps), then remove the `dreamd`
> block from your MCP client config, then reload the client.

### Uninstall

```sh
npx -y dreamd-mcp uninstall          # prompts for y when stdin is a tty
npx -y dreamd-mcp uninstall --yes    # no prompt; the form to use in a script
```

One command: stops local `dreamd mcp` / `dreamd watch` processes, removes the
daemon socket, unregisters the current project from the registry (skipped with a
benign note when run outside a project root), and clears the native binary cache
(`~/.cache/dreamd-mcp`) plus the dreamd-mcp-scoped entries under `~/.npm/_npx`.
On a tty it prints that plan and waits for `y`. `--yes` skips the wait. Without
a tty and without `--yes` it exits 2 and changes nothing.
Safe to run twice: a second run succeeds with nothing left to do.

| Flag | Effect |
|---|---|
| `--yes` | Skip the confirmation prompt. Required when stdin is not a tty. `--quiet` does not imply it. |
| `--keep-caches` | Skip the cache clears |
| `--all-npx` | Loud: wipe the **entire** `~/.npm/_npx` (every npx-cached package, not just dreamd). Prints a warning before deleting. |
| `--quiet` / `-q` | Suppress non-essential output |

Left in place: `~/.agent/registry.toml`, `~/.agent/dreamd.log`, and every
project's `.agent/` memory store.
The systemd unit, LaunchAgent, or scheduled task stays installed; removing it is `dreamd service uninstall`.
To wipe a project's store entirely, see
[Full fresh store](https://github.com/botzrDev/dreamd/blob/main/docs/troubleshooting.md#how-do-i-reset-or-clear-memory)
in the troubleshooting guide: delete `.agent/` and re-run `npx -y dreamd-mcp init`. That is
destructive; back up first if the store has value.

**Then remove the client config entry.** Delete the `dreamd` MCP server block from
your harness config (`.mcp.json`, Cursor settings, Cline
`cline_mcp_settings.json`, …) and reload the client. Until that entry is gone, the
harness keeps respawning dreamd on the next session; `uninstall` does not edit
harness configs.

### Update

```sh
npx -y dreamd-mcp update        # or: dreamd update
```

Prints the current version, stops local servers, removes the socket, and clears
`~/.cache/dreamd-mcp`, then prints the **restart contract**:

1. **Stop** local `dreamd mcp` / `dreamd watch` if running. `update` does this
   for you on every non-dry run, and reports whether anything matched.
2. **Reload your MCP harness** (Claude Code, Cursor, …) so it drops the old
   binary. Until the harness reloads, it keeps the running process alive and you
   stay on the old version.
3. **Re-run `npx -y dreamd-mcp`.** The floating npx spawn re-resolves `latest`
   and fetches the new binary. Keep the pin floating; a hard version pin never
   picks up new releases.

`update` never relaunches anything for you: no OS service, no auto-respawn.

| Flag | Effect |
|---|---|
| `--dry-run` | Print the current version and the restart contract as a plan; change nothing |
| `--restart` | Explicitly stop local `dreamd mcp` / `dreamd watch` and say so. Same stop that already runs by default; the flag makes the step loud, and is a no-op if nothing is running |
| `--quiet` / `-q` | Suppress non-essential output. Version lines and a one-line reload + re-run reminder still print |

`update` does not touch `~/.npm/_npx`. If you built from source with
`cargo install --path crates/dreamd-cli`, clearing the cache does not replace that
binary; rebuild it instead.

### Manual fallback

**Use `npx -y dreamd-mcp uninstall` instead of the recipe below.** It does all
four steps in one command. (`dreamd update` covers steps 1 and 3 only: it never
unregisters the project and never touches `~/.npm/_npx`.)

Both commands' stop step is scoped: they signal only the `dreamd mcp` /
`dreamd watch` processes attributable to *this* `$HOME` and *this*
`~/.cache/dreamd-mcp`. Anything serving another home, another sandbox, or
another user on the box is left running and named on stderr. A hand-run `pkill`
has no such scope.

If you still want the steps by hand, run the cleanup below (stop processes,
remove the socket, unregister the project, clear caches), **then** remove the
`dreamd` block from your MCP client config and reload the client, in the same
order as [Uninstall](#uninstall):

```sh
# 1. Stop your dreamd servers + remove the socket.
#    `-u "$(id -u)"` keeps the signal inside your own account. Never run a bare
#    `pkill -f` for these patterns: it is machine-global and SIGTERMs every
#    matching process on the box, including other users' servers.
pkill -u "$(id -u)" -f 'dreamd mcp'   || true
pkill -u "$(id -u)" -f 'dreamd watch' || true
rm -f ~/.agent/dreamd.sock

# 2. Unregister the project from the registry (run from the project root)
dreamd init --uninstall-project
# Drops the registry entry only. The store stays. A later `dreamd init` puts the entry back.

# 3. Native binary cache
rm -rf ~/.cache/dreamd-mcp                  # macOS/Linux
#   Windows: Remove-Item -Recurse "$env:LOCALAPPDATA\dreamd-mcp\cache"

# 4. npx shim cache: delete only _npx dirs whose package.json references
#    dreamd-mcp. npm writes these manifests without a "name" field; the
#    requested package appears as a dependencies key, e.g.
#    {"dependencies":{"dreamd-mcp":"^0.1.0-rc.6"}}, so match the quoted
#    package key, and check each manifest before deleting anything.
for d in ~/.npm/_npx/*/; do
  [ -f "$d/package.json" ] && grep -q '"dreamd-mcp"' "$d/package.json" && rm -rf "$d"
done
# Windows / WSL with Windows Node: same pattern under:
#   "$LOCALAPPDATA/npm-cache/_npx"
```

> **Warning:** `rm -rf ~/.npm/_npx` deletes **every** npx-cached package on your
> machine, not just dreamd. Use the scoped loop, or `dreamd uninstall`, which
> scopes by default, unless you intend a full npx reset.

> **Warning:** step 1 is still coarser than `dreamd uninstall`. `pkill -u` stops
> at the user boundary, not the home boundary, so if you run dreamd under more
> than one `$HOME` on the same account (a sandbox, a devcontainer, a test rig)
> it stops those servers too. `dreamd uninstall` / `dreamd update` signal only
> what belongs to the `$HOME` you run them under.

## License

Apache-2.0
