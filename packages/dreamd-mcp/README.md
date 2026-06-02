# @dataprime1/dreamd-mcp

Node shim for the dreamd MCP server. Downloads the right prebuilt binary for your OS/arch and starts the MCP server.

## Install

```sh
# 1. Scaffold .agent/ into your project
npx dreamd-mcp init

# 2. Point Claude Code, Cursor, or any MCP-aware harness at the server
npx dreamd-mcp
```

No Rust installation required. Supports Linux x86_64/aarch64, macOS x86_64/aarch64.

## Override

Set `DREAMD_BIN=/path/to/dreamd` to skip download and use a local build.

## License

Apache-2.0
