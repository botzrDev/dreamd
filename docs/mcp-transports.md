# MCP transports

`dreamd mcp` serves the dreamd MCP server — the `search_nodes` and `append_node` tools — over one of two transports. The tools, the backend choice (daemon proxy or in-process store) and the `.agent/` store they read and write are the same on both. Only the wire differs.

| | stdio (default) | Streamable HTTP (opt-in) |
|---|---|---|
| Start | `dreamd mcp`, or `npx -y dreamd-mcp` | `dreamd mcp --bind 127.0.0.1:<port>` |
| Endpoint | the process's stdin / stdout | `http://<ADDR>/mcp` |
| Started by | the IDE, one process per MCP session | you or a service manager, one process for many clients |
| Stops on | stdin EOF (the client closes the pipe) | Ctrl-C, or SIGTERM on Unix (Ctrl-C only on Windows) |
| Auth | none needed: only the IDE that spawned the process holds its pipes | **none**: see [Security](#security) |
| Built in | every build | only builds with the `mcp-http` cargo feature: see [Availability](#availability) |

**Canonical source:** `crates/dreamd-core/src/mcp/mod.rs` (`run_mcp_server`), `crates/dreamd-core/src/mcp/http.rs`

---

## stdio (default)

This is what MCP-aware IDEs (Claude Code, Cursor, …) spawn, and what `npx -y dreamd-mcp setup` writes into their MCP config. JSON-RPC runs on stdout, and all logs and diagnostics go to stderr. When the IDE closes stdin, `dreamd mcp` exits.

Nothing about this path changed when HTTP was added. Without `--bind`, `dreamd mcp` never opens a TCP port.

## Availability

Streamable HTTP is compiled in only when you build with the **`mcp-http`** cargo feature, which is off by default. It adds about 1.5 MB to the stripped binary, and the default release build has to stay under the 20 MB size limit (NFR-2). So the **prebuilt binaries** — GitHub release tarballs, and what `npx -y dreamd-mcp` downloads — are **stdio-only**. On those binaries `--bind` still parses, but it exits **2** with a message that names the feature. Nothing binds.

Build it yourself:

```bash
cargo install --path crates/dreamd-cli --features mcp-http
# or: cargo build --release -p dreamd --features mcp-http
```

## Streamable HTTP (`--bind`)

```bash
dreamd mcp --bind 127.0.0.1:8080
# stderr: dreamd mcp: Streamable HTTP at http://127.0.0.1:8080/mcp
```

- **Address.** `--bind` takes an IP literal with an optional port: `127.0.0.1:8080`, `[::1]:0`, `127.0.0.1`. The port defaults to 0, which lets the OS pick one; the stderr line above prints the port you got. Hostnames (`localhost`) are a usage error (exit **2**) and are never resolved. The parser is the one `dreamd watch --bind` uses.
- **Protocol.** [MCP Streamable HTTP](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports#streamable-http), served by rmcp with its defaults. Sessions are stateful: `initialize` returns an `Mcp-Session-Id` header, and later requests must send it back. POST responses are `text/event-stream`, so clients must send `Accept: application/json, text/event-stream` and `Content-Type: application/json`.
- **Host check.** On a loopback bind, only a `Host` of `localhost`, `127.0.0.1` or `::1` (with any port) is accepted. Any other `Host` gets **403**. This guards a local server against DNS rebinding.
- **Backend.** The backend is chosen once, at start, by the same rules as stdio. If the daemon is reachable, requests are proxied to it. Otherwise, on Unix, the store is served in-process. Every HTTP session shares that one backend.
- **Process.** stdin is not read and stdout carries no protocol, so the server keeps running when stdin closes. Stop it with Ctrl-C or SIGTERM.

A handshake by hand:

```bash
curl -sN http://127.0.0.1:8080/mcp \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"curl","version":"0"}}}'
```

### Non-loopback binds: `--insecure`

The bind check is the same one `dreamd watch --bind` uses (AILAB-197):

| Invocation | Result |
|---|---|
| `--bind 127.0.0.1:<port>` or `--bind [::1]:<port>` | Binds. No flag needed. |
| `--bind 0.0.0.0:<port>` (or any non-loopback IP) | Refused **before** anything binds, exit **1**, with a message naming `--insecure`. |
| `--bind 0.0.0.0:<port> --insecure` | Binds. A `WARN` is logged on every start, and the `Host` check is turned off so LAN clients, which send the machine's own address as `Host`, are not refused. |
| `--insecure` without `--bind` | Usage error, exit **2**. stdio has no listener for the flag to loosen. |

## Security

**The HTTP port is unauthenticated.** It has no bearer token, reads no `auth.json`, and checks no peer credential.

- A **loopback** bind can be reached by every local process and every local user account on the machine. That is weaker than stdio, where only the IDE that spawned the process can talk to it. It is also weaker than the Unix daemon socket, which is `0600` and checks the peer's UID with `SO_PEERCRED`. Do not use `--bind` on a shared multi-user host.
- An **`--insecure`** bind exposes your memory store to the network. Anyone who can reach the port can read it with `search_nodes` and write to it with `append_node`. Use it only on a network you control, such as a test VM.
- The port is **not published**. `dreamd mcp --bind` does not write `~/.agent/server.json`, and `dreamd status` does not know the port exists. That file belongs to `dreamd watch` on Windows.

## This is not the REST API

[http-api.md](./http-api.md) documents the `dreamd watch` REST API under `/api/v1`. That is a different server: it listens on its own socket or port and has its own auth (peer UID on Unix, a bearer token on Windows). The MCP HTTP port serves `/mcp` and nothing else, so `GET /api/v1/recall` on it is a **404**. `dreamd watch` does not serve MCP.

## Platform notes

- **Unix.** `dreamd watch` still serves only `~/.agent/dreamd.sock`, and `watch --bind` / `watch --insecure` still exit 2. `dreamd mcp --bind` is the only TCP listener dreamd opens on Unix.
- **Windows.** `dreamd mcp` still needs a live `dreamd watch` to proxy to, whichever transport it uses. Without one it exits 2 (`Unsupported`), because there is no in-process store on Windows and `--bind` does not add one. See [windows.md](./windows.md).
- **npm.** `npx -y dreamd-mcp --bind 127.0.0.1:8080` runs `dreamd mcp --bind 127.0.0.1:8080`, but the prebuilt binary the shim downloads has no `mcp-http`, so it exits 2 (see [Availability](#availability)). The MCP Registry entry (`packages/dreamd-mcp/server.json`) still declares a stdio transport.
