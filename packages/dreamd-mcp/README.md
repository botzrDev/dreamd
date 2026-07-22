# dreamd-mcp

Node shim for the [dreamd](https://github.com/botzrDev/dreamd) MCP server. Downloads the right prebuilt binary for your OS/arch and starts the MCP server over stdio.

## Install

Requires a project root sentinel (`.git/`, `Cargo.toml`, `package.json`, or `pyproject.toml`).

```sh
# 1. Scaffold .agent/ into your project
npx dreamd-mcp@latest init

# 2. Start a shared daemon (recommended when multiple agents write)
npx dreamd-mcp@latest watch

# 3. Point Claude Code, Cursor, or any MCP-aware harness at the MCP server
npx dreamd-mcp@latest
```

> **Pin `@latest` (or an explicit version) in your MCP config.** A bare
> `npx dreamd-mcp` resolves once and then reuses whatever npx first cached —
> it never re-checks the registry, so it can serve an old version indefinitely.
> `npx dreamd-mcp@latest` (or `npx --yes dreamd-mcp@<version>`) forces a refresh.

No Rust installation required. Prebuilt binaries are available for **Linux x86_64** and **macOS x86_64/aarch64** (see `manifest.json`).

Adapter quickstarts: [Claude Code](https://github.com/botzrDev/dreamd/tree/main/adapters/claude-code) · [Cursor](https://github.com/botzrDev/dreamd/tree/main/adapters/cursor)

## Running several agents at once

`npx dreamd-mcp` auto-connects to a shared daemon if one is running, and otherwise runs a standalone in-process server. Sequential use across tools is safe. If you point **several agents at the same project simultaneously**, start one shared daemon per machine with `npx dreamd-mcp watch` (or the native `dreamd watch`) so every agent routes through a single serialized writer. See the [project README](https://github.com/botzrDev/dreamd#quick-start--30-seconds) for the full footprint and crash-safety notes.

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

Build from source:

```sh
cargo install --path crates/dreamd-cli
export DREAMD_BIN=~/.cargo/bin/dreamd
export DREAMD_BIN_ALLOW_UNVERIFIED=1
npx dreamd-mcp
```

## Uninstall / reset

`dreamd-mcp` is never installed globally — it runs straight from the npx cache and
downloads the native binary into a per-version cache. `npm uninstall -g dreamd-mcp`
is therefore a no-op. To fully remove it (or force a clean re-download), clear both
caches and drop the server entry from your MCP client config:

```sh
# npx package cache (macOS/Linux)
rm -rf ~/.npm/_npx
# npx package cache (Windows / WSL running Windows Node)
#   rm -rf "$LOCALAPPDATA/npm-cache/_npx"   (PowerShell: Remove-Item -Recurse "$env:LOCALAPPDATA\npm-cache\_npx")

# native binary cache
rm -rf ~/.cache/dreamd-mcp                  # macOS/Linux
#   Windows: Remove-Item -Recurse "$env:LOCALAPPDATA\dreamd-mcp\cache"
```

Then remove the `dreamd` MCP server from your client config (Claude Code, Cursor, …).
Clearing only one cache is a common cause of "it still runs the old version" — clear
both.

## License

Apache-2.0
