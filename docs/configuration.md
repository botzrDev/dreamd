# Configuration reference

dreamd loads runtime configuration from layered TOML files. Missing files are silently skipped; malformed TOML is a parse error at startup (`dreamd watch: config load: config parse error: …`, exit 1).

**Canonical source:** `crates/dreamd-core/src/config.rs`

---

## Precedence (low → high)

Later layers override earlier ones:

1. **Built-in defaults** — hardcoded `Config::default()`
2. **User config** — `dreamd/config.toml` under the platform config directory (`~/.config/dreamd/config.toml` on Linux; see below for macOS and Windows)
3. **Project config** — `<project>/.agent/.dreamd/config.toml`

`dreamd init` writes a commented template to the project config path. Uncomment keys to override.

---

## Config file locations

| Layer | Path | Created by |
|---|---|---|
| User | `~/.config/dreamd/config.toml` | Manual (optional) |
| Project | `<project>/.agent/.dreamd/config.toml` | `dreamd init` (commented template) |

The user path is `dirs::config_dir()` + `dreamd/config.toml`. On Linux that is `$XDG_CONFIG_HOME/dreamd/config.toml` when `XDG_CONFIG_HOME` is set, otherwise `~/.config/dreamd/config.toml`. On macOS it is `~/Library/Application Support/dreamd/config.toml`. On Windows it is `%APPDATA%\dreamd\config.toml`. The `~/.config/…` spelling used elsewhere on this page (and in the template comment) is the Linux default.

---

## Default template

This is the exact template written by `dreamd init` (`CONFIG_TEMPLATE`):

```toml
# dreamd config — all keys optional. Precedence: this file > ~/.config/dreamd/config.toml > built-in defaults.

# redaction = true              # redact secrets/PII on POST /api/v1/learn (DR-111)
# log_level = "info"            # trace | debug | info | warn | error
# dream_cycle_mode = "manual"   # "manual" | "auto" — v0.1 is manual-only (DR-315)

# --- LLM keys: read by the dream cycle (AILAB-204) ---
# provider = ""                 # "anthropic" | "openai"; empty infers from model
# model = "claude-haiku-4-5"    # model id
# endpoint = ""                 # base URL override; empty uses the provider default
# cost_cap_usd = 0.10           # hard per-cycle spend cap — enforced pre-call (AILAB-196)
```

A fully commented project config parses successfully and yields built-in defaults.

---

## Fields

| Key | Type | Default | Layer | Effect |
|---|---|---|---|---|
| `redaction` | boolean | `true` | user, project | When `true`, secret patterns are redacted from `content` before the durable write — on `POST /api/v1/learn` and on the in-process MCP `append_node` path |
| `log_level` | string | `"info"` | user, project | **Parsed but unused.** The live log filter is the `DREAMD_LOG` env var, not this TOML key. |
| `dream_cycle_mode` | string | `"manual"` | user, project | `"manual"` or `"auto"`. 1.0.0 is manual-only: `dreamd watch` and `dreamd mcp` **hard-error** (exit 1) if mode is `"auto"` (not a silent ignore). Auto scheduling is not shipped |
| `provider` | string | `""` | user, project | LLM provider id — `"anthropic"` or `"openai"`. Empty (default) infers the provider from the model name. Read by the dream cycle (AILAB-204) |
| `model` | string | `"claude-haiku-4-5"` | user, project | LLM model id used to compose `LESSONS.md` bodies. Read by the dream cycle (AILAB-204) |
| `endpoint` | string | `""` | user, project | Base URL override for the LLM provider. Empty (default) uses the provider's own endpoint. Read by the dream cycle (AILAB-204) |
| `cost_cap_usd` | float | `0.10` | user, project | Per-cycle USD spend cap, **enforced** by the dream cycle (AILAB-196). The composition prompt is priced before the model call; over the cap the call is skipped and the deterministic exemplar body is written instead. The estimate is approximate — see [llm-cost-accuracy.md](./llm-cost-accuracy.md) |

### `redaction`

When enabled (default), content passed to `POST /api/v1/learn` (or to MCP `append_node`) is scanned for a narrow set of high-confidence secret patterns — AWS access key ids (`AKIA…`), `Bearer <token>`, `sk-…` / `sk-ant-…` API keys, and `API_KEY=` / `SECRET=` / `TOKEN=` / `PASSWORD=` style assignments — and each match is replaced with `[REDACTED]` before append. There is no per-request opt-out — only this config flag controls redaction.

To disable for local development:

```toml
# <project>/.agent/.dreamd/config.toml
redaction = false
```

### `dream_cycle_mode`

| Value | Meaning |
|---|---|
| `manual` | Dream cycles run only when invoked (`dreamd dream` / `npx -y dreamd-mcp dream`, or `POST /api/v1/dream`). There is no MCP dream tool. |
| `auto` | Not supported. `dreamd watch` and `dreamd mcp` exit 1 if this value is set, `dreamd dream --auto` exits 2, and `dreamd doctor` prints a `WARNING` on its `dream_cycle_mode:` line. `dreamd mcp --manual-only` overrides the config value for that one server. |

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
| `DREAMD_SOCK` | `~/.agent/dreamd.sock` | MCP client, `dreamd dream` proxy, and every command that probes the daemon (`status`, `doctor`, `archive`, `forget`, `setup`, `uninstall`, `update`; not `memory checkout`, which always looks at `~/.agent/dreamd.sock`) | Override Unix socket path for **clients** connecting to the daemon. Must be an absolute path. **`dreamd watch` ignores this** and always binds `$HOME/.agent/dreamd.sock`. Unix only. |
| `DREAMD_BIN` | (shim download) | `npx dreamd-mcp` shim only | Dev override: run a local `dreamd` binary instead of the cached release artifact. Requires `DREAMD_BIN_ALLOW_UNVERIFIED=1` or the shim exits 1. Skips SHA-256 verification. |
| `DREAMD_BIN_ALLOW_UNVERIFIED` | unset | `npx dreamd-mcp` shim only | Must be `1` alongside `DREAMD_BIN` to accept an unverified local binary. |
| `DREAMD_LOG` | `info` | CLI / daemon tracing | Live log filter (`trace`, `debug`, `info`, `warn`, `error`). Not a TOML key. |
| `HOME` | (OS default) | CLI / daemon layout | Resolves every `~/.agent` path (registry, socket, daemon log). Override in tests/CI sandboxes. An empty value counts as unset. On Windows only, `USERPROFILE` is used when `HOME` is unset or empty. |
| `ANTHROPIC_API_KEY`, `OPENAI_API_KEY` | unset | dream cycle | API key for the opt-in LLM-assisted dream cycle. See [LLM credentials](#llm-credentials). |
| `SOURCE_DATE_EPOCH` | unset (wall clock) | `dreamd dream` (in-process cycle and `--dry`; not a cycle proxied to the daemon) | Integer unix timestamp that pins the dream-cycle clock, for reproducible fixtures. A non-integer value is an error, not a fallback. |
| `HF_HOME` | unset | `dreamd vectors enable` in a `--features vectors` build | Where the optional embedding model is downloaded instead of `~/.agent/models`. Has no effect on the default binary, where the command exits 2. |

### `DREAMD_SOCK`

**Clients** (MCP, `dreamd dream` when proxying, `status`, `doctor`, `archive`, `forget`, …) honor this variable. **`dreamd watch` does not** — it always binds `$HOME/.agent/dreamd.sock`. Setting `DREAMD_SOCK` and then running `watch` leaves clients pointing at a socket the daemon never created.

The value must be absolute. With a relative path `dreamd mcp` exits 1 (`DREAMD_SOCK is not an absolute path: …`); the one-shot commands treat it as "no daemon address" — `dreamd status` reports `daemon: not running`, and `dreamd dream` runs in-process instead of proxying.

```bash
# Client override only — the daemon must already be listening at this path
export DREAMD_SOCK=/tmp/my-dreamd.sock
npx -y dreamd-mcp              # connects here
```

To run a daemon on a non-default path, change `$HOME` (which relocates the whole daemon home, including the socket).

### `DREAMD_BIN`

Local development only — never set in production MCP configs. Both variables are required:

```bash
export DREAMD_BIN=~/.cargo/bin/dreamd
export DREAMD_BIN_ALLOW_UNVERIFIED=1
npx -y dreamd-mcp
```

Without `DREAMD_BIN_ALLOW_UNVERIFIED=1` the shim prints an error and exits 1.

See [../packages/dreamd-mcp/README.md](../packages/dreamd-mcp/README.md) and [../SECURITY.md](../SECURITY.md) for the threat model around both variables.

### LLM credentials

The dream cycle is deterministic unless an API key is found. Keys are never TOML keys in `config.toml`; they are resolved in this order:

1. `ANTHROPIC_API_KEY` in the environment
2. `OPENAI_API_KEY` in the environment
3. `anthropic_api_key`, then `openai_api_key`, in `secrets.toml` — next to the user `config.toml` (`~/.config/dreamd/secrets.toml` on Linux)

An empty value counts as unset. On Unix `secrets.toml` is read only when its mode is exactly `0600`; any other mode is ignored with a warning (`chmod 600` to enable it). With no key the cycle runs deterministically. `dreamd dream --no-llm` forces the deterministic cycle even when a key is present. A daemon reads its own environment, so a key exported in your shell is not seen by a `dreamd watch` started elsewhere (a login service, for example) — use `secrets.toml` there.

---

## What is not configurable

| Setting | Value | Notes |
|---|---|---|
| Index commit cadence | 5 seconds | Fixed; not user-configurable |
| Socket permissions | `0600` | Fixed |
| HTTP bind address | Unix: the socket only. Windows: `127.0.0.1:0` by default | Not a config key. Unix never binds TCP, and `dreamd watch --bind` / `--insecure` exit 2 there. On Windows `dreamd watch` binds loopback TCP and publishes the bound port in `~/.agent/server.json` (AILAB-192); `--bind <IP[:port]>` picks the address (port defaults to 0; hostnames are not resolved), and a non-loopback address is refused unless `--insecure` is also passed, which logs a warning on every start (AILAB-197). Neither flag skips the bearer token, and `server.json` always names a loopback host. |

---

## See also

- [../GUIDE.md](../GUIDE.md) — first-run walkthrough (learn / recall / watch)
- [llm-cost-accuracy.md](./llm-cost-accuracy.md) — how `cost_cap_usd` is estimated, and why the estimate is approximate
- [http-api.md](./http-api.md) — API affected by `redaction`
- [../SPEC.md](../SPEC.md) — schema and on-disk layout
- [../SECURITY.md](../SECURITY.md) — `DREAMD_SOCK` / `DREAMD_BIN` threat model
