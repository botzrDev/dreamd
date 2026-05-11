# dreamd

> **Status: pre-release.** The reference implementation does not yet exist. The current `src/main.rs` is a stub. The spec ([`SPEC.md`](./SPEC.md)) is the contract; everything else is in flight. Track progress in [issues](https://github.com/botzrDev/dreamd/issues) and the [changelog](./CHANGELOG.md). Public APIs and on-disk formats may change before v0.1.

A local-first memory layer for AI coding agents. Ships as an MCP server (`npx dreamd-mcp`) so any MCP-aware harness can read and write a shared `.agent/` folder. A standalone binary and persistent-daemon mode are also available.

`dreamd` is the reference implementation of [`.agent/`](./SPEC.md) — a directory convention any AI coding agent or harness can read and write to share runtime memory across tools. It sits beside [`AGENTS.md`](https://agents.md) and [`SKILL.md`](https://www.anthropic.com/news/skills) in a project root.

> **AGENTS.md is what you wrote down. `.agent/` is what your agent learned.**

## What it does

- Exposes a standardized `.agent/` folder as the source of truth — plain markdown and JSONL files you can read, edit, and check into git. v0.1 ships four directories (`episodic/`, `semantic/`, `personal/`, `working/`); `episodic/` and `semantic/` are the two active memory systems, `personal/` is user-authored static context, and `working/` is reserved for v0.2's session model.
- Serves a small local API for `learn` (append an episodic event), `recall` (BM25 × salience search over episodic events), and `cycle` (run a deterministic dream cycle that promotes recurring events into `LESSONS.md`). LLM-assisted consolidation and blended episodic + semantic recall arrive in v0.1.1.
- Ships an MCP server so Claude Code and any other MCP-aware harness can share memory across sessions and tools. Additional adapters (Cursor, OpenCode, Aider, Continue, Cline) land progressively after v0.1.

> *AGENTS.md belongs to a project; PREFERENCES.md belongs to you. Same agent reads both; different ownership.*

## Install (planned)

The primary distribution surface is the MCP server:

```bash
npx dreamd-mcp
```

Standalone binary and Cargo install paths arrive with v0.1. See the [v0.1 milestone](https://github.com/botzrDev/dreamd/milestones).

### Privacy

> When LLM mode is enabled, the content of `AGENT_LEARNINGS.jsonl` entries meeting the salience threshold is sent to the configured LLM provider. No data is sent in `--no-llm` mode. Users working with sensitive codebases should use `--no-llm` or a local model via Ollama. The `personal/` layer is excluded from LLM calls unless `--share-personal` is passed.

LLM mode ships in v0.1.1; v0.1 makes no network calls. Full disclosure, redaction details, and the v0.1.1 contract: [`docs/security.md#privacy-disclosure`](./docs/security.md#privacy-disclosure).

## Quickstart (planned)

```bash
# scaffold .agent/ into the current project
dreamd init

# point any MCP-aware harness at dreamd-mcp; the writer process self-starts
npx dreamd-mcp
```

## Platforms

v0.1 supports Linux and macOS. Windows lifecycle support arrives in v0.1.1.

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
