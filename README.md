# dreamd

[![License: Apache 2.0](https://img.shields.io/badge/license-Apache_2.0-blue.svg)](./LICENSE)
![MCP-compatible](https://img.shields.io/badge/MCP-compatible-blueviolet.svg)
[![Platforms](https://img.shields.io/badge/platforms-linux%20%7C%20macOS%20%7C%20Windows-lightgrey.svg)](#platforms)
[![Status](https://img.shields.io/badge/version-1.2.0-brightgreen.svg)](#status)

[![dreamd MCP server](https://mcprush.com/badge/dreamd-mcp/card.svg)](https://mcprush.com/to/dreamd-mcp?ref=badge)

**Agent memory you can audit. When a memory goes bad, trace it to its source, see what it fed, and remove it.**

dreamd is local-first, long-term memory for AI agents. Every memory is a line in a file in your repo, every lesson your agents learn cites the events it came from, and each dream cycle commits its lessons to git. When an agent acts on something wrong, you can find the memory that caused it, see which lessons it fed, and take it out, with a receipt.

```bash
npx -y dreamd-mcp setup   # scaffold .agent/ and wire your harness
npx -y dreamd-mcp         # MCP server (stdio): what the harness spawns
```

Works today with Claude Code, Cursor, Cline and other MCP-aware harnesses. Self-hosted, single binary, Apache-2.0 core.

---

## Why auditable memory

Memory is the layer of an agent stack you usually can't see into. A prompt can be read and a model can be swapped, but when memory goes wrong there is rarely a way to tell what the agent believed, where that belief came from, or what else it touched.

That matters more now that memory is an attack surface. The OWASP Top 10 for Agentic Applications lists **memory and context poisoning (ASI06)**: an attacker, a bad tool result or a plain mistake plants a "fact" that the agent keeps acting on in later sessions. The defenses OWASP points to start with knowing the source of every memory and being able to remove one cleanly ([OWASP agentic top 10](https://genai.owasp.org/resource/owasp-top-10-for-agentic-applications-for-2026)).

dreamd is built for that response:

| You need to | dreamd gives you |
|---|---|
| See what drove an answer | `dreamd blame <query>` lists the stored memories behind a recall, with ids, timestamps, source and score. `dreamd recall --explain` prints the salience factors for each hit |
| Know where a lesson came from | Every lesson in `LESSONS.md` cites the episodic events it was promoted from, and the provenance ledger records which event fed which lesson and index entry |
| Check the record is consistent | `dreamd doctor --provenance` recomputes the ledger's Merkle root and lists orphan, missing and corrupt edges. Read-only |
| Remove a bad memory | `dreamd forget <event-id>` removes the event and the lesson state that names it. `--dry-run` shows what would change; `--proof <path>` writes a `forget-receipt/1.0` file |
| Review what agents learned | `.agent/` is committed with your code, and each dream cycle commits its output as `dreamd: cycle YYYY-MM-DD`. Review new lessons with `git diff` or in a pull request, and undo a cycle with `git revert` |
| Read everything yourself | Plain JSONL and Markdown. `cat`, `grep` and `git log` work on every byte |

**What dreamd does not do.** It does not detect or block poisoned writes, and it does not filter injected lesson content ([SECURITY.md](./SECURITY.md)). `source_harness` is asserted by the caller, not verified. The provenance ledger and the forget receipt are unsigned, and there are no trust labels yet. dreamd makes bad memory traceable and removable; it does not make it impossible. See [Roadmap](#roadmap) for what comes next.

---

## The audit loop in four commands

Say an agent keeps repeating advice you know is wrong.

```bash
# 1. Find the memories behind it
npx -y dreamd-mcp blame "deploy without tests" --explain

# 2. Check the lesson it came from and what that lesson cites
cat .agent/semantic/LESSONS.md
npx -y dreamd-mcp doctor --provenance

# 3. Preview, then remove the bad event (stop `dreamd watch` first; forget refuses while it runs)
npx -y dreamd-mcp forget evt_... --dry-run
npx -y dreamd-mcp forget evt_... --proof forget-receipt.json

# 4. Review the change like code
git diff .agent/
```

After a `forget`, `doctor --provenance` reports the removed event's old ledger edges as orphans and exits 1. That is expected: the ledger is append-only, so it keeps the record that the event once existed.

To undo a whole dream cycle instead, `git revert` its `dreamd: cycle` commit with the daemon stopped, then run `dreamd doctor --repair` to rebuild the recall index.

---

## Install

### npm (recommended)

```bash
npx -y dreamd-mcp setup   # scaffold .agent/ and write the harness MCP config
npx -y dreamd-mcp         # MCP server (stdio): your harness spawns this
```

stdio is the default transport. `dreamd mcp --bind 127.0.0.1:<port>` is an opt-in Streamable HTTP server at `/mcp`. It is unauthenticated, and it is only in source builds with `--features mcp-http`. See [docs/mcp-transports.md](./docs/mcp-transports.md).

dreamd needs a project root sentinel (`.git/`, `Cargo.toml`, `package.json` or `pyproject.toml`).

`setup` prompts when it has a TTY. In scripts and non-interactive shells, pass `--yes --harness claude|cursor|both` (`--harness none` or `--no-write-mcp` scaffolds without touching any MCP config).

### Cargo / from source

```bash
git clone https://github.com/botzrDev/dreamd.git
cd dreamd
cargo install --path crates/dreamd-cli
```

See [CONTRIBUTING.md](./CONTRIBUTING.md) for the full dev setup.

---

## Quick start (under a minute)

If `~/your-project` is a brand-new folder, run `git init` first.

```bash
cd ~/your-project
npx -y dreamd-mcp setup

# Optional: shared daemon, recommended when several agents write
npx -y dreamd-mcp watch
```

Reload your harness. `setup` already wired the dreamd MCP server, so the harness spawns `npx -y dreamd-mcp` itself.

Ask the agent to search memory for something it learned earlier. It calls `search_nodes` and recalls prior context, with the source harness on every hit.

```bash
cat .agent/episodic/AGENT_LEARNINGS.jsonl
npx -y dreamd-mcp dream --dry   # preview the lesson the next cycle would write
npx -y dreamd-mcp doctor
```

The npm shim does not put `dreamd` on `PATH`. Use `npx -y dreamd-mcp <cmd>` on the npm path, or `cargo install --path crates/dreamd-cli` for the `dreamd` binary. The shim forwards a subset of commands (`npx -y dreamd-mcp --help` lists them); `recall`, `score`, `status`, `archive` and `migrate` need the `dreamd` binary.

Adapters: [Claude Code](./adapters/claude-code/README.md) · [Cursor](./adapters/cursor/README.md) · [Cline](./adapters/cline/README.md)

![Claude Code saves a lesson to dreamd; OpenCode, in a fresh session, finds it with search_nodes](docs/assets/demo.gif)

Recorded live, sped up 1.5x: Claude Code writes the lesson, OpenCode reads it back through the same `.agent/` folder. Tape: [docs/assets/demo.tape](docs/assets/demo.tape).

---

## How agents learn

Agents write episodes (what happened, how painful it was, how important) through MCP `append_node`. The **dream cycle** clusters episodes by `skill_action` and promotes a recurring pattern into a lesson in `LESSONS.md`. With an API key it composes the lesson with an LLM under a $0.10 per-cycle spend cap; without one it writes a deterministic lesson and makes no network call. Either way the lesson cites the events it came from, and every output is staged and committed through one redo WAL, so a crash leaves the store as it was or rolls the cycle forward.

Recall ranks episodes and lessons with BM25 × salience (age decay, pain, importance, recurrence). Recall is deliberately lexical; vector recall is not shipped ([docs/vectors.md](./docs/vectors.md)). Formula: [docs/salience.md](./docs/salience.md).

---

## What dreamd writes

| Location | Contents | Commit? |
|---|---|---|
| `<project>/.agent/` | Episodic JSONL, semantic lessons, personal prefs | **Yes** (this is the shared memory) |
| `<project>/.agent/.dreamd/` | Local index, daemon state, config template, provenance ledger | No (gitignored by `init`) |
| `~/.agent/registry.toml` | Which projects have a store | No |
| `~/.agent/dreamd.sock` | Daemon API socket (while running) | No |

`setup` scaffolds the store by calling `init`, then writes the harness MCP config. Both are idempotent. To uninstall from a machine, run `npx -y dreamd-mcp uninstall` (project `.agent/` stores are left in place). `npx -y dreamd-mcp init --uninstall-project` drops only the registry entry; `dreamd init` puts it back and does not rewrite the store.

---

## All inspection commands

The MCP surface is two tools (`search_nodes`, `append_node`). Everything else is CLI over the same files. `dreamd <cmd> --help` is the reference for flags.

| Command | What it does |
|---|---|
| `dreamd recall <query>` | Ranked search as a markdown table. `--explain` prints the salience factors per hit; `--without <id>` / `--without-cluster <prefix>` drop hits before top-k, so you can see the answer without a suspect memory |
| `dreamd blame <query>` | Which stored memories drove a query's ranking: id, timestamp, `skill_action`, bm25, salience, total, layer. `--json` for one compact array |
| `dreamd score` | Top memories by salience alone, no query |
| `dreamd salience-drift` | Salience distribution, top clusters, and drift against the snapshot from seven days earlier. See [docs/observability.md](./docs/observability.md) |
| `dreamd dream --dry` | Print the `LESSONS.md` the next cycle would write, without writing |
| `dreamd forget <event-id>` | Remove one episodic event and the lesson state that names it. `--dry-run` writes nothing; `--proof <path>` writes a `forget-receipt/1.0` file (a receipt, not a signed proof). Refuses while the daemon is running |
| `dreamd doctor --provenance` | Recompute the provenance ledger's Merkle root and list orphan edges, missing edges and corrupt lines. Read-only. See [docs/provenance.md](./docs/provenance.md) |
| `dreamd memory branch\|checkout\|branches\|delete\|diff\|bisect` | Deprecated in 1.1.0, removed in 2.0. `.agent/` is plain files, so use git. See [docs/branching-guide.md](./docs/branching-guide.md) |

---

## Architecture (one paragraph)

Agents talk to dreamd over MCP. On Linux and macOS the MCP server proxies to a single-writer daemon (`dreamd watch`) over HTTP on a Unix domain socket, or runs in-process when no daemon is present. On Windows it proxies to loopback `dreamd watch` and exits 2 when that daemon is down. One writer lock covers every process that writes a project. The coordinator appends to `AGENT_LEARNINGS.jsonl` and feeds a Tantivy BM25 index; recall ranks hits with the salience formula, and each hit carries `source_harness` and `skill_action`. Details: [ARCHITECTURE.md](./ARCHITECTURE.md) · [SPEC.md](./SPEC.md) · [docs/http-api.md](./docs/http-api.md). Warm recall latency (local Criterion benches) is in [PERF.md](./PERF.md).

---

## Roadmap

Direction, not dated commitments. Full view: [ROADMAP.md](./ROADMAP.md).

- **Trust labels.** A label on every memory saying where it came from and how far to trust it, joined along provenance edges, so a lesson from an untrusted source can't drive a privileged action. Needs its own spec first.
- **Signed provenance.** Signed forget receipts and membership proofs, as specified in [docs/provenance.md](./docs/provenance.md).
- **Proof of learning.** Publish the WasTrue benchmark results (does memory update a fact that later changed?) and measure whether learned lessons actually improve task success over time.
- **Vector recall.** Hybrid lexical and vector scoring, as derived state over the natural-language record.
- **Robots and edge devices.** dreamd is one local binary with no cloud dependency, which is the shape an offline robot needs. No robot integration ships today.

### Release history

| Version | What |
|---|---|
| **v1.2.0** (2026-10-29) | Windows dream cycle and Tantivy index; audit fixes (writer lock, atomic dream cycle, faithful log rewrites, index coverage); deprecates `dreamd memory` branch commands, `vectors enable` and `dream --auto` for removal in 2.0. No 1.1.0 tag |
| **v1.0.1** (2026-10-21) | Dependency security fixes |
| **v1.0.0** (2026-10-02) | Provenance ledger and `doctor --provenance`, `dreamd forget`, `dreamd blame`, `recall --without`, `salience-drift`, `explain=1` citations on HTTP recall |
| **v0.1.1** (2026-09-14) | LLM dream cycle, `LESSONS.md` semantic layer, Windows watch and learn, `dreamd service` |
| **v0.1.0** (2026-08-05) | BM25 lexical recall, Linux and macOS, deterministic dream cycle, npm `dreamd-mcp` |

Full notes: [CHANGELOG.md](./CHANGELOG.md).

---

## FAQ

**Other tools store memory as files too. What's different?** Files are the substrate, not the feature. dreamd adds the trace: lessons that cite their source events, a provenance ledger you can check, `blame` for any recall, and `forget` that removes an event together with the lesson state built from it.

**Does dreamd stop memory poisoning?** No. It makes a poisoned memory findable and removable, and it keeps a record. It does not screen what agents write. See [SECURITY.md](./SECURITY.md).

**Do I need Rust?** No. `npx -y dreamd-mcp` downloads a prebuilt binary for Linux x86_64 and macOS. Rust is only needed to build from source (and for native Windows).

**Where does memory live?** In `<project>/.agent/`. The daemon state and index under `.agent/.dreamd/` are local and gitignored. You can read and edit the JSONL and Markdown by hand; durable appends should go through the daemon or MCP so there is one writer.

**What if I want a full wipe?** See [Full fresh store](./docs/troubleshooting.md#how-do-i-reset-or-clear-memory). There is no `dreamd reset --all`. `dreamd uninstall` stops processes and clears caches; `dreamd service uninstall --purge` deletes the daemon home `~/.agent/` and never a project `.agent/` store ([docs/install.md](./docs/install.md)).

**Windows?** `dreamd watch` serves the HTTP API on loopback TCP with a bearer token from `auth.json`. Learn, the dream cycle and the Tantivy index run there. The directory flush follows the Unix call order and is not a power-loss proof. There is no prebuilt npm binary for Windows. Details: [docs/windows.md](./docs/windows.md).

**Is everything free forever?** The Apache-2.0 core is open, and a single repo stays free. Premium, self-hosted features may come later. There is no hosted SaaS.

More: [docs/troubleshooting.md](./docs/troubleshooting.md).

---

## Status

**v1.2.0** is tagged and published on npm (`dreamd-mcp`, `latest`). Linux, macOS and Windows. Commands marked as 1.0.0 features above need 1.0.0 or later. If you upgrade a cargo-installed binary in place and run the daemon under the per-user service, restart it with `dreamd service restart` ([docs/install.md](./docs/install.md)).

| Layer | Status |
|---|---|
| `SPEC.md` v0.1 | Shipped |
| Reference implementation (daemon, HTTP API, dream cycle, Tantivy recall) | Shipped |
| MCP server (`dreamd mcp` + `npx dreamd-mcp` shim) | Shipped on npm |
| Provenance ledger, `forget`, `blame` | Shipped (unsigned) |
| Trust labels, signed proofs | Not shipped |
| CI / cross-platform matrix | Lint, test, build, binary-size gate, DCO (Windows jobs are informational) |

### Benchmarks

**WasTrue** tests whether a memory system updates a fact that later changes. The harness here is a scaffold and no results table is published yet ([scripts/benchmark/README.md](./scripts/benchmark/README.md)).

**Coding recall oracle.** `python3 scripts/benchmark/coding_oracle.py` printed `dreamd_only=12 both=0 stuffing_only=0 neither=0 n=12 budget_tokens=36`: after one `dreamd dream --no-llm`, k=32 recall packed to the same token budget found all 12 episodes that context stuffing missed. It is a small in-repo check, not a general benchmark.

---

## Platforms

Linux, macOS and Windows. Windows serves over loopback TCP. See [docs/windows.md](./docs/windows.md).

---

## Where dreamd fits

dreamd is one of three sibling repositories that make one product, **dreamOS**: *the first computer you can hand your keys to*. It acts on your behalf, it is structurally incapable of doing what you did not allow, and it can prove what it did. The kernel under it is **momo**; the sandbox beside it is `aegis`. momo's charter (RFC-009, amended by RFC-011, accepted 2026-09-26) records dreamd as the Linux-hosted proof of one of the product's pillars: memory as plain files a person owns, with provenance. The same amendment names the rule the provenance ledger grows toward: *every effect can be traced to the data that caused it, and no datum can cause an effect above its own trust label*. Nothing shipped implements a trust label yet. The momo repository is private until the kernel's first public release, and dreamOS is not announced or marketed before its v1; this paragraph is a pointer, not a launch.

---

## Documentation

| Doc | What |
|---|---|
| [GUIDE.md](./GUIDE.md) | 20-minute walkthrough, including inspect and undo |
| [docs/README.md](./docs/README.md) | Full documentation index |
| [docs/provenance.md](./docs/provenance.md) | Provenance ledger format and what `doctor --provenance` checks |
| [docs/observability.md](./docs/observability.md) | Recall citations, `blame`, counterfactual recall and salience drift |
| [SECURITY.md](./SECURITY.md) | Threat model, including the lesson-injection surface |
| [docs/http-api.md](./docs/http-api.md) | REST API (Unix socket / Windows loopback TCP) |
| [docs/configuration.md](./docs/configuration.md) | TOML config and env vars |
| [docs/troubleshooting.md](./docs/troubleshooting.md) | Common failures |
| [SPEC.md](./SPEC.md) | On-disk contract |
| [docs/salience.md](./docs/salience.md) | Salience formula deep-dive |
| [ARCHITECTURE.md](./ARCHITECTURE.md) | Engineering decisions |
| [CONTRIBUTING.md](./CONTRIBUTING.md) | Dev setup and RFC process |

---

## Contributing

See [CONTRIBUTING.md](./CONTRIBUTING.md). By participating you agree to the [Code of Conduct](./CODE_OF_CONDUCT.md). Security reports: [SECURITY.md](./SECURITY.md) (do not open a public issue for vulnerabilities).

## License

Apache-2.0. See [LICENSE](./LICENSE) and [NOTICE](./NOTICE).
