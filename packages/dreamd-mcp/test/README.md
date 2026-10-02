# dreamd-mcp shim tests

Node.js tests for the `npx dreamd-mcp` entrypoint (`bin/dreamd-mcp.js`) and the package metadata it ships with.

## Files

| File | Covers |
|---|---|
| `route.test.js` | Subcommand routing: which first tokens pass through to the binary (`init`, `watch`, `service`, `memory`, `forget`, …), the default to `dreamd mcp`, and the shim-handled `--version` / `--help` flags |
| `security.test.js` | Download redirect allowlist, tar path traversal guards, sha256 check of a cached binary before exec |
| `manifest.test.js` | `manifest.json` version matches `package.json`, and every platform has a real sha256 (no placeholders) |
| `registry-metadata.test.js` | `server.json` (MCP registry entry) agrees with `package.json`: name, versions, npm identifier, repository, stdio transport |
| `version-consistency.test.js` | `dreamd-mcp -V` / `--version` print the `package.json` version (runs the shim; no download) |

## Run

```bash
cd packages/dreamd-mcp
npm test
```

Requires Node.js; `npm test` is `node --test`, with no dependencies to install. The tests download nothing and need no built binary. `manifest.test.js` fails while `manifest.json` holds `PENDING_*` placeholders, which is the state between a version bump and the release workflow's manifest write-back. To run the shim itself against a local build, set `DREAMD_BIN=<path>` together with `DREAMD_BIN_ALLOW_UNVERIFIED=1`; that is a manual check, not something these tests do.
