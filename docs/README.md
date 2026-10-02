# dreamd documentation index

Canonical map of every documentation artifact in this repository: what it is, who it is for, and where to find it.

## Start here

| Document | Audience | Purpose |
|---|---|---|
| [../README.md](../README.md) | Everyone | What dreamd is, install, quickstart, status |
| [../GUIDE.md](../GUIDE.md) | New users | 20-minute linear tutorial (install → crash recovery) |
| [./dreamd.1](./dreamd.1) | Power users | Top-level man page, generated from the CLI definitions (`scripts/generate-man.sh`). The `dreamd-<cmd>(1)` labels are not separate pages |
| [cli.md](./cli.md) | Power users | Command index. `status`, `score`, `setup`, `reset workspace`, and `version` are written out here |
| [../SPEC.md](../SPEC.md) | Implementers, contributors | On-disk layout, JSON schema, scoring formula, dream-cycle contract — **canonical**; MUST/SHOULD language lives here |
| [spec/README.md](./spec/README.md) | Adapter authors, new implementers | Two-page digest of that contract — [layout](./spec/layout.md) and [schemas](./spec/schemas.md) |
| [../ARCHITECTURE.md](../ARCHITECTURE.md) | Contributors | Load-bearing engineering decisions and crate boundaries |
| [../CONTRIBUTING.md](../CONTRIBUTING.md) | Contributors | Dev setup, commit conventions, DCO, RFC process |
| [../SECURITY.md](../SECURITY.md) | Operators, security reviewers | Threat model, socket auth, disclosure policy |
| [glossary.md](./glossary.md) | Everyone | Domain term definitions |

## Guides and reference

| Document | Audience | Purpose |
|---|---|---|
| [http-api.md](./http-api.md) | MCP shim authors, integrators | HTTP API over a Unix domain socket (Windows: loopback TCP + bearer token) — endpoints, headers, status codes |
| [mcp-transports.md](./mcp-transports.md) | MCP client authors, operators | `dreamd mcp` transports — stdio (default) vs opt-in Streamable HTTP at `/mcp` via `--bind` / `--insecure` (source builds with the `mcp-http` feature); unauthenticated; not the `/api/v1` REST API |
| [configuration.md](./configuration.md) | Operators | TOML config keys, precedence, defaults, environment variables |
| [observability.md](./observability.md) | Integrators, operators | `explain=1` citations on `GET /api/v1/recall`, plus `dreamd blame`, counterfactual recall, and salience drift |
| [llm-cost-accuracy.md](./llm-cost-accuracy.md) | Operators | How `cost_cap_usd` is enforced for the opt-in LLM dream cycle and how approximate the estimate is |
| [troubleshooting.md](./troubleshooting.md) | Users | FAQ — symptom → cause → fix |
| [operator-handbook.md](./operator-handbook.md) | Operators | Manual store-maintenance runbook (e.g. `archive --force-unpin`) |
| [migrate.md](./migrate.md) | Operators | `dreamd migrate` — episodic schema migration (identity `1.0.0 → 1.0.0` no-op is the only registered path), `--from`/`--to` tokens, `.bak` behavior |
| [install.md](./install.md) | Operators | Optional per-user daemon service — `dreamd service install` / `start` / `restart` / `status` / `uninstall` on Linux (systemd `--user` unit), macOS (LaunchAgent) and Windows (Task Scheduler logon task) |
| [windows.md](./windows.md) | Operators | Native Windows status — what `service install` registers (logon task + `auth.json`), how `watch` serves the API over loopback TCP + bearer, and what is still blocked by `write_atomic` |

## Memory branches, provenance, and forgetting

Shipped commands. Both format pages describe trees under the gitignored `.agent/.dreamd/`, outside the v0.1 `SPEC.md` contract.

| Document | Audience | Purpose |
|---|---|---|
| [branching-guide.md](./branching-guide.md) | Users | How to use `dreamd memory branch` / `checkout` / `branches` / `delete` / `diff` / `bisect` |
| [branching.md](./branching.md) | Contributors, implementers | Branch storage format (`branches/1.0`) under `.agent/.dreamd/branches/` — objects, refs, `HEAD`, the per-dream-cycle `snap-*` refs, `memory diff` and `memory bisect` |
| [provenance.md](./provenance.md) | Contributors, implementers | Provenance ledger format (`provenance/1.0`) under `.agent/.dreamd/provenance/`. Edge recording, the read-only `dreamd doctor --provenance` check, and `dreamd forget` (with its `forget-receipt/1.0` file) are implemented; the signed proof and a standalone verifier are **not** |

## Optional and library-only features

| Document | Audience | Purpose |
|---|---|---|
| [vectors.md](./vectors.md) | Contributors | Optional `vectors` Cargo feature (off by default) — `dreamd vectors enable` downloads the model on a `--features vectors` build and exits 2 on the default binary; **still no vector recall**. Also covers the unused `rrf::fuse` helper |
| [context-grant.md](./context-grant.md) | Contributors | `GrantedStore` / `ContextGrant` — a prototype recall/append filter in `dreamd-core`; **not wired to MCP** or the CLI |
| [../adapters/letta/README.md](../adapters/letta/README.md) | Contributors | `LettaStore` — a code seam only; nothing to install |

## Marketing & narrative

| Document | Audience | Purpose |
|---|---|---|
| [marketing.md](./marketing.md) | Evaluators, press | Product story, positioning, the "moment it earns its name" demo |
| [compared.md](./compared.md) | Evaluators, HN | Comparison vs Mem0 / Letta Code / MCP-ref / Cline Memory Bank (dreamd column re-checked for 1.0.0; competitor cells dated in the table) |
| [../ROADMAP.md](../ROADMAP.md) | Evaluators, contributors | Direction: what shipped and what is next, without dates |

## Architecture deep-dives

| Document | Audience | Purpose |
|---|---|---|
| [architecture.md](./architecture.md) | Contributors | Extended architecture notes — crate split, IPC/coordinator, Tantivy indexing + salience, dream cycle, WAL/crash recovery, API stability |
| [salience.md](./salience.md) | Contributors | Salience ranking formula deep-dive — factor-by-factor derivation, lineage, and calibration |
| [architecture/durability.md](./architecture/durability.md) | Contributors | JSONL append durability protocol |
| [architecture/tantivy-migration.md](./architecture/tantivy-migration.md) | Contributors | What a future Tantivy major-version bump has to re-verify; no migration is planned or performed |

## Agent harness adapters

| Document | Audience | Purpose |
|---|---|---|
| [adapters.md](./adapters.md) | Harness maintainers / third parties | How to author an adapter (MCP-first + doc-first) |
| [../adapters/claude-code/README.md](../adapters/claude-code/README.md) | Claude Code users | MCP config and setup (with `.mcp.json.example` and an optional `AGENTS.md.snippet`) |
| [../adapters/cursor/README.md](../adapters/cursor/README.md) | Cursor users | MCP config, daemon recommendation, agent rule |
| [../adapters/cursor/.cursor/rules/dreamd-recall.mdc](../adapters/cursor/.cursor/rules/dreamd-recall.mdc) | Cursor agent | When and how to recall and append learnings |
| [../adapters/cline/README.md](../adapters/cline/README.md) | Cline users | MCP config and verification steps |
| [../adapters/aider/README.md](../adapters/aider/README.md) | Aider users | Doc-first `CONVENTIONS.md.template`; append via UDS HTTP |

## Examples

| Document | Audience | Purpose |
|---|---|---|
| [../examples/README.md](../examples/README.md) | New users | Overview of bundled example projects |
| [../examples/solo-rust-dev/README.md](../examples/solo-rust-dev/README.md) | New users | Single-harness happy path |
| [../examples/multi-harness/README.md](../examples/multi-harness/README.md) | New users | Claude Code + Cursor sharing one `.agent/` |
| [../examples/crash-recovery/README.md](../examples/crash-recovery/README.md) | Contributors | WAL crash-recovery fixture |
| [../examples/pinned-events/README.md](../examples/pinned-events/README.md) | Contributors | Pinned vs unpinned decay behavior |
| [../examples/cross-project/README.md](../examples/cross-project/README.md) | New users | Per-project memory isolation |

## Package & distribution

| Document | Audience | Purpose |
|---|---|---|
| [../packages/dreamd-mcp/README.md](../packages/dreamd-mcp/README.md) | npm users | `npx dreamd-mcp` shim, `DREAMD_BIN` override, uninstall / update, MCP Registry metadata |
| [../SKILL.md](../SKILL.md) | AI agents | Skill file for dreamd recall behavior |
| [../AGENTS.md](../AGENTS.md) | AI coding agents | Repository conventions for agent harnesses |

## Project, CI, and release

| Document | Audience | Purpose |
|---|---|---|
| [../CHANGELOG.md](../CHANGELOG.md) | Users, contributors | Release history |
| [ci.md](./ci.md) | Contributors | CI pipeline jobs, local reproduction, merge gates; release, nightly and manifest workflows |
| [../RELEASING.md](../RELEASING.md) | Maintainers | Release procedure — version surfaces, tag, draft release, manifest PR, human-only npm and MCP Registry publish |
| [../PERF.md](../PERF.md) | Contributors | Last recorded binary size, idle RSS and recall latency against the CI limits |
| [benchmarks.md](./benchmarks.md) | Contributors | How to re-run the in-RAM recall latency bench |
| [../STORY_IDS.md](../STORY_IDS.md) | Contributors | `DR-` / `WEG-` / `AILAB-` / `BZR-` story ID legend |
| [../CODE_OF_CONDUCT.md](../CODE_OF_CONDUCT.md) | Contributors | Community standards |

## Tests and tooling

| Document | Audience | Purpose |
|---|---|---|
| [../tests/README.md](../tests/README.md) | Contributors | Workspace test fixtures |
| [../crates/dreamd-cli/tests/README.md](../crates/dreamd-cli/tests/README.md) | Contributors | CLI integration tests and the `cli_help` snapshot workflow |
| [../packages/dreamd-mcp/test/README.md](../packages/dreamd-mcp/test/README.md) | Contributors | `node --test` suites for the npx shim |
| [../scripts/alpha/README.md](../scripts/alpha/README.md) | Contributors | Alpha suites — cross-harness append → recall smoke test, quality and install-funnel suites |
| [../scripts/benchmark/README.md](../scripts/benchmark/README.md) | Contributors | WasTrue benchmark scaffold (a separate eval, not the recall latency bench) |

## Historical records

Point-in-time documents. They are kept as written and are not updated when the code moves.

| Document | Audience | Purpose |
|---|---|---|
| [audits/2026-10-01-pre-1.0.0/README.md](./audits/2026-10-01-pre-1.0.0/README.md) | Maintainers | Pre-1.0.0 test audits of commit `bb84721` — workspace suite, CI and release gates, memory invariants, end-to-end |
| [v0.1.1-scope.md](./v0.1.1-scope.md) | Maintainers | Frozen v0.1.1 contents, review window, reserve, and cut order (shipped) |
| [spikes/dr-014-cline-mcp.md](./spikes/dr-014-cline-mcp.md) | Maintainers | 2026-07 spike: does Cline surface the two MCP tools |
| [superpowers/specs/2026-07-31-install-upgrade-funnel-design.md](./superpowers/specs/2026-07-31-install-upgrade-funnel-design.md) | Maintainers | 2026-07 design note for the install / upgrade funnel (`dreamd setup`, `update`) |

## Where to put new docs

| If you are documenting… | Put it in… |
|---|---|
| User-facing how-to or reference | `docs/` and link from this index |
| Planning, spikes, video production, internal ADRs | `context/` (gitignored and local-only; nothing under it is in a clone) |
| On-disk contract or scoring formula | `SPEC.md` (via RFC) |
| Engineering invariant or crate boundary | `ARCHITECTURE.md` |
| Harness-specific setup | `adapters/<harness>/README.md` |
| Runnable walkthrough | `examples/<name>/` |
| Agent behavior guidance | `SKILL.md` or harness rule file |

When you add a new `docs/*.md` file, add a row to the appropriate table above. When a feature ships, move its row out of any "not implemented" wording in the same change.
