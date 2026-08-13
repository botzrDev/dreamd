# Configuration reference

dreamd loads runtime configuration from layered TOML files. Missing files are silently skipped; malformed TOML returns a typed parse error at startup.

**Canonical source:** `crates/dreamd-core/src/config.rs`

---

## Precedence (low → high)

Later layers override earlier ones:

1. **Built-in defaults** — hardcoded `Config::default()`
2. **User config** — `~/.config/dreamd/config.toml` (XDG config dir on Linux/macOS)
3. **Project config** — `<project>/.agent/.dreamd/config.toml`

`dreamd init` writes a commented template to the project config path. Uncomment keys to override.

---

## Config file locations

| Layer | Path | Created by |
|---|---|---|
| User | `~/.config/dreamd/config.toml` | Manual (optional) |
| Project | `<project>/.agent/.dreamd/config.toml` | `dreamd init` (commented template) |

On macOS, the user path resolves via the platform config directory (typically `~/Library/Application Support/dreamd/config.toml` through the `dirs` crate). On Linux, it is `~/.config/dreamd/config.toml`.

---

## Default template

This is the exact template written by `dreamd init` (`CONFIG_TEMPLATE`):

```toml
# dreamd config — all keys optional. Precedence: this file > ~/.config/dreamd/config.toml > built-in defaults.

# redaction = true              # redact secrets/PII on POST /api/v1/learn (DR-111)
# log_level = "info"            # trace | debug | info | warn | error
# dream_cycle_mode = "manual"   # "manual" | "auto" — v0.1 is manual-only (DR-315)

# --- LLM keys: present but inert until v0.1.1 ---
# provider = ""                 # LLM provider id
# model = "claude-haiku-4-5"    # model id
# cost_cap_usd = 0.10           # hard per-cycle spend cap (DR-307)
```

A fully commented project config parses successfully and yields built-in defaults.

---

## Fields

| Key | Type | Default | Layer | Effect |
|---|---|---|---|---|
| `redaction` | boolean | `true` | user, project | When `true`, `POST /api/v1/learn` redacts secrets and PII patterns from `content` before durable write |
| `log_level` | string | `"info"` | user, project | **Parsed but unused in v0.1.** The live log filter is the `DREAMD_LOG` env var, not this TOML key. |
| `dream_cycle_mode` | string | `"manual"` | user, project | `"manual"` or `"auto"`. v0.1 is manual-only: `dreamd watch` **hard-errors** if mode is `"auto"` (not a silent ignore). Auto scheduling arrives in v0.1.1 |
| `provider` | string | `""` | user, project | LLM provider id. **Inert at v0.1** — reserved for v0.1.1 |
| `model` | string | `"claude-haiku-4-5"` | user, project | LLM model id. **Inert at v0.1** |
| `cost_cap_usd` | float | `0.10` | user, project | Per-cycle USD spend cap. **Inert at v0.1** |

### `redaction`

When enabled (default), content passed to `POST /api/v1/learn` is scanned for common secret patterns (AWS keys, etc.) and replaced with `[REDACTED]` before append. There is no per-request opt-out — only this config flag controls redaction.

To disable for local development:

```toml
# <project>/.agent/.dreamd/config.toml
redaction = false
```

### `dream_cycle_mode`

| Value | Meaning |
|---|---|
| `manual` | Dream cycles run only when invoked (`dreamd dream` / `npx -y dreamd-mcp dream`, or `POST /api/v1/dream`). There is no MCP dream tool. |
| `auto` | Not supported in v0.1. `dreamd watch` exits with an error if this value is set. |

---

### `log_level`

Present in the init template and merged like any other key, but **no runtime path reads it**. Setting `log_level = "debug"` does not change daemon logs. Use `DREAMD_LOG` instead.

---

## Example overrides

**Project-level log verbosity (env, not TOML):**

```bash
export DREAMD_LOG=debug
npx -y dreamd-mcp watch
```

**User-wide redaction off (all projects on this machine):**

```toml
# ~/.config/dreamd/config.toml
redaction = false
```

**Project overrides user for `log_level` (parsed only), inherits user for `redaction`:**

```toml
# ~/.config/dreamd/config.toml
log_level = "warn"
redaction = false

# .agent/.dreamd/config.toml
log_level = "debug"
# → effective parsed config: log_level=debug, redaction=false
# → live logs still follow DREAMD_LOG (default info)
```

---

## Environment variables

These are **not** TOML keys. They override runtime paths and shim behavior.

| Variable | Default | Used by | Purpose |
|---|---|---|---|
| `DREAMD_SOCK` | `~/.agent/dreamd.sock` | MCP client, `dreamd dream` proxy, `status`, `archive` | Override Unix socket path for **clients** connecting to the daemon. **`dreamd watch` ignores this** and always binds `$HOME/.agent/dreamd.sock`. |
| `DREAMD_BIN` | (shim download) | `npx dreamd-mcp` shim only | Dev override: run a local `dreamd` binary instead of the cached release artifact. Requires `DREAMD_BIN_ALLOW_UNVERIFIED=1` or the shim exits 1. Skips SHA-256 verification. |
| `DREAMD_BIN_ALLOW_UNVERIFIED` | unset | `npx dreamd-mcp` shim only | Must be `1` alongside `DREAMD_BIN` to accept an unverified local binary. |
| `DREAMD_LOG` | `info` | CLI / daemon tracing | Live log filter (`trace`, `debug`, `info`, `warn`, `error`). Not a TOML key. |
| `HOME` | (OS default) | CLI / daemon layout | Resolves every `~/.agent` path (registry, socket, daemon log). Override in tests/CI sandboxes. |

### `DREAMD_SOCK`

**Clients** (MCP, `dreamd dream` when proxying, `status`, `archive`) honor this variable. **`dreamd watch` does not** — it always binds `$HOME/.agent/dreamd.sock`. Setting `DREAMD_SOCK` and then running `watch` leaves clients pointing at a socket the daemon never created.

```bash
# Client override only — the daemon must already be listening at this path
export DREAMD_SOCK=/tmp/my-dreamd.sock
npx -y dreamd-mcp              # connects here
```

To run a daemon on a non-default path in v0.1, change `$HOME` (which relocates the whole daemon home, including the socket).

### `DREAMD_BIN`

Local development only — never set in production MCP configs. Both variables are required:

```bash
export DREAMD_BIN=~/.cargo/bin/dreamd
export DREAMD_BIN_ALLOW_UNVERIFIED=1
npx -y dreamd-mcp
```

Without `DREAMD_BIN_ALLOW_UNVERIFIED=1` the shim prints an error and exits 1.

See [../packages/dreamd-mcp/README.md](../packages/dreamd-mcp/README.md) and [../SECURITY.md](../SECURITY.md) for the threat model around both variables.

---

## What is not configurable in v0.1

| Setting | Value | Notes |
|---|---|---|
| Index commit cadence | 5 seconds | Fixed; not user-configurable |
| Socket permissions | `0600` | Fixed |
| HTTP bind address | Unix socket only | No TCP bind and no `--insecure` flag in v0.1 |

---

## See also

- [../GUIDE.md](../GUIDE.md) — first-run walkthrough (learn / recall / watch)
- [http-api.md](./http-api.md) — API affected by `redaction`
- [../SPEC.md](../SPEC.md) — schema and on-disk layout
- [../SECURITY.md](../SECURITY.md) — `DREAMD_SOCK` / `DREAMD_BIN` threat model
