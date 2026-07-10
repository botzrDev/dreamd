# dreamd

[![License: Apache 2.0](https://img.shields.io/badge/license-Apache_2.0-blue.svg)](./LICENSE)
![MCP-compatible](https://img.shields.io/badge/MCP-compatible-blueviolet.svg)
[![Platforms](https://img.shields.io/badge/platforms-linux%20%7C%20macOS-lightgrey.svg)](#platforms)
[![Status](https://img.shields.io/badge/v0.1-in%20progress-orange.svg)](#v01-progress)

**A local-first memory layer for AI coding agents.**

dreamd gives multiple coding agents — Claude Code, Cursor, Cline — a shared memory format. One `.agent/` folder in your repo. Every agent reads and writes to it. Sessions, learnings, and skills in plain files, version-controlled alongside your code.

Every coding agent ships its own memory format. dreamd is what they could share.

AGENTS.md is what you wrote down. dreamd is what your agent learned, across every tool.

Watch this repo for v0.1 release notifications (Aug 9).

---

## Install

### npm (recommended)

```bash
npx dreamd-mcp@0.1.0-rc.2 init    # scaffold .agent/ in your project
npx dreamd-mcp@0.1.0-rc.2         # MCP server (stdio)
```

Requires a project root sentinel (`.git/`, `Cargo.toml`, `package.json`, or `pyproject.toml`).

### Cargo

```bash
cargo install --path crates/dreamd-cli   # from a clone
# or, when published: cargo install dreamd
```

### From source

```bash
git clone https://github.com/botzrDev/dreamd.git
cd dreamd
cargo install --path crates/dreamd-cli
```

See [CONTRIBUTING.md](./CONTRIBUTING.md) for the full dev setup.

---

## Quick start (< 30 seconds)

```bash
cd ~/your-project          # must contain a repo root sentinel
npx dreamd-mcp@0.1.0-rc.2 init

# Terminal 1 — shared daemon (recommended for multiple agents)
dreamd watch

# Terminal 2 — point your harness at the MCP server
npx dreamd-mcp@0.1.0-rc.2
```

In Claude Code, Cursor, or Cline (experimental): ask the agent to search memory for something you just learned. It calls `search_nodes` over MCP and recalls prior context.

Verify the store:

```bash
cat .agent/episodic/AGENT_LEARNINGS.jsonl
dreamd doctor
```

Adapter-specific setup: [adapters/claude-code](./adapters/claude-code/README.md) · [adapters/cursor](./adapters/cursor/README.md)

---

## What dreamd writes

| Location | Contents | Commit? |
|---|---|---|
| `<project>/.agent/` | Episodic JSONL, semantic lessons, personal prefs | **Yes** — this is your shared memory |
| `<project>/.agent/.dreamd/` | Local index, daemon state, config template | No — gitignored by `init` |
| `~/.agent/registry.toml` | Which projects have a store | No |
| `~/.agent/dreamd.sock` | Daemon API socket (while running) | No |

`dreamd init` is idempotent. To unregister a project: `dreamd init --uninstall-project`.

---

## Architecture (one paragraph)

Agents talk to dreamd over MCP (`search_nodes`, `append_node`). The MCP server proxies to a single-writer daemon (`dreamd watch`) over HTTP on a Unix domain socket, or runs in-process when no daemon is present. The coordinator appends to `AGENT_LEARNINGS.jsonl` and feeds a Tantivy BM25 index. Recall ranks hits with a query-time salience formula (BM25 × age decay × pain × importance × recurrence), and each hit carries its `source_harness` and `skill_action` — so recall is cross-harness-attributable, not an opaque lookup. The dream cycle consolidates episodic learnings into `LESSONS.md` under WAL protection.

Details: [ARCHITECTURE.md](./ARCHITECTURE.md) · [SPEC.md](./SPEC.md) · [docs/http-api.md](./docs/http-api.md)

---

## Performance

Recall latency (warm in-RAM index, Criterion 0.5, WSL2/Linux):

| Corpus size | Mean (warm) |
|---|---|
| 1 000 entries   | ~50 µs |
| 10 000 entries  | ~313 µs |
| 100 000 entries | ~2.8 ms |

_Criterion reports mean across 100 samples; used here as the P50 proxy. All three sizes are well under the `<5ms P50 warm` NFR. Run `cargo bench -p dreamd-core` to reproduce._

> **Read-after-write visibility:** up to 5 seconds (the index commit cadence). A just-written event becomes recallable within one commit cycle; recall latency itself is unaffected.

---

## Documentation

| Doc | What |
|---|---|
| [GUIDE.md](./GUIDE.md) | 20-minute tutorial walkthrough |
| [docs/README.md](./docs/README.md) | Full documentation index |
| [docs/http-api.md](./docs/http-api.md) | REST API over Unix socket |
| [docs/configuration.md](./docs/configuration.md) | TOML config and env vars |
| [docs/troubleshooting.md](./docs/troubleshooting.md) | FAQ — common failures |
| [docs/glossary.md](./docs/glossary.md) | Domain terms |
| [docs/ci.md](./docs/ci.md) | CI pipeline and local reproduction |
| [SPEC.md](./SPEC.md) | On-disk contract |
| [ARCHITECTURE.md](./ARCHITECTURE.md) | Engineering decisions |
| [CONTRIBUTING.md](./CONTRIBUTING.md) | Dev setup and RFC process |
| [SECURITY.md](./SECURITY.md) | Threat model |
| [docs/marketing.md](./docs/marketing.md) | Product story and positioning |

---

## Status

**v0.1 in active development — targeting 2026-08-09.** The daemon builds and runs locally today: `dreamd init`, `dreamd dream`, `dreamd doctor`, `dreamd mcp`, `dreamd watch`, `dreamd reset workspace`, and `dreamd version`. The `npx dreamd-mcp` install path is live on npm as `dreamd-mcp@0.1.0-rc.2`. Linux and macOS.

### v0.1 progress

| Layer | Status |
|---|---|
| `SPEC.md` v0.1 | Shipped |
| Reference implementation (daemon, HTTP API, dream cycle, Tantivy recall) | In progress |
| MCP server (`dreamd mcp` + `npx dreamd-mcp` shim) | Shipped — `dreamd-mcp@0.1.0-rc.2` on npm |
| CI / cross-platform matrix | Lint, test, cross-platform build, binary-size gate, DCO check |
| Conformance test suite | Reference-impl suites shipped (`scripts/alpha/`); no formal certification in v0.1 |

---

## State-Drift benchmark

In October 2026, we're publishing a neutral, reproducible benchmark measuring whether AI memory systems correctly update superseded facts — the specific failure where a system "knows" you moved from London to Tokyo but keeps confidently answering "London" because the old fact was never retired.

The benchmark runs Mem0, Zep, Letta, Anthropic's memory, and dreamd through identical scenarios. The same probe question is asked after a fact has changed. Every system is scored by a deterministic oracle — not an LLM judge — so results are identical across runs and independent of model version. dreamd is one row in the results table, published regardless of where it places.

**Transparency:** dreamd's developer built and maintains this benchmark, which creates an obvious conflict of interest. We've taken explicit steps to address it: every system is configured using its maintainer's documented recommended settings; we contact Mem0/Zep/Letta maintainers before publication to verify our configurations; raw per-question outputs for every system are committed to the repo so you can audit every individual answer; and an external engineer independently reproduces results before we publish. There is a scenario subset where dreamd's BM25 retrieval loses to paraphrase-heavy probes — that result is in the table.

The methodology and bake-off harness live in [scripts/benchmark/README.md](./scripts/benchmark/README.md); the public results repo will be linked here when it publishes in October. If you maintain a memory system and want to verify how we configured yours before we publish, open an issue.

---

## Platforms

v0.1 supports Linux and macOS. Windows lifecycle support arrives in v0.1.1.

---

## Contributing

See [CONTRIBUTING.md](./CONTRIBUTING.md) for development setup, commit conventions, DCO sign-off, and the RFC process.

By participating you agree to the [Code of Conduct](./CODE_OF_CONDUCT.md). To report a security issue, see [SECURITY.md](./SECURITY.md) — do not open a public issue for vulnerabilities.

## License

Apache-2.0. See [LICENSE](./LICENSE) and [NOTICE](./NOTICE).
