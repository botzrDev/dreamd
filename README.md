# dreamd

**Same memory in every IDE.**

dreamd makes Claude Code, Cursor, and Cline remember the same things. Drop a `.agent/` folder in your repo, run `npx dreamd-mcp`, and every MCP (Model Context Protocol) -aware coding agent reads and writes to the same memory -- episodic events, lessons, your preferences -- checked into git alongside your code.

Every coding agent ships its own memory format. dreamd is what they could share.

`AGENTS.md` is what you wrote down. `.agent/` is what your agent learned. dreamd is how it learns it once and remembers it everywhere.

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

**v0.1 (spec drafted, implementation starting).** Episodic memory + BM25-by-salience recall + deterministic dream cycle. Claude Code and Cursor adapters land in v0.1. Linux and macOS. See [`SPEC.md`](./SPEC.md) for the conformance contract, [`FUTURE.md`](./FUTURE.md) for where this is going, and [`CONTRIBUTING.md`](./CONTRIBUTING.md) to propose changes.

## Getting started

*v0.1 is being implemented. The commands below are the target surface.*

```bash
# scaffold .agent/ into the current project
npx dreamd-mcp init

# point Claude Code, Cursor, or any MCP-aware harness at the server
npx dreamd-mcp
```

Distribution: npm (primary). Cargo and Homebrew paths arrive in v0.1.1. See the [v0.1 milestone](https://github.com/botzrDev/dreamd/milestones).

### Privacy

When LLM mode is enabled in v0.1.1, episodic content meeting the salience threshold may be sent to the configured provider. The `personal/` layer is excluded unless you explicitly opt in. v0.1 makes no network calls. Full disclosure: [`docs/security.md`](./docs/security.md).

## Platforms

v0.1 supports Linux and macOS. Windows lifecycle support arrives in v0.1.1.

## Performance

Recall latency (warm index, 10k entries): **<5ms P50 / <50ms P99 cold-start.** These numbers reflect the query operation itself and will be confirmed by criterion benchmarks in v0.1.

> **Read-after-write visibility:** up to `commit_cadence_seconds` (default 5s). A just-written event becomes recallable within one commit cycle; recall latency itself is unaffected.

Users who need tighter freshness can lower the commit cadence at the cost of higher index churn. User-facing config lands in v0.1.1; the cadence is a constructor argument today.

## Spec

The on-disk layout, JSON schema, salience formula, and dream-cycle contract are defined in [`SPEC.md`](./SPEC.md). The spec is implementation-agnostic — `dreamd` is one implementation, not the only one.

To propose a change to the spec, open an issue prefixed with `[RFC]`. See [CONTRIBUTING.md](./CONTRIBUTING.md).

## v0.1 progress

| Layer | Status |
|---|---|
| `SPEC.md` v0.1-draft | Drafted |
| Reference implementation | Not started — see `src/main.rs` |
| MCP server (`dreamd-mcp`) | Not started |
| CI / cross-platform matrix | Wiring up |
| Conformance test suite | Not started |

## Contributing

See [CONTRIBUTING.md](./CONTRIBUTING.md) for the development setup, commit conventions (`DR-XXX` story IDs), DCO sign-off policy, and the RFC process.

By participating in this project you agree to abide by the [Code of Conduct](./CODE_OF_CONDUCT.md).

To report a security issue, see [SECURITY.md](./SECURITY.md). Do not open a public issue for vulnerabilities.

## License

Apache-2.0. See [LICENSE](./LICENSE) and [NOTICE](./NOTICE).
