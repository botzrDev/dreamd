# Configuration reference

dreamd loads runtime configuration from layered TOML files. Missing files are silently skipped. Malformed TOML stops `dreamd watch`, `dreamd mcp`, `dreamd doctor` and `dreamd dream` at start with exit 1 (for `watch`: `dreamd watch: config load: config parse error: …`), and fails an append made after the file went bad; see [When dreamd reads a project's config](#when-dreamd-reads-a-projects-config).

**Canonical source:** `crates/dreamd-core/src/config.rs`

---

## Precedence (low → high)

Later layers override earlier ones:

1. **Built-in defaults** — hardcoded `Config::default()`
2. **User config** — `dreamd/config.toml` under the platform config directory (`~/.config/dreamd/config.toml` on Linux; see below for macOS and Windows)
3. **Project config** — `<project>/.agent/.dreamd/config.toml`

`dreamd init` writes a commented template to the project config path. Uncomment keys to override.

---

## When dreamd reads a project's config

- `dreamd watch` reads the config of the project it starts in once, at start, for the `dream_cycle_mode = "auto"` refusal.
- The daemon reads each project's own config on every append (`POST /api/v1/learn`, and `append_node` through it). An edit to `redaction` applies to the next append, with no restart. In-process `dreamd mcp` does the same.
- The dream cycle reads the project's config when the cycle starts.
- `dreamd doctor`, `dreamd dream` and `dreamd mcp` read the config of the store they find from the working directory (or from `--project-root`), never the working directory's own. From `<project>/src` they read `<project>/.agent/.dreamd/config.toml`.
- A malformed file stops `watch`, `mcp`, `doctor` and `dream` at start, with exit 1.
- An append made after the file goes bad writes nothing. Over MCP it answers `-32600` with `config load failed: …`; over HTTP it is `500` with `{"error":"config load failed: …"}`.

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

# redaction = true              # redact known secret shapes on every append
# log_level = "info"            # parsed but unused; logs follow the DREAMD_LOG env var
# dream_cycle_mode = "manual"   # "manual" | "auto" — manual only; auto is rejected

# --- LLM keys: read by the dream cycle ---
# provider = ""                 # "anthropic" | "openai"; empty infers from model
# model = "claude-haiku-4-5"    # model id
# endpoint = ""                 # base URL override; empty uses the provider default
# cost_cap_usd = 0.10           # hard per-cycle spend cap, enforced before the model call
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

While `redaction` is `true` (the default), every append (MCP `append_node` in process or through the daemon, and `POST /api/v1/learn`) replaces the secret shapes below in `content` with `[REDACTED]` before the record is written. There is no per-request opt-out; only this setting controls it.

The patterns run in this order:

| # | Shape | What matches | Replaced | Example | Stored as |
|---|---|---|---|---|---|
| 1 | PEM private key | `-----BEGIN … PRIVATE KEY-----` through its `-----END … PRIVATE KEY-----` line, or to the end of the content when that line is missing | whole block | `pasted -----BEGIN RSA PRIVATE KEY----- MIIEFAKEFAKE truncated` | `pasted [REDACTED]` |
| 2 | URL password | `scheme://user:password@` | the password | `DATABASE_URL=postgres://admin:fakedbpass@db.invalid/app` | `DATABASE_URL=postgres://admin:[REDACTED]@db.invalid/app` |
| 3 | JSON Web Token | `eyJ…` `.` `eyJ…` `.` signature | whole | `jwt eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJmYWtlIn0.ZmFrZXNpZ2` | `jwt [REDACTED]` |
| 4 | Anthropic key | `sk-ant-` and 10 or more letters, digits, `_` or `-` | whole | `key=sk-ant-api03-abc123` | `key=[REDACTED]` |
| 5 | OpenAI and other `sk-` keys | `sk-` and 20 or more letters, digits, `_` or `-`, so `sk-proj-`, `sk-admin-` and `sk-svcacct-` keys too | whole | `openai project key sk-proj-FAKEfake_1234567890-abcdefghijklmnopqrst` | `openai project key [REDACTED]` |
| 6 | Stripe live key | `sk_live_` or `rk_live_` and 10 or more letters or digits | whole | `stripe sk_live_FAKEfake12` | `stripe [REDACTED]` |
| 7 | GitHub token | `ghp_`, `gho_`, `ghu_`, `ghs_` or `ghr_` and 36 or more letters or digits | whole | `pushed with ghp_FAKEfakeFAKEfakeFAKEfakeFAKEfake1234` | `pushed with [REDACTED]` |
| 8 | GitHub fine-grained token | `github_pat_` and 22 or more letters, digits or `_` | whole | `github_pat_11FAKEFAKE0fakefakefake_FAKEfakeFAKEfake` | `[REDACTED]` |
| 9 | Slack token | `xoxa-`, `xoxb-`, `xoxp-`, `xoxo-`, `xoxs-` or `xoxr-` and 10 or more letters, digits or `-` | whole | `bot token xoxb-FAKE-fake-FAKEfake` | `bot token [REDACTED]` |
| 10 | npm token | `npm_` and 20 or more letters or digits | whole | `//registry.npmjs.org/:_authToken=npm_FAKEfakeFAKEfakeFAKEfake1234` | `//registry.npmjs.org/:_authToken=[REDACTED]` |
| 11 | Google API key | `AIza` and 30 or more letters, digits, `_` or `-` | whole | `maps key AIzaFAKEfakeFAKEfakeFAKEfakeFAKEfa` | `maps key [REDACTED]` |
| 12 | AWS access key id | `AKIA` or `ASIA` and 16 uppercase letters or digits | whole | `aws temp key ASIAFAKEFAKEFAKE5678` | `aws temp key [REDACTED]` |
| 13 | Bearer token | `Bearer` in any case, then a token of letters, digits and `. _ ~ + / = -` | the token, if it looks like a credential | `Authorization: Bearer ZmFrZS1zZWNyZXQ+ZXQ/dG9rZW4=` | `Authorization: Bearer [REDACTED]` |
| 14 | Secret-named assignment | a name that ends in `password`, `passwd`, `secret`, `token`, `api_key`, `secret_key`, `access_key` or `private_key` (any case; `_`, `-` or nothing between the two words), an optional closing quote, `=` or `:`, then a quoted string or a run without spaces | the value, by the rules below | `{"api_key": "fakejsonkey789"}` | `{"api_key": "[REDACTED]"}` |

Shapes 2 to 11 and 13 to 14 start at a word boundary, so `task-based-approach` is not an `sk-` key and `NO_VAULT_TOKEN=1` is read as the name `NO_VAULT_TOKEN`. Shape 14's name must end in the secret word, so `TOKENIZER=cl100k_base` and `max_tokens: 1024` do not match.

For shapes 2, 13 and 14 only the value is replaced; the name, the separator and any quotes stay. An assignment's value is handled like this:

- **Always kept:** an empty value, a number made only of digits, `true`, `false`, `yes`, `no`, `on`, `off`, `null` or `none` in any case, a value that is already `[REDACTED]`, and a variable reference such as `$OPENAI_API_KEY` or `${NPM_TOKEN}`. So `MAX_TOKEN=4096` and `USE_TOKEN=true` stay.
- **Quoted:** otherwise replaced inside its quotes: `PASSWORD="fake pass phrase here"` becomes `PASSWORD="[REDACTED]"`.
- **Unquoted, after a bare `=`** (`NAME=value`): replaced. `SECRET=hunter2` becomes `SECRET=[REDACTED]`.
- **Unquoted, after `:` or an `=` with spaces** (YAML, headers, code): replaced only if it looks like a credential, the same test a bearer token gets. It needs 8 or more characters with at least one letter, and also a digit, `+`, `/` or `=`, or 32 or more characters. So `token: fakeyamltoken321` is replaced, and `secret: keep the cache warm`, `token = get_token()` and `Bearer authentication` stay.

Limits:

- A secret in a shape the table does not list is stored as written.
- An unquoted value after `:` or an `=` with spaces is kept unless it looks like a credential (8 or more characters with a letter, plus a digit, `+`, `/` or `=`, or 32 or more characters).
- Records stored before 1.3.3, or while `redaction` was off, are not changed.

While `redaction` is on, `dreamd doctor` runs the same patterns over the stored log. For each record that matches, it prints the record's line and id under `stored_secrets: N record(s)`, and fails the run. It never prints the content and changes nothing. Check each record, and remove one with `dreamd forget <id>`; see [troubleshooting](./troubleshooting.md#stored_secrets-n-records-).

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
# → live logs still follow DREAMD_LOG (default: dreamd info, libraries warn)
```

---

## Environment variables

These are **not** TOML keys. They override runtime paths and shim behavior.

| Variable | Default | Used by | Purpose |
|---|---|---|---|
| `DREAMD_SOCK` | `~/.agent/dreamd.sock` | `dreamd watch` (the path it binds), the MCP client, the `dreamd dream` proxy, and every command that probes the daemon (`status`, `doctor`, `archive`, `forget`, `setup`, `uninstall`, `update`; not `memory checkout`, which looks only at `~/.agent/dreamd.sock`) | Override the Unix socket path. `dreamd watch` and every client resolve it the same way, so a daemon started with it and a client run with it meet at that path. Must be an absolute path. Unix only. |
| `DREAMD_BIN` | (shim download) | `npx dreamd-mcp` shim only | Dev override: run a local `dreamd` binary instead of the cached release artifact. Requires `DREAMD_BIN_ALLOW_UNVERIFIED=1` or the shim exits 1. Skips SHA-256 verification. |
| `DREAMD_BIN_ALLOW_UNVERIFIED` | unset | `npx dreamd-mcp` shim only | Must be `1` alongside `DREAMD_BIN` to accept an unverified local binary. |
| `DREAMD_LAUNCHER` | unset | `dreamd setup` | The `npx dreamd-mcp` shim sets it to `npx` when it runs the binary. When the value is exactly `npx`, setup's hints print `npx -y dreamd-mcp <command>` instead of `dreamd <command>`. Not meant to be set by hand. |
| `DREAMD_LOG` | dreamd `info`, libraries `warn` | CLI / daemon tracing | Live log filter (`trace`, `debug`, `info`, `warn`, `error`, or `EnvFilter` directives). Not a TOML key. A set, valid value replaces the defaults for both the console and `~/.agent/dreamd.log`; an empty or invalid value gets the defaults. `info` shows library INFO lines (tantivy, rmcp) and `dreamd watch`'s per-request lines on the console. |
| `DREAMD_LOG_FORMAT` | `text` | CLI / daemon console | `json` (exactly) prints the console's lines on stderr as JSON objects, one per line. Any other value is text. `~/.agent/dreamd.log` is JSON either way. |
| `HOME` | (OS default) | CLI / daemon layout | Resolves every `~/.agent` path (registry, socket, daemon log). Override in tests/CI sandboxes. An empty value counts as unset. On Windows only, `USERPROFILE` is used when `HOME` is unset or empty. |
| `ANTHROPIC_API_KEY`, `OPENAI_API_KEY` | unset | dream cycle | API key for the opt-in LLM-assisted dream cycle. See [LLM credentials](#llm-credentials). |
| `SOURCE_DATE_EPOCH` | unset (wall clock) | `dreamd dream` (in-process cycle and `--dry`; not a cycle proxied to the daemon) | Integer unix timestamp that pins the dream-cycle clock, for reproducible fixtures. A non-integer value is an error, not a fallback. |
| `HF_HOME` | unset | `dreamd vectors enable` in a `--features vectors` build | Where the optional embedding model is downloaded instead of `~/.agent/models`. Has no effect on the default binary, where the command exits 2. |

### `DREAMD_SOCK`

There is one daemon socket per user, at `~/.agent/dreamd.sock` by default. One `dreamd watch` serves every project the user has registered: each request names its project in `X-Agent-Root`, and the daemon looks it up in `~/.agent/registry.toml`.

`dreamd watch` and every client (MCP, `dreamd dream` when proxying, `status`, `doctor`, `archive`, `forget`, …) resolve `DREAMD_SOCK` the same way: the variable's value when it is set, else `~/.agent/dreamd.sock`. Use it when one user needs a second daemon, for example a release binary and a development build side by side. Start that daemon and its clients with the same value. `dreamd memory checkout` looks only at `~/.agent/dreamd.sock`.

The value must be absolute. With a relative path, `dreamd watch` and `dreamd mcp` exit 1 (`DREAMD_SOCK is not an absolute path: …`). The one-shot commands treat it as "no daemon address": `dreamd status` reports `daemon: not running`, and `dreamd dream` runs in-process instead of proxying.

```bash
# A second daemon, and a client that talks to it
DREAMD_SOCK=/tmp/my-dreamd.sock dreamd watch &
export DREAMD_SOCK=/tmp/my-dreamd.sock
npx -y dreamd-mcp              # connects here
```

A client and a daemon of different dreamd versions refuse each other. The client prints, for example:

```text
version mismatch: client 1.3.1, daemon 1.3.0. Stop the other daemon and start this one with `dreamd watch`, or point DREAMD_SOCK at a daemon of this version.
```

Under `npx -y dreamd-mcp`, the shim sets `DREAMD_LAUNCHER=npx`, and the client names commands that work there:

```text
version mismatch: client 1.3.1, daemon 1.3.0. Stop the other daemon with `npx -y dreamd-mcp update`, start this one with `npx -y dreamd-mcp watch`, then reload your MCP harness. Or point DREAMD_SOCK at a daemon of this version.
```

A daemon from a release without this check sends no version, so the client prints `daemon unknown` in its place. The fix is the same.

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
