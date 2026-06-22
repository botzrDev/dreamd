# Security policy

## Reporting a vulnerability

**Do not open a public GitHub issue for security vulnerabilities.**

Email `austin@botzr.com` with the subject line `dreamd security: <short description>`. Please include:

- A description of the issue and its impact.
- Steps to reproduce, or a proof-of-concept.
- The version, OS, and any relevant configuration (`--insecure`, custom socket paths, etc.).
- Whether you are willing to be credited.

You should receive an acknowledgement within 5 business days. We aim to provide an initial assessment within 10 business days and a fix or mitigation timeline within 30 days, depending on severity.

We follow a coordinated-disclosure model: please give us a reasonable window to ship a fix before publicly disclosing.

## Supported versions

`dreamd` is pre-release. Until v0.1.0, only the `main` branch is supported and only the latest commit receives security fixes. After v0.1.0 this table will be updated to reflect supported minor versions.

| Version | Supported |
|---|---|
| `main` (pre-v0.1) | Yes |
| Anything else | No |

## Threat model (summary)

The reference implementation is local-first and single-tenant. It assumes:

- The host machine and the user account running `dreamd` are trusted.
- Other local users on the same machine are **not** trusted.
- The network is **not** trusted by default.

At v0.1, the daemon enforces:

- **Unix:** binds to a Unix domain socket at `~/.agent/dreamd.sock` with `0600` permissions. Every request is authenticated by validating the connecting peer's UID via `SO_PEERCRED` (Linux) or `getpeereid` (macOS); requests from any other UID are rejected.

Additional enforcement landing in v0.1.1:

- **Windows:** binds to `127.0.0.1` on an ephemeral port and requires a bearer token written to `~/.agent/auth.json` protected by Windows ACLs.
- **TCP binding to non-localhost is refused unless `--insecure` is passed**, which is intended only for ephemeral test environments.
- **The `personal/` layer is excluded from any network call (LLM or otherwise) unless the user opts in with `--share-personal`.**
- **LLM cost cap.** Token usage is estimated before each dream-cycle call; if the estimate exceeds `$0.10` the cycle aborts and falls back to deterministic mode. A `--no-llm` mode always works without network.

An expanded threat model — lesson-injection analysis, privacy disclosure, and untrusted-input caps — is planned for a future release.

## Environment variables

The MCP shim and client honor two overrides. Both assume a trusted local environment; a process that can set environment variables for your user can redirect dreamd traffic.

| Variable | Effect | Risk |
|---|---|---|
| `DREAMD_SOCK` | Overrides the Unix socket path used by MCP Phase 2 to reach the daemon (default `~/.agent/dreamd.sock`). | Redirects MCP traffic to an attacker-controlled socket that impersonates the daemon API. |
| `DREAMD_BIN` | Skips hash verification and runs the specified binary instead of the shim-downloaded release artifact. | Runs arbitrary code with the privileges of the MCP server process. Intended for local development only. |

Do not set these in shared shells, CI secrets, or harness configs you did not author. The reference implementation does not read them from project config files.

## Out of scope

Issues we do **not** consider security vulnerabilities:

- Denial-of-service from a local user with the same UID as the daemon (they can already do anything the daemon can).
- Any issue requiring `--insecure` on a trusted network.
- Bugs in third-party AI agents or MCP clients that consume the API.
