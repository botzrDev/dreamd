# dreamd

[![License: Apache 2.0](https://img.shields.io/badge/license-Apache_2.0-blue.svg)](./LICENSE)
![MCP-compatible](https://img.shields.io/badge/MCP-compatible-blueviolet.svg)
[![Platforms](https://img.shields.io/badge/platforms-linux%20%7C%20macOS-lightgrey.svg)](#platforms)
[![Status](https://img.shields.io/badge/v0.1-targeting%20Aug%209-orange.svg)](#status)

**The plain files in your repo are the memory. dreamd is the local server that reads and writes them.**

Drop a `.agent/` folder in the project. Claude Code, Cursor, Cline, and other MCP-aware harnesses share it. What one agent learns, the next already knows. You can `cat`, `grep`, `git diff`, and hand-edit every byte.

This is not "another memory product." It is a storage-model wedge: the filesystem is the source of truth, and the MCP tools (`search_nodes` / `append_node`) are a thin interface over those files.

**Open core:** Apache-2.0 core today, self-hosted only. Premium features may ship later. Do not read this as free-forever for everything.

```bash
npx -y dreamd-mcp init    # scaffold .agent/
npx -y dreamd-mcp         # MCP server (stdio)
```

> First run prompts once. Press `y`, or keep using `npx -y dreamd-mcp`.

---

## The moment it earns its name

```text
~/project $ npx -y dreamd-mcp init

# Claude Code, Tuesday:
you   > axum keeps blowing up when I unwrap in route handlers
claude> filed under rust::error_handling::axum_rejection

# Cursor, Friday, fresh session:
you   > why is this build failing?
cursor> You're unwrapping in a route handler. dreamd has a
        lesson from Tuesday: axum needs IntoResponse on
        custom Error types. Try `?` and a typed error.
```

No re-explaining. No re-pasting. Same `.agent/` folder, every harness.

---

## Install

### npm (recommended)

```bash
npx -y dreamd-mcp init
npx -y dreamd-mcp
```

Requires a project root sentinel (`.git/`, `Cargo.toml`, `package.json`, or `pyproject.toml`).

### Cargo / from source

```bash
git clone https://github.com/botzrDev/dreamd.git
cd dreamd
cargo install --path crates/dreamd-cli
```

See [CONTRIBUTING.md](./CONTRIBUTING.md) for the full dev setup.

---

## Quick start (< 30 seconds)

```bash
cd ~/your-project
npx -y dreamd-mcp init

# Terminal 1: shared daemon (recommended when several agents write)
dreamd watch

# Terminal 2: MCP server for your harness
npx -y dreamd-mcp
```

Ask the agent to search memory for something you just learned. It calls `search_nodes` and recalls prior context.

```bash
cat .agent/episodic/AGENT_LEARNINGS.jsonl
dreamd doctor
```

Adapters: [Claude Code](./adapters/claude-code/README.md) · [Cursor](./adapters/cursor/README.md)

---

## What dreamd writes

| Location | Contents | Commit? |
|---|---|---|
| `<project>/.agent/` | Episodic JSONL, semantic lessons, personal prefs | **Yes** (this is the shared memory) |
| `<project>/.agent/.dreamd/` | Local index, daemon state, config template | No (gitignored by `init`) |
| `~/.agent/registry.toml` | Which projects have a store | No |
| `~/.agent/dreamd.sock` | Daemon API socket (while running) | No |

`dreamd init` is idempotent. To unregister a project: `dreamd init --uninstall-project`.

---

## Architecture (one paragraph)

Agents talk to dreamd over MCP (`search_nodes`, `append_node`). The MCP server proxies to a single-writer daemon (`dreamd watch`) over HTTP on a Unix domain socket, or runs in-process when no daemon is present. The coordinator appends to `AGENT_LEARNINGS.jsonl` and feeds a Tantivy BM25 index. Recall ranks hits with a query-time salience formula (BM25 × age decay × pain × importance × recurrence). Each hit carries `source_harness` and `skill_action`, so recall is attributable across harnesses. The dream cycle consolidates episodic learnings into `LESSONS.md` under WAL protection.

v0.1 recall is deliberately lexical (BM25 + salience). That is a scope choice, not a scoreboard claim. Semantic / embedding recall is out of scope until after v0.1.

Details: [ARCHITECTURE.md](./ARCHITECTURE.md) · [SPEC.md](./SPEC.md) · [docs/http-api.md](./docs/http-api.md)

---

## FAQ

**Is this the first / only cross-harness memory?** No. Other projects exist (including large ones). dreamd owns the storage-model wedge: plain files you already version-control, not a category claim.

**Do I need Rust?** No for the recommended path. `npx -y dreamd-mcp` downloads a prebuilt binary. Rust is only required if you build from source.

**Where does memory live?** In `<project>/.agent/`. The daemon and index under `.agent/.dreamd/` are local and gitignored. You can read and edit the JSONL / Markdown by hand; durable appends should go through the daemon / MCP so the writer stays single-writer.

**What if I want a full wipe?** See [Full fresh store](./docs/troubleshooting.md#how-do-i-reset-or-clear-memory). There is no `dreamd reset --all`. Uninstall steps: [packages/dreamd-mcp/README.md](./packages/dreamd-mcp/README.md#uninstall--reset).

**Windows?** Not in v0.1. Linux and macOS only. Windows lifecycle is planned for v0.1.1.

**Is everything free forever?** Apache-2.0 core is open. Premium may come later. Self-hosted only in v0.1 (no hosted SaaS).

More troubleshooting: [docs/troubleshooting.md](./docs/troubleshooting.md).

---

## Roadmap

| When | What |
|---|---|
| **v0.1** (~2026-08-09) | BM25 lexical recall, Linux + macOS, deterministic dream cycle, npm `dreamd-mcp` |
| **v0.1.1** | Windows lifecycle, semantic / embedding recall, LLM-assisted dream cycle (not claimed in v0.1) |
| **Oct 2026** | State-Drift benchmark publish (dreamd is one row; conflict of interest disclosed) |

v0.1.1 features are intentionally not implemented or documented as shipped in v0.1 code.

---

## Documentation

| Doc | What |
|---|---|
| [GUIDE.md](./GUIDE.md) | 20-minute tutorial walkthrough |
| [docs/README.md](./docs/README.md) | Full documentation index |
| [docs/http-api.md](./docs/http-api.md) | REST API over Unix socket |
| [docs/configuration.md](./docs/configuration.md) | TOML config and env vars |
| [docs/troubleshooting.md](./docs/troubleshooting.md) | Common failures |
| [docs/glossary.md](./docs/glossary.md) | Domain terms |
| [SPEC.md](./SPEC.md) | On-disk contract |
| [ARCHITECTURE.md](./ARCHITECTURE.md) | Engineering decisions |
| [CONTRIBUTING.md](./CONTRIBUTING.md) | Dev setup and RFC process |
| [SECURITY.md](./SECURITY.md) | Threat model |
| [docs/marketing.md](./docs/marketing.md) | Product story and positioning |

Warm recall latency numbers (local Criterion benches) live in [PERF.md](./PERF.md) if you want them. They are not the product pitch.

---

## Status

**v0.1 targeting 2026-08-09.** Daemon commands available today: `init`, `dream`, `doctor`, `mcp`, `watch`, `reset workspace`, `version`. npm package: `dreamd-mcp` (floating: `npx -y dreamd-mcp`). Linux and macOS.

| Layer | Status |
|---|---|
| `SPEC.md` v0.1 | Shipped |
| Reference implementation (daemon, HTTP API, dream cycle, Tantivy recall) | In progress |
| MCP server (`dreamd mcp` + `npx dreamd-mcp` shim) | Shipped on npm |
| CI / cross-platform matrix | Lint, test, build, binary-size gate, DCO |
| Conformance | Reference-impl alpha suites (`scripts/alpha/`); no formal certification in v0.1 |

---

## State-Drift benchmark (Oct 2026)

A separate, reproducible eval measuring whether memory systems correctly update superseded facts. dreamd is one row in the table, published regardless of placement. Conflict of interest is disclosed; configs use each maintainer's documented defaults; raw outputs are committed for audit. Methodology: [scripts/benchmark/README.md](./scripts/benchmark/README.md).

---

## Platforms

v0.1: Linux and macOS. Windows in v0.1.1.

---

## Contributing

See [CONTRIBUTING.md](./CONTRIBUTING.md). By participating you agree to the [Code of Conduct](./CODE_OF_CONDUCT.md). Security reports: [SECURITY.md](./SECURITY.md) (do not open a public issue for vulnerabilities).

## License

Apache-2.0. See [LICENSE](./LICENSE) and [NOTICE](./NOTICE).
