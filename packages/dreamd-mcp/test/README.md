# dreamd-mcp shim tests

Node.js tests for the `npx dreamd-mcp` entrypoint (`bin/dreamd-mcp.js`).

## Files

| File | Covers |
|---|---|
| `route.test.js` | Subcommand routing (`init`, `watch`, `mcp`, passthrough to binary) |
| `security.test.js` | Download redirect allowlist, tar path traversal guards |

## Run

```bash
cd packages/dreamd-mcp
npm test
```

Requires Node.js. Tests mock the binary download path where possible; use `DREAMD_BIN` (with `DREAMD_BIN_ALLOW_UNVERIFIED=1`) for local integration against a built `dreamd` binary.
