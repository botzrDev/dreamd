# dreamd-mcp

Node shim for the [dreamd](https://github.com/botzrDev/dreamd) MCP server. Downloads the right prebuilt binary for your OS/arch and starts the MCP server over stdio.

## Install

Requires a project root sentinel (`.git/`, `Cargo.toml`, `package.json`, or `pyproject.toml`).

```sh
# 1. Scaffold .agent/ and wire your harness's MCP config
npx -y dreamd-mcp setup

# 2. Start a shared daemon (recommended when multiple agents write)
npx -y dreamd-mcp watch

# 3. Reload the harness — it now spawns the MCP server itself
npx -y dreamd-mcp
```

`setup` prompts when it has a TTY; in scripts pass `--yes` with `--harness claude|cursor|both|none`. It writes each MCP config as 2-space pretty-printed JSON with a trailing newline and does **not** preserve your original formatting, so an existing `.mcp.json` / `.cursor/mcp.json` can come back reformatted — other MCP servers in the file are kept. `npx -y dreamd-mcp init` remains the scaffold-only primitive (store, no harness config), same as `setup --no-write-mcp`.

> **Leave `npx dreamd-mcp` floating — don't pin.** On a fresh spawn, npx
> re-resolves the `latest` dist-tag from the registry, so a floating config always
> starts the current version. Two caveats: a **running** MCP server or `dreamd watch`
> daemon keeps the version it started with until you restart it, and an **offline**
> run falls back to the last-cached binary. A hard version pin
> (`dreamd-mcp@0.1.0-rc.3`) is the one form that never picks up new releases.

No Rust installation required. Prebuilt binaries are available for **Linux x86_64** and **macOS x86_64/aarch64** (see `manifest.json`). **Native Windows is out of scope for v0.1** — use WSL2 or a Linux/macOS host (Windows support is planned for v0.1.1).

Adapter quickstarts: [Claude Code](https://github.com/botzrDev/dreamd/tree/main/adapters/claude-code) · [Cursor](https://github.com/botzrDev/dreamd/tree/main/adapters/cursor)

## Running several agents at once

`npx -y dreamd-mcp` auto-connects to a shared daemon if one is running, and otherwise runs a standalone in-process server. Sequential use across tools is safe. If you point **several agents at the same project simultaneously**, start one shared daemon per machine with `npx -y dreamd-mcp watch` (or the native `dreamd watch`) so every agent routes through a single serialized writer. See the [project README](https://github.com/botzrDev/dreamd#quick-start--30-seconds) for the full footprint and crash-safety notes.

## Learn more

- [GUIDE.md](https://github.com/botzrDev/dreamd/blob/main/GUIDE.md) — 20-minute tutorial walkthrough
- [SPEC.md](https://github.com/botzrDev/dreamd/blob/main/SPEC.md) — on-disk `.agent/` contract
- [docs/troubleshooting.md](https://github.com/botzrDev/dreamd/blob/main/docs/troubleshooting.md) — common failures

## Official MCP Registry

`server.json` holds the metadata for the [official MCP Registry](https://registry.modelcontextprotocol.io) entry `io.github.botzrDev/dreamd`. The registry serves metadata only — it points at the npm package, so the matching version must already be public on npm before publishing.

Anyone (including CI) can check the metadata. Validation is non-mutating: it neither authenticates nor writes to the registry.

```sh
# from packages/dreamd-mcp
mcp-publisher validate server.json
```

Publication is owner-only. `mcp-publisher login github` must authenticate as a GitHub identity authorized for the `botzrDev` namespace — the registry derives the `io.github.botzrDev/*` namespace from that identity and rejects the publish otherwise.

```sh
mcp-publisher login github
mcp-publisher publish server.json
curl "https://registry.modelcontextprotocol.io/v0.1/servers?search=io.github.botzrDev%2Fdreamd"
```

The `curl` query is read-only and confirms the entry is live. Publishing is deliberately not automated in CI — no workflow holds registry credentials.

## Override (development only)

Set `DREAMD_BIN=/path/to/dreamd` to skip download and use a local build instead of the cached release binary. Because this bypasses sha256 verification, you must also set `DREAMD_BIN_ALLOW_UNVERIFIED=1` to confirm — `DREAMD_BIN` on its own is refused.

**Warning:** when `DREAMD_BIN` is set, sha256 verification is skipped. Use this only for local development — never point production MCP configs at an unverified binary.

Build from source (Linux / macOS only — native Windows is out of scope for v0.1):

```sh
cargo install --path crates/dreamd-cli
export DREAMD_BIN=~/.cargo/bin/dreamd
export DREAMD_BIN_ALLOW_UNVERIFIED=1
npx -y dreamd-mcp
```

> First run prints a local-only privacy disclosure. `setup` prompts for harness choice when it has a TTY.

## Uninstall / reset

`dreamd-mcp` is never installed globally — it runs straight from the npx cache and
downloads the native binary into a per-version cache. `npm uninstall -g dreamd-mcp`
is therefore a no-op. There is no `dreamd reset --all` — use `dreamd uninstall`.

> **Order:** run uninstall (or the manual cleanup steps) → remove the `dreamd`
> block from your MCP client config → reload the client.

### Uninstall

```sh
npx -y dreamd-mcp uninstall     # or, with a native binary: dreamd uninstall
```

One command: stops local `dreamd mcp` / `dreamd watch` processes, removes the
daemon socket, unregisters the current project from the registry (skipped with a
benign note when run outside a project root), and clears the native binary cache
(`~/.cache/dreamd-mcp`) plus the dreamd-mcp-scoped entries under `~/.npm/_npx`.
Safe to run twice — a second run succeeds with nothing left to do.

| Flag | Effect |
|---|---|
| `--keep-caches` | Skip the cache clears |
| `--all-npx` | Loud: wipe the **entire** `~/.npm/_npx` (every npx-cached package, not just dreamd). Prints a warning before deleting. |
| `--quiet` / `-q` | Suppress non-essential output |

Left in place: `~/.agent/registry.toml`, `~/.agent/dreamd.log`, and every
project's `.agent/` memory store. To wipe a project's store entirely, see
[Full fresh store](https://github.com/botzrDev/dreamd/blob/main/docs/troubleshooting.md#how-do-i-reset-or-clear-memory)
in the troubleshooting guide — delete `.agent/` and re-run `npx -y dreamd-mcp init`. That is
destructive; back up first if the store has value.

**Then remove the client config entry.** Delete the `dreamd` MCP server block from
your harness config (`.mcp.json`, Cursor settings, Cline
`cline_mcp_settings.json`, …) and reload the client. Until that entry is gone, the
harness keeps respawning dreamd on the next session — `uninstall` does not edit
harness configs.

### Update

```sh
npx -y dreamd-mcp update        # or: dreamd update
```

Prints the current version, stops local servers, removes the socket, and clears
`~/.cache/dreamd-mcp`, then prints the **restart contract**:

1. **Stop** local `dreamd mcp` / `dreamd watch` if running — `update` does this
   for you on every non-dry run, and reports whether anything matched.
2. **Reload your MCP harness** (Claude Code, Cursor, …) so it drops the old
   binary. Until the harness reloads, it keeps the running process alive and you
   stay on the old version.
3. **Re-run `npx -y dreamd-mcp`** — the floating npx spawn re-resolves `latest`
   and fetches the new binary. Keep the pin floating; a hard version pin never
   picks up new releases.

`update` never relaunches anything for you — no OS service, no auto-respawn.

| Flag | Effect |
|---|---|
| `--dry-run` | Print the current version and the restart contract as a plan; change nothing |
| `--restart` | Explicitly stop local `dreamd mcp` / `dreamd watch` and say so. Same stop that already runs by default — the flag makes the step loud, and is a no-op if nothing is running |
| `--quiet` / `-q` | Suppress non-essential output. Version lines and a one-line reload + re-run reminder still print |

`update` does not touch `~/.npm/_npx`. If you built from source with
`cargo install --path crates/dreamd-cli`, clearing the cache does not replace that
binary — rebuild it instead.

### Manual fallback

The same cleanup by hand, if you prefer explicit steps. Follow the same order as
the command above: perform the dreamd cleanup first, then remove the MCP client
configuration entry and reload the client. If the client respawns dreamd while
you work through these steps, finish removing the configuration entry before
reloading it.
**Use `npx -y dreamd-mcp uninstall` instead of the recipe below** — it does all
four steps in one command. (`dreamd update` covers steps 1 and 3 only: it never
unregisters the project and never touches `~/.npm/_npx`.)

Both commands' stop step is scoped: they signal only the `dreamd mcp` /
`dreamd watch` processes attributable to *this* `$HOME` and *this*
`~/.cache/dreamd-mcp`. Anything serving another home, another sandbox, or
another user on the box is left running and named on stderr. A hand-run `pkill`
has no such scope.

If you still want the steps by hand, run the cleanup below (stop processes,
remove the socket, unregister the project, clear caches), **then** remove the
`dreamd` block from your MCP client config and reload the client — the same
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

# 3. Native binary cache
rm -rf ~/.cache/dreamd-mcp                  # macOS/Linux
#   Windows: Remove-Item -Recurse "$env:LOCALAPPDATA\dreamd-mcp\cache"

# 4. npx shim cache — delete only _npx dirs whose package.json references
#    dreamd-mcp. npm writes these manifests without a "name" field — the
#    requested package appears as a dependencies key, e.g.
#    {"dependencies":{"dreamd-mcp":"^0.1.0-rc.6"}} — so match the quoted
#    package key, and check each manifest before deleting anything.
for d in ~/.npm/_npx/*/; do
  [ -f "$d/package.json" ] && grep -q '"dreamd-mcp"' "$d/package.json" && rm -rf "$d"
done
# Windows / WSL with Windows Node — same pattern under:
#   "$LOCALAPPDATA/npm-cache/_npx"
```

After cleanup, remove the `dreamd` entry from the MCP client configuration and
reload the client. Until that entry is gone, a later client session can respawn
`dreamd mcp`.

> **Warning:** `rm -rf ~/.npm/_npx` deletes **every** npx-cached package on your
> machine, not just dreamd. Use the scoped loop — or `dreamd uninstall`, which
> scopes by default — unless you intend a full npx reset.

> **Warning:** step 1 is still coarser than `dreamd uninstall`. `pkill -u` stops
> at the user boundary, not the home boundary, so if you run dreamd under more
> than one `$HOME` on the same account — a sandbox, a devcontainer, a test rig —
> it stops those servers too. `dreamd uninstall` / `dreamd update` signal only
> what belongs to the `$HOME` you run them under.

## License

Apache-2.0
