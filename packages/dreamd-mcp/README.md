# dreamd-mcp

Node shim for the dreamd MCP server. Downloads the right prebuilt binary for your OS/arch and starts the MCP server.

## Install

```sh
# 1. Scaffold .agent/ into your project
npx dreamd-mcp@0.1.0-rc.2 init

# 2. Point Claude Code, Cursor, or any MCP-aware harness at the server
npx dreamd-mcp@0.1.0-rc.2
```

No Rust installation required. Prebuilt binaries are available for **Linux x86_64** and **macOS x86_64/aarch64** (see `manifest.json`).

## Running several agents at once

`npx dreamd-mcp` auto-connects to a shared daemon if one is running, and otherwise runs a standalone in-process server. Sequential use across tools is safe. If you point **several agents at the same project simultaneously**, start one shared daemon per machine with `npx dreamd-mcp watch` (or the native `dreamd watch`) so every agent routes through a single serialized writer. See the [project README](https://github.com/botzrDev/dreamd#running) for the full footprint and crash-safety notes.

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

## License

Apache-2.0
