# Security policy

## Reporting a vulnerability

**Do not open a public GitHub issue for security vulnerabilities.**

Email `austin@botzr.com` with the subject line `dreamd security: <short description>`. Please include:

- A description of the issue and its impact.
- Steps to reproduce, or a proof-of-concept.
- The version, OS, and any relevant configuration (custom socket paths, etc.).
- Whether you are willing to be credited.

You should receive an acknowledgement within 5 business days. We aim to provide an initial assessment within 10 business days and a fix or mitigation timeline within 30 days, depending on severity.

We follow a coordinated-disclosure model: please give us a reasonable window to ship a fix before publicly disclosing.

## Supported versions

Security fixes land on `main` and the latest `0.1.x` release.

| Version | Supported |
|---|---|
| `0.1.0` | Yes |
| `main` | Yes |
| Anything else | No |

## Threat model (summary)

The reference implementation is local-first and single-tenant. It assumes:

- The host machine and the user account running `dreamd` are trusted.
- Other local users on the same machine are **not** trusted.
- The network is **not** trusted by default.

At v0.1, the daemon enforces:

- **Unix:** binds to a Unix domain socket at `~/.agent/dreamd.sock` with `0600` permissions. Every request is authenticated by validating the connecting peer's UID via `SO_PEERCRED` (Linux) or `getpeereid` (macOS); requests from any other UID are rejected.

- **`personal/` LLM exclusion.** `.agent/personal/` is never included in the dream-cycle composition prompt unless the operator passes `dreamd dream --share-personal` (or sends `x-dreamd-share-personal: 1` to `POST /api/v1/dream`). Consent is **per call**: there is no config key, no environment variable, and no sticky state — a cycle that was not asked, on that invocation, to share the personal layer does not read it. See [Personal-layer consent](#personal-layer-consent-v01) below.
- **LLM cost cap** ($0.10/cycle by default, `cost_cap_usd`) enforced *before* the request, with deterministic fallback.

**Planned for v0.1.1** (not in v0.1 binaries):

- **Windows:** `127.0.0.1` on an ephemeral port with bearer token in `~/.agent/auth.json`.
- **TCP binding to non-localhost** refused unless `--insecure` is passed (test environments only).

### Same-user-cross-project surface (accepted for v0.1)

Routing uses the `X-Agent-Root` header (project root path). With a per-user UDS, any process running as the user can target any registered project. Peer-credential auth verifies **same user**, not same project. If code runs as the user, it can already read project files directly. v0.2 may add per-project tokens or per-project sockets.

## Lesson-injection surface

`LESSONS.md` and `PREFERENCES.md` are plain files on disk. Any process running as the user can write to them and influence agent recall — the standard agent-memory threat model, not a dreamd-specific bypass.

- dreamd does not filter "malicious" lesson content (unsolved problem; false confidence).
- Users should treat `.agent/` like shell rc files: review with `git diff` if the repo is committed.
- The redaction scrubber (below) targets **secret leakage**, not prompt injection.

## Privacy and redaction (v0.1)

**v0.1 makes network calls only during the dream cycle, and only when an API key is present.** LLM-assisted composition shipped in v0.1: when `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` is in the environment or `~/.config/dreamd/secrets.toml` (mode `0600`) holds a key, `dreamd dream` and `POST /api/v1/dream` send the promoted cluster's `AGENT_LEARNINGS.jsonl` event bodies to the configured model to compose the `LESSONS.md` body. With no usable key — or with `--no-llm` — nothing leaves the device and the cycle writes the deterministic exemplar copy instead. Recall, append, indexing, and every other command make no network calls at all.

### Personal-layer consent (v0.1)

`.agent/personal/` is excluded from that request **by default and by construction**. The composition prompt is built from the promoted cluster alone; the function that builds it is never handed the store, so it cannot reach `personal/` whatever the operator has put there.

Passing `dreamd dream --share-personal` — or sending `x-dreamd-share-personal: 1` — appends `personal/` to that one prompt:

- **Per call, never sticky.** The flag authorizes exactly the cycle it is passed to. There is no config key and no persisted consent.
- **Only `personal/`.** Regular UTF-8 files directly under `.agent/personal/`, ordered `PREFERENCES.md`, `DECISIONS.md`, then the rest by name. Subdirectories are not walked, and a symlink whose target resolves outside `personal/` is refused rather than followed.
- **16 KiB cap**, the same ceiling `GET /api/v1/preferences` truncates at. Over-cap content is truncated with a `WARN`.
- **Logged every time.** A cycle that attaches personal bytes emits a `WARN` naming `share-personal` and `personal/`, so a disclosure is visible in the daemon log rather than silent.
- **Nothing is persisted.** Personal bytes are prompt context only. The dream cycle still never distills `personal/` into `semantic/`, and the citation trailer still names only `evt_` ids from the episodic log.
- **`--no-llm` wins.** `--no-llm --share-personal` is legal and discloses nothing: with no request, there is nobody to consent to. `personal/` is not even read.

`GET /api/v1/preferences` is unaffected — it serves `PREFERENCES.md` to the local agent over the UDS and is not an LLM call.

On every `POST /api/v1/learn`, a pattern scrubber runs **before persistence** (on by default; disable with `redaction = false` in config). It redacts AWS keys, bearer tokens, `sk-…` patterns, and common `*_KEY=` assignments — logging `redaction_hits` but not rejecting the request.

## Untrusted-input caps

- **`PREFERENCES.md`:** 16 KB read cap; responses may include `X-Dreamd-Truncated: true` and `X-Dreamd-Original-Size`.
- Caps address DoS and token cost, not injection.

## Environment variables

The MCP shim and client honor two overrides. Both assume a trusted local environment; a process that can set environment variables for your user can redirect dreamd traffic.

| Variable | Effect | Risk |
|---|---|---|
| `DREAMD_SOCK` | Overrides the Unix socket path used by MCP Phase 2 to reach the daemon (default `~/.agent/dreamd.sock`). | Redirects MCP traffic to an attacker-controlled socket that impersonates the daemon API. |
| `DREAMD_BIN` | Skips hash verification and runs the specified binary instead of the shim-downloaded release artifact. Refused unless `DREAMD_BIN_ALLOW_UNVERIFIED=1` is also set to confirm the bypass. | Runs arbitrary code with the privileges of the MCP server process. Intended for local development only. |

Do not set these in shared shells, CI secrets, or harness configs you did not author. The reference implementation does not read them from project config files.

## Out of scope

Issues we do **not** consider security vulnerabilities:

- Denial-of-service from a local user with the same UID as the daemon (they can already do anything the daemon can).
- Any issue requiring the planned v0.1.1 `--insecure` test mode on a trusted network.
- Bugs in third-party AI agents or MCP clients that consume the API.
