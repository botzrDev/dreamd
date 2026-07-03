# dreamd documentation index

Canonical map of every documentation artifact in this repository: what it is, who it is for, and where to find it.

## Start here

| Document | Audience | Purpose |
|---|---|---|
| [../GUIDE.md](../GUIDE.md) | New users | 20-minute linear tutorial (install → crash recovery) |
| [../STORY_IDS.md](../STORY_IDS.md) | Contributors | DR-/WEG- story ID legend |
| [../doc/dreamd.1](../doc/dreamd.1) | Power users | Man page |
| [../SPEC.md](../SPEC.md) | Implementers, contributors | On-disk layout, JSON schema, scoring formula, dream-cycle contract |
| [../ARCHITECTURE.md](../ARCHITECTURE.md) | Contributors | Load-bearing engineering decisions and crate boundaries |
| [../CONTRIBUTING.md](../CONTRIBUTING.md) | Contributors | Dev setup, commit conventions, DCO, RFC process |
| [../SECURITY.md](../SECURITY.md) | Operators, security reviewers | Threat model, socket auth, disclosure policy |

## User guides (Phase 0)

| Document | Audience | Purpose |
|---|---|---|
| [http-api.md](./http-api.md) | MCP shim authors, integrators | HTTP API over Unix domain socket — endpoints, headers, status codes |
| [configuration.md](./configuration.md) | Operators | TOML config keys, precedence, defaults, environment variables |
| [ci.md](./ci.md) | Contributors | CI pipeline jobs, local reproduction, merge gates |
| [troubleshooting.md](./troubleshooting.md) | Users | FAQ — symptom → cause → fix |
| [glossary.md](./glossary.md) | Everyone | Domain term definitions |

## Marketing & narrative

| Document | Audience | Purpose |
|---|---|---|
| [marketing.md](./marketing.md) | Evaluators, press | Product story, positioning, the "moment it earns its name" demo |

## Architecture deep-dives

| Document | Audience | Purpose |
|---|---|---|
| [architecture.md](./architecture.md) | Contributors | Extended architecture notes |
| [architecture/durability.md](./architecture/durability.md) | Contributors | Durability guarantees and WAL semantics |

## Agent harness adapters

| Document | Audience | Purpose |
|---|---|---|
| [../adapters/claude-code/README.md](../adapters/claude-code/README.md) | Claude Code users | MCP config and setup |
| [../adapters/cursor/README.md](../adapters/cursor/README.md) | Cursor users | MCP config, daemon recommendation, agent rule |
| [../adapters/cline/README.md](../adapters/cline/README.md) | Cline users | MCP config and verification steps |
| [../adapters/cursor/.cursor/rules/dreamd-recall.mdc](../adapters/cursor/.cursor/rules/dreamd-recall.mdc) | Cursor agent | When and how to recall and append learnings |

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
| [../packages/dreamd-mcp/README.md](../packages/dreamd-mcp/README.md) | npm users | `npx dreamd-mcp` shim, `DREAMD_BIN` override |
| [../SKILL.md](../SKILL.md) | AI agents | Skill file for dreamd recall behavior |
| [../AGENTS.md](../AGENTS.md) | AI coding agents | Repository conventions for agent harnesses |

## Reference & policy

| Document | Audience | Purpose |
|---|---|---|
| [../PERF.md](../PERF.md) | Contributors | Performance benchmark methodology (WIP) |
| [../CHANGELOG.md](../CHANGELOG.md) | Users, contributors | Release history |
| [../CODE_OF_CONDUCT.md](../CODE_OF_CONDUCT.md) | Contributors | Community standards |
| [security.md](./security.md) | Redirect | Points to [../SECURITY.md](../SECURITY.md) |
| [../tests/README.md](../tests/README.md) | Contributors | Workspace test fixtures |

## Where to put new docs

| If you are documenting… | Put it in… |
|---|---|
| User-facing how-to or reference | `docs/` and link from this index |
| Planning, spikes, video production, internal ADRs | `context/` (gitignored — see `context/README.md`) |
| On-disk contract or scoring formula | `SPEC.md` (via RFC) |
| Engineering invariant or crate boundary | `ARCHITECTURE.md` |
| Harness-specific setup | `adapters/<harness>/README.md` |
| Runnable walkthrough | `examples/<name>/` |
| Agent behavior guidance | `SKILL.md` or harness rule file |

When you add a new `docs/*.md` file, add a row to the appropriate table above.
