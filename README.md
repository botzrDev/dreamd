# dreamd

> **Status: pre-release.** The reference implementation does not yet exist. The current `src/main.rs` is a stub. The spec ([`SPEC.md`](./SPEC.md)) is the contract; everything else is in flight. Track progress in [issues](https://github.com/botzrDev/dreamd/issues) and the [changelog](./CHANGELOG.md). Public APIs and on-disk formats may change before v0.1.

A local-first, single-binary daemon that gives AI coding agents a portable memory layer.

`dreamd` is the reference implementation of [`.agent/`](./SPEC.md) — a directory convention any AI coding agent or harness can read and write to share runtime memory across tools. It sits beside [`AGENTS.md`](https://agents.md) and [`SKILL.md`](https://www.anthropic.com/news/skills) in a project root.

> **AGENTS.md is what you wrote down. `.agent/` is what your agent learned.**

## What it does

- Exposes a standardized `.agent/` folder (`working/`, `episodic/`, `semantic/`, `personal/`) as the source of truth — plain markdown and JSONL files you can read, edit, and check into git.
- Serves a small local HTTP API for `learn` (append an episode), `recall` (BM25 × salience search), and `dream` (consolidate episodic into semantic lessons).
- Ships an MCP server so Claude Code, Cursor, OpenCode, Aider, Continue, Cline, and any other MCP-aware harness can share memory across sessions and tools.

## Install (planned)

The primary distribution surface is the MCP server:

```bash
npx dreamd-mcp
```

Standalone binary and Cargo install paths arrive with v0.1. See the [v0.1 milestone](https://github.com/botzrDev/dreamd/milestones).

## Quickstart (planned)

```bash
# scaffold .agent/ into the current project
dreamd init

# start the daemon
dreamd serve

# from any AI coding agent that supports MCP, point it at dreamd-mcp
```

## Spec

The on-disk layout, JSON schema, salience formula, and dream-cycle contract are defined in [`SPEC.md`](./SPEC.md). The spec is implementation-agnostic — `dreamd` is one implementation, not the only one.

To propose a change to the spec, open an issue prefixed with `[RFC]`. See [CONTRIBUTING.md](./CONTRIBUTING.md).

## Project status

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
