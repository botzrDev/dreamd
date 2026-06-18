# dreamd

[![License: Apache 2.0](https://img.shields.io/badge/license-Apache_2.0-blue.svg)](./LICENSE)
![MCP-compatible](https://img.shields.io/badge/MCP-compatible-blueviolet.svg)
[![Platforms](https://img.shields.io/badge/platforms-linux%20%7C%20macOS-lightgrey.svg)](#platforms)
[![Status](https://img.shields.io/badge/v0.1-in%20progress-orange.svg)](#v01-progress)

**Same memory in every IDE.**

dreamd makes Claude Code, Cursor, and Cline remember the same things. Drop a .agent/ folder in your repo. Every coding agent you use reads and writes to it.

Every coding agent ships its own memory format. dreamd is what they could share.

AGENTS.md is what you wrote down. dreamd is what your agent learned, across every tool.

## The moment it earns its name

```text
~/project $ npx dreamd-mcp init

# In Claude Code, Tuesday afternoon:
you   ▸ axum keeps blowing up when I unwrap in route handlers
claude▸ filed under rust::error_handling::axum_rejection

# In Cursor, Friday morning, fresh session:
you   ▸ why is this build failing?
cursor▸ You're unwrapping in a route handler. dreamd has a
        lesson from Tuesday -- axum needs IntoResponse on
        custom Error types. Try `?` and a typed error.
```

No re-explaining. No re-pasting. No "as I mentioned before."

## What dreamd is -- and isn't

| dreamd is | dreamd isn't |
|---|---|
| A portable memory format (`.agent/`) checked into your repo | A vector database |
| A reference MCP server for reading and writing it | A knowledge graph engine |
| Local-first by default -- zero network calls without `--llm` | A hosted SaaS |
| One source of truth across every coding agent you use | A replacement for `AGENTS.md` or `SKILL.md` |

If you need graph multi-hop reasoning, use [Cognee](https://github.com/topoteretes/cognee). If you need a single-file portable memory capsule, use [Memvid](https://github.com/Olow304/memvid). dreamd does the one thing they don't: makes your memory follow you between coding agents.

## Status

**v0.1 in active development — targeting 2026-07-07.** Sprint 3 in progress; Sprints 1–2 shipped. The daemon builds and runs locally today: `dreamd init`, `dreamd dream`, `dreamd doctor`, `dreamd mcp`, `dreamd watch`, `dreamd reset workspace`, and `dreamd version`, plus the HTTP API (`POST /api/v1/learn`, `GET /api/v1/recall`, `POST /api/v1/dream`, `GET /api/v1/preferences`) on a Unix domain socket. The `npx dreamd-mcp` install path lands ahead of v0.1. Linux and macOS. See [`SPEC.md`](./SPEC.md) for the conformance contract and [`CONTRIBUTING.md`](./CONTRIBUTING.md) to propose changes.

⭐ **Star and Watch this repo** to be notified when v0.1 lands.

## Performance

Recall latency (warm in-RAM index, Criterion 0.5, WSL2/Linux):

| Corpus size | Mean (warm) |
|---|---|
| 1 000 entries   | ~50 µs |
| 10 000 entries  | ~313 µs |
| 100 000 entries | ~2.8 ms |

_Criterion reports mean across 100 samples; used here as the P50 proxy. All three sizes are well under the `<5ms P50 warm` NFR. Run `cargo bench -p dreamd-core` to reproduce._

> **Read-after-write visibility:** up to `commit_cadence_seconds` (default 5s). A just-written event becomes recallable within one commit cycle; recall latency itself is unaffected.

Users who need tighter freshness can lower the commit cadence at the cost of higher index churn. User-facing config lands in v0.1.1; the cadence is a constructor argument today.

## Getting started

### Try it today (from source)

While v0.1 finishes baking, build the daemon locally:

```bash
git clone https://github.com/botzrDev/dreamd.git
cd dreamd
cargo install --path crates/dreamd-cli

# Then in any project:
cd ~/your-project
dreamd init      # scaffold .agent/
dreamd mcp       # speak MCP over stdio -- point Claude Code, Cursor, etc. at this
```

Requires Rust stable. See [`CONTRIBUTING.md`](./CONTRIBUTING.md) for the full dev setup.

### v0.1 install path

*Landing ahead of 2026-07-07.*

```bash
# scaffold .agent/ into the current project
npx dreamd-mcp init

# point Claude Code, Cursor, or any MCP-aware harness at the server
npx dreamd-mcp
```

Distribution: npm (primary). Cargo and Homebrew paths arrive in v0.1.1. See the [v0.1 milestone](https://github.com/botzrDev/dreamd/milestones).

### Privacy

When LLM mode is enabled in v0.1.1, entries above the relevance threshold may be sent to the configured provider. The `personal/` layer is excluded unless you explicitly opt in. v0.1 makes no network calls. See [`SECURITY.md`](./SECURITY.md) for the threat model and disclosure policy.

## Platforms

v0.1 supports Linux and macOS. Windows lifecycle support arrives in v0.1.1.

## Spec

The on-disk layout, JSON schema, scoring formula, and dream-cycle contract are defined in [`SPEC.md`](./SPEC.md). The spec is implementation-agnostic — `dreamd` is one implementation, not the only one.

To propose a change to the spec, open an issue prefixed with `[RFC]`. See [CONTRIBUTING.md](./CONTRIBUTING.md).

## v0.1 progress

| Layer | Status |
|---|---|
| `SPEC.md` v0.1-draft | Drafted |
| Reference implementation (`dreamd` daemon, HTTP API, dream cycle, Tantivy recall) | In progress — Sprint 3 of 6 |
| MCP server (`dreamd mcp` subcommand + `npx dreamd-mcp` shim) | Shipped — `dreamd-mcp@0.1.0-rc.1` on npm |
| CI / cross-platform matrix | Lint, test, cross-platform build, binary-size gate, DCO check |
| Conformance test suite | Not started |

## Contributing

See [CONTRIBUTING.md](./CONTRIBUTING.md) for the development setup, commit conventions (`DR-XXX` story IDs), DCO sign-off policy, and the RFC process.

By participating in this project you agree to abide by the [Code of Conduct](./CODE_OF_CONDUCT.md).

To report a security issue, see [SECURITY.md](./SECURITY.md). Do not open a public issue for vulnerabilities.

## License

Apache-2.0. See [LICENSE](./LICENSE) and [NOTICE](./NOTICE).
