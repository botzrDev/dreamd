# dreamd-mcp

Node shim for the [dreamd](https://github.com/botzrDev/dreamd) MCP server. Downloads the right prebuilt binary for your OS/arch and starts the MCP server over stdio.

## Install

Requires a project root sentinel (`.git/`, `Cargo.toml`, `package.json`, or `pyproject.toml`).

```sh
# 1. Scaffold .agent/ into your project
npx -y dreamd-mcp init

# 2. Start a shared daemon (recommended when multiple agents write)
npx -y dreamd-mcp watch

# 3. Point Claude Code, Cursor, or any MCP-aware harness at the MCP server
npx -y dreamd-mcp
```

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

> First run prompts once — press `y`, or use `npx -y dreamd-mcp`.

## Uninstall / reset

`dreamd-mcp` is never installed globally — it runs straight from the npx cache and
downloads the native binary into a per-version cache. `npm uninstall -g dreamd-mcp`
is therefore a no-op. There is no `dreamd reset --all` — use `dreamd uninstall`.

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
[Full fresh store](../../docs/troubleshooting.md#how-do-i-reset-or-clear-memory)
in the troubleshooting guide — delete `.agent/` and re-run `dreamd init`. That is
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
`~/.cache/dreamd-mcp`, then instructs you to re-run `npx -y dreamd-mcp` — the
floating npx spawn re-resolves `latest` and fetches the new binary.

| Flag | Effect |
|---|---|
| `--dry-run` | Print the current version and planned actions; change nothing |
| `--quiet` / `-q` | Suppress non-essential output |

`update` does not touch `~/.npm/_npx`. If you built from source with
`cargo install --path crates/dreamd-cli`, clearing the cache does not replace that
binary — rebuild it instead.

### Manual fallback

The same cleanup by hand, if you prefer explicit steps. First quit or reload your
MCP client so it stops spawning `dreamd mcp`, then:

```sh
# 1. Stop processes + remove the socket
pkill -f 'dreamd mcp' || true
pkill -f 'dreamd watch' || true
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

> **Warning:** `rm -rf ~/.npm/_npx` deletes **every** npx-cached package on your
> machine, not just dreamd. Use the scoped loop — or `dreamd uninstall`, which
> scopes by default — unless you intend a full npx reset.

## License

Apache-2.0
