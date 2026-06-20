# dreamd-mcp

Node shim for the dreamd MCP server. Downloads the right prebuilt binary for your OS/arch and starts the MCP server.

## Install

```sh
# 1. Scaffold .agent/ into your project
npx dreamd-mcp init

# 2. Point Claude Code, Cursor, or any MCP-aware harness at the server
npx dreamd-mcp
```

No Rust installation required. Supports Linux x86_64, macOS x86_64/aarch64.

## Running several agents at once

`npx dreamd-mcp` auto-connects to a shared daemon if one is running, and otherwise runs a standalone in-process server. Sequential use across tools is safe. If you point **several agents at the same project simultaneously**, start one shared daemon per machine with `npx dreamd-mcp watch` (or the native `dreamd watch`) so every agent routes through a single serialized writer. See the [project README](https://github.com/botzrDev/dreamd#running) for the full footprint and crash-safety notes.

## Override

Set `DREAMD_BIN=/path/to/dreamd` to skip download and use a local build.

## License

Apache-2.0
