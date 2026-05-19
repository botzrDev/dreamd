# @dataprime1/dreamd-mcp

Node shim for the dreamd MCP server. Downloads the right prebuilt binary for your OS/arch and starts the MCP server.

## Install

```sh
npx @dataprime1/dreamd-mcp
# or shorthand (if npm shorthand is claimed):
npx dreamd-mcp
```

No Rust installation required. Supports Linux x86_64/aarch64, macOS x86_64/aarch64, Windows x86_64.

## Override

Set `DREAMD_BIN=/path/to/dreamd` to skip download and use a local build.

## License

Apache-2.0
