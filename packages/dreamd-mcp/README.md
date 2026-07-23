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
> run falls back to the last-cached binary. A hard version pin (`dreamd-mcp@0.1.0-rc.3`)
> is the one form that never picks up new releases.

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
is therefore a no-op. There is **no** `dreamd reset --all` — use the steps below.

**For floating-npx users:** removing the MCP client config entry and reloading the
client is what actually stops dreamd. Clearing caches alone does not stop a running
client or daemon from respawning on the next harness launch.

### Step 0 — stop running processes

Quit or reload your MCP client (Claude Code, Cursor, Cline, …) so it stops spawning
`dreamd mcp`. Then stop any background daemon and remove the socket:

```sh
pkill -f 'dreamd mcp' || true
pkill -f 'dreamd watch' || true
rm -f ~/.agent/dreamd.sock
```

### Step 1 — remove the client config entry

Delete the `dreamd` MCP server block from your harness config (`.mcp.json`, Cursor
settings, Cline `cline_mcp_settings.json`, …) and reload the client. Until this
entry is gone, the harness will keep launching dreamd on the next session.

### Step 2 — clear caches (optional; force a clean re-download)

Clear **both** the npx shim cache and the native binary cache. Clearing only one is
a common cause of "it still runs the old version".

**npx shim cache — scoped to dreamd only (recommended):**

```sh
# macOS/Linux — delete only _npx dirs whose package.json names dreamd-mcp
for d in ~/.npm/_npx/*/; do
  [ -f "$d/package.json" ] && grep -q '"name"[[:space:]]*:[[:space:]]*"dreamd-mcp"' "$d/package.json" && rm -rf "$d"
done
# Windows / WSL with Windows Node — same pattern under:
#   "$LOCALAPPDATA/npm-cache/_npx"
```

> **Warning:** `rm -rf ~/.npm/_npx` deletes **every** npx-cached package on your
> machine, not just dreamd. Use the scoped loop above unless you intend a full npx reset.

**Native binary cache:**

```sh
rm -rf ~/.cache/dreamd-mcp                  # macOS/Linux
#   Windows: Remove-Item -Recurse "$env:LOCALAPPDATA\dreamd-mcp\cache"
```

### Step 3 — daemon leftovers (optional)

The user-scoped daemon home may still contain:

- `~/.agent/registry.toml` — project registry (remove a single project with
  `dreamd init --uninstall-project` from that project's root)
- `~/.agent/dreamd.log` — daemon log (safe to delete when nothing is running)

To wipe a project's memory store entirely, see [Full fresh store](../../docs/troubleshooting.md#how-do-i-reset-or-clear-memory) in the troubleshooting guide — delete `.agent/` and re-run `dreamd init`. That is destructive; back up first if the store has value.

## License

Apache-2.0
