# dreamd

[![License: Apache 2.0](https://img.shields.io/badge/license-Apache_2.0-blue.svg)](./LICENSE)
![MCP-compatible](https://img.shields.io/badge/MCP-compatible-blueviolet.svg)
[![Platforms](https://img.shields.io/badge/platforms-linux%20%7C%20macOS%20%7C%20Windows-lightgrey.svg)](#platforms)
[![Status](https://img.shields.io/badge/version-1.0.0-brightgreen.svg)](#status)

**The plain files in your repo are the memory. dreamd is the local server that reads and writes them.**

Drop a `.agent/` folder in the project. Claude Code, Cursor, Cline, and other MCP-aware harnesses share it. What one agent learns, the next already knows. You can `cat`, `grep`, and `git diff` every byte. Durable appends go through MCP / the daemon so the writer stays single-writer.

This is not "another memory product." It is a storage-model wedge: the filesystem is the source of truth, and the MCP tools (`search_nodes` / `append_node`) are a thin interface over those files.

**Where dreamd fits.** dreamd is one of three sibling repositories that make one product: **dreamOS**, *the first computer you can hand your keys to* — it acts on your behalf, it is structurally incapable of doing what you did not allow, and it can prove what it did. The kernel under it is **momo**; the sandbox beside it is `aegis`. momo's charter (its RFC-009, amended by RFC-011, accepted 2026-09-26) records dreamd as the **Linux-hosted proof** of one of the product's pillars: memory as plain files a person owns, with provenance — in the product's words, *the Memory*. The same amendment names the rule the provenance ledger grows toward — *every effect can be traced to the data that caused it, and no datum can cause an effect above its own trust label* — as a design with its own spec first. Nothing shipped implements a trust label: `source_harness` is asserted by the caller, the provenance ledger ([`docs/provenance.md`](docs/provenance.md)) records derivation, not trust — it is unsigned and nothing gates on it — and [`SECURITY.md`](SECURITY.md) says plainly that injected lessons are not filtered. The momo repository is private until the kernel's first public release, and the product is not announced, previewed or marketed before its v1; this paragraph is a pointer, not a launch.

**Open core:** Apache-2.0 core today, self-hosted only. Premium features may ship later. Do not read this as free-forever for everything.

```bash
npx -y dreamd-mcp setup   # scaffold .agent/ and wire your harness
npx -y dreamd-mcp         # MCP server (stdio) — what the harness spawns
```

> First run prints a local-only privacy disclosure. `setup` prompts for harness choice when it has a TTY.

---

## The moment it earns its name

```text
~/project $ npx -y dreamd-mcp setup

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
npx -y dreamd-mcp setup   # scaffold .agent/ + write the harness MCP config
npx -y dreamd-mcp         # MCP server (stdio) — your harness spawns this
```

stdio stays the default transport. `dreamd mcp --bind 127.0.0.1:<port>` is the opt-in Streamable HTTP server at `/mcp`. It is unauthenticated, and it is only in source builds with `--features mcp-http` — see [docs/mcp-transports.md](./docs/mcp-transports.md).

Requires a project root sentinel (`.git/`, `Cargo.toml`, `package.json`, or `pyproject.toml`).

`setup` prompts when it has a TTY. In scripts and non-interactive shells, pass `--yes --harness claude|cursor|both` (`--harness none` or `--no-write-mcp` scaffolds without touching any MCP config).

### Cargo / from source

```bash
git clone https://github.com/botzrDev/dreamd.git
cd dreamd
cargo install --path crates/dreamd-cli
```

See [CONTRIBUTING.md](./CONTRIBUTING.md) for the full dev setup.

---

## Quick start (< 30 seconds)

If `~/your-project` is a brand-new folder, run `git init` first (or make sure it contains one of the supported root sentinels).

```bash
cd ~/your-project
npx -y dreamd-mcp setup

# Optional: shared daemon (recommended when several agents write)
npx -y dreamd-mcp watch
```

Reload your harness. `setup` already wired the dreamd MCP server, so the harness spawns `npx -y dreamd-mcp` itself — no config to copy by hand.

Ask the agent to search memory for something you just learned. It calls `search_nodes` and recalls prior context.

```bash
cat .agent/episodic/AGENT_LEARNINGS.jsonl
npx -y dreamd-mcp doctor
```

The npm shim does not put `dreamd` on `PATH`. Use `npx -y dreamd-mcp <cmd>` on the npm path, or `cargo install --path crates/dreamd-cli` if you want the `dreamd` binary. The shim forwards only some subcommands (`npx -y dreamd-mcp --help` lists them); `recall`, `score`, `status`, `archive`, and `migrate` need the `dreamd` binary.

Adapters: [Claude Code](./adapters/claude-code/README.md) · [Cursor](./adapters/cursor/README.md)

---

## What dreamd writes

| Location | Contents | Commit? |
|---|---|---|
| `<project>/.agent/` | Episodic JSONL, semantic lessons, personal prefs | **Yes** (this is the shared memory) |
| `<project>/.agent/.dreamd/` | Local index, daemon state, config template, memory branches, provenance ledger | No (gitignored by `init`) |
| `~/.agent/registry.toml` | Which projects have a store | No |
| `~/.agent/dreamd.sock` | Daemon API socket (while running) | No |

`npx -y dreamd-mcp setup` (or `dreamd setup` after a cargo install) scaffolds the store by calling `init`, then writes the harness MCP config. `init` is the scaffold primitive and still works on its own when you want the store without touching any MCP config. Both are idempotent. To uninstall from a machine — stop local servers, remove the socket, unregister the current project, clear caches — run `npx -y dreamd-mcp uninstall` (project `.agent/` stores are left in place). Advanced, registry-only: `npx -y dreamd-mcp init --uninstall-project` unregisters the current project and touches nothing else.

---

## Inspecting and editing memory

The MCP surface is two tools. Everything else is CLI, run against the same files. These need the `dreamd` binary unless noted; `dreamd <cmd> --help` is the reference for flags.

| Command | What it does |
|---|---|
| `dreamd recall <query>` | Ranked search as a markdown table. `--explain` prints the salience factors per hit; `--without <id>` / `--without-cluster <prefix>` drop hits before top-k (1.0.0). |
| `dreamd blame <query>` | Which stored memories drove a query's ranking: id, timestamp, `skill_action`, bm25, salience, total, layer. `--json` for one compact array. New in 1.0.0; forwarded by the npm shim. |
| `dreamd score` | Top memories by salience alone, no query. |
| `dreamd salience-drift` | Salience distribution, top clusters, and drift against the snapshot from exactly seven days earlier. New in 1.0.0; forwarded by the npm shim. See [docs/observability.md](./docs/observability.md). |
| `dreamd dream --dry` | Print the `LESSONS.md` the next cycle would write, without writing. |
| `dreamd memory branch\|checkout\|branches\|delete\|diff\|bisect` | Name snapshots of the memory files, switch between them, diff two, and binary-search the automatic `snap-*` snapshots. New in 1.0.0; forwarded by the npm shim. `checkout` and `bisect` refuse while `dreamd watch` holds the live log: on Linux and macOS while the socket file exists (including a stale socket), on Windows while a loopback connect to `server.json` succeeds. See [docs/branching-guide.md](./docs/branching-guide.md). |
| `dreamd forget <event-id>` | Remove one episodic event and the lesson state that names it. `--dry-run` writes nothing; `--proof <path>` writes a `forget-receipt/1.0` JSON file (a receipt, not a signed proof). Refuses while the daemon is running. New in 1.0.0; forwarded by the npm shim. |
| `dreamd doctor --provenance` | Recompute the provenance ledger's Merkle root and list orphan edges, missing edges, and corrupt lines. Read-only. New in 1.0.0. See [docs/provenance.md](./docs/provenance.md). |

---

## Architecture (one paragraph)

Agents talk to dreamd over MCP (`search_nodes`, `append_node`). On Linux and macOS the MCP server proxies to a single-writer daemon (`dreamd watch`) over HTTP on a Unix domain socket, or runs in-process when no daemon is present. On Windows it proxies to loopback `dreamd watch` and exits 2 when that daemon is down; there is no in-process server. The coordinator appends to `AGENT_LEARNINGS.jsonl` and feeds a Tantivy BM25 index. Recall ranks hits with a query-time salience formula (BM25 × age decay × pain × importance × recurrence). Each hit carries `source_harness` and `skill_action`, so recall is attributable across harnesses. The dream cycle consolidates episodic learnings into `LESSONS.md` under WAL protection.

Recall is deliberately lexical (BM25 + salience), including the `LESSONS.md` document layer indexed alongside episodic events. That is a scope choice, not a scoreboard claim. Vector / embedding recall is not shipped: the optional `vectors` cargo feature (off by default, not in release binaries) only downloads a model, and nothing ranks with it ([docs/vectors.md](./docs/vectors.md)).

Details: [ARCHITECTURE.md](./ARCHITECTURE.md) · [SPEC.md](./SPEC.md) · [docs/http-api.md](./docs/http-api.md)

---

## FAQ

**Is this the first / only cross-harness memory?** No. Other projects exist (including large ones). dreamd owns the storage-model wedge: plain files you already version-control, not a category claim.

**Do I need Rust?** No for the recommended path. `npx -y dreamd-mcp` downloads a prebuilt binary. Rust is only required if you build from source.

**Where does memory live?** In `<project>/.agent/`. The daemon and index under `.agent/.dreamd/` are local and gitignored. You can read and edit the JSONL / Markdown by hand; durable appends should go through the daemon / MCP so the writer stays single-writer.

**What if I want a full wipe?** See [Full fresh store](./docs/troubleshooting.md#how-do-i-reset-or-clear-memory). There is no `dreamd reset --all`. To uninstall dreamd itself, run `dreamd uninstall` — details: [packages/dreamd-mcp/README.md](./packages/dreamd-mcp/README.md#uninstall--reset). That stops running processes and clears caches; removing the per-user *service* entry (the systemd unit, the LaunchAgent, or the Windows Task Scheduler logon task) is the separate `dreamd service uninstall`, whose optional `--purge` deletes the daemon home `~/.agent/` and never a per-project `.agent/` store ([docs/install.md](./docs/install.md)).

**Windows?** Partial. `dreamd watch` serves the HTTP API on loopback TCP with a bearer token from `auth.json`; `POST /api/v1/learn` works. The dream cycle and Tantivy index do not — `io::write_atomic` is still `Unsupported` there. For consolidate-and-search, use WSL2 or a Linux/macOS host. The npm shim downloads prebuilt binaries for Linux x86_64 and macOS (x86_64 / arm64) only, so native Windows means a `cargo` build. Details: [docs/windows.md](./docs/windows.md).

**Is everything free forever?** Apache-2.0 core is open. Premium may come later. Self-hosted only (no hosted SaaS).

More troubleshooting: [docs/troubleshooting.md](./docs/troubleshooting.md).

---

## Roadmap

| When | What |
|---|---|
| **v0.1.0** (2026-08-05) | BM25 lexical recall, Linux + macOS, deterministic dream cycle, npm `dreamd-mcp` |
| **v0.1.1** (2026-09-14) | LLM dream cycle, `LESSONS.md` semantic layer (still BM25 × salience, not embeddings), Windows watch+learn, `dreamd service` on Linux / macOS / Windows |
| **v1.0.0** (2026-10-02) | Stable release of the tree since v0.1.1: `dreamd memory` branches / diff / bisect, provenance ledger + `doctor --provenance`, `dreamd forget`, `dreamd blame`, `recall --without`, `salience-drift`, `explain=1` citations on HTTP recall. Recall is still BM25 × salience |
| **Next** | Windows atomic writes (dream cycle + index), vector recall, WasTrue benchmark publish |

---

## Documentation

| Doc | What |
|---|---|
| [GUIDE.md](./GUIDE.md) | 20-minute tutorial walkthrough |
| [docs/README.md](./docs/README.md) | Full documentation index |
| [docs/http-api.md](./docs/http-api.md) | REST API (Unix socket / Windows loopback TCP) |
| [docs/configuration.md](./docs/configuration.md) | TOML config and env vars |
| [docs/troubleshooting.md](./docs/troubleshooting.md) | Common failures |
| [docs/glossary.md](./docs/glossary.md) | Domain terms |
| [SPEC.md](./SPEC.md) | On-disk contract |
| [docs/salience.md](./docs/salience.md) | Salience formula deep-dive |
| [docs/branching-guide.md](./docs/branching-guide.md) | Memory branches: branch, checkout, diff, bisect |
| [docs/provenance.md](./docs/provenance.md) | Provenance ledger format and what `doctor --provenance` checks |
| [docs/observability.md](./docs/observability.md) | Recall citations (`explain=1`) and salience drift |
| [docs/spec/](./docs/spec/README.md) | Two-page `.agent/` digest of that contract |
| [ARCHITECTURE.md](./ARCHITECTURE.md) | Engineering decisions |
| [CONTRIBUTING.md](./CONTRIBUTING.md) | Dev setup and RFC process |
| [SECURITY.md](./SECURITY.md) | Threat model |
| [docs/marketing.md](./docs/marketing.md) | Product story and positioning |

Warm recall latency numbers (local Criterion benches) live in [PERF.md](./PERF.md) if you want them. They are not the product pitch. To re-run the bench yourself, see [docs/benchmarks.md](./docs/benchmarks.md).

---

## Status

**v1.0.0 is released** (GitHub tag `v1.0.0`, 2026-10-02; npm `dreamd-mcp` 1.0.0 on `latest` and `next`). The tag adds no behavior of its own ([CHANGELOG.md](./CHANGELOG.md)). The MCP Registry entry `io.github.botzrDev/dreamd` is a separate human publish ([RELEASING.md](./RELEASING.md)). Commands marked "new in 1.0.0" above need 1.0.0 or later. CLI commands: `setup`, `init`, `watch`, `mcp`, `dream`, `doctor`, `status`, `service`, `recall`, `blame`, `score`, `salience-drift`, `memory`, `forget`, `archive`, `migrate`, `reset workspace`, `vectors`, `uninstall`, `update`, `version` (`dreamd --help` is the full list; on the npm path use `npx -y dreamd-mcp <cmd>` — the shim forwards a subset, see [packages/dreamd-mcp/README.md](./packages/dreamd-mcp/README.md)). `vectors enable` exits 2 on the default build and does not change recall. Linux, macOS, and partial Windows (watch + learn; no dream cycle or index). If you upgrade a cargo-installed binary in place (`cargo install --path crates/dreamd-cli`) and run the daemon under the per-user service, bounce it afterwards with `dreamd service restart` so the supervisor picks up the new binary ([docs/install.md](./docs/install.md)).

| Layer | Status |
|---|---|
| `SPEC.md` v0.1 | Shipped |
| Reference implementation (daemon, HTTP API, dream cycle, Tantivy recall) | Shipped |
| MCP server (`dreamd mcp` + `npx dreamd-mcp` shim) | Shipped on npm |
| CI / cross-platform matrix | Lint, test, build, binary-size gate, DCO (Windows jobs are informational) |
| Conformance | Reference-impl alpha suites (`scripts/alpha/`); no formal certification |

---

## WasTrue benchmark

A separate eval of whether memory systems update a fact that later changes. The harness in this repository is a scaffold, and a results table is not published here. Publishing that table is listed under Next above. Methodology for the scaffold: [scripts/benchmark/README.md](./scripts/benchmark/README.md).

---

## Platforms

Linux and macOS (full). Windows is watch + learn over loopback TCP; the dream cycle and Tantivy index stay Unix-only until atomic writes land. See [docs/windows.md](./docs/windows.md).

---

## Contributing

See [CONTRIBUTING.md](./CONTRIBUTING.md). By participating you agree to the [Code of Conduct](./CODE_OF_CONDUCT.md). Security reports: [SECURITY.md](./SECURITY.md) (do not open a public issue for vulnerabilities).

## License

Apache-2.0. See [LICENSE](./LICENSE) and [NOTICE](./NOTICE).
