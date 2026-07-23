# Adapter authoring guide

How to wire a coding harness to dreamd so it shares a project's `.agent/` store.
This is the hub for harness maintainers and third parties. For copy-paste setup of
a *specific* harness, use its README under [`adapters/`](../adapters/); this guide
explains the two patterns those READMEs follow.

The on-disk contract every adapter targets is root [`SPEC.md`](../SPEC.md) — the
`.agent/` layout, the `AgentLearning` JSON shape, and the dream-cycle output.

> **Versions.** Use the floating form `npx -y dreamd-mcp` everywhere — npx
> re-resolves the `latest` dist-tag on each fresh spawn, so a config never goes
> stale. Do **not** hard-pin a version in copy-paste examples (a hard pin never
> tracks new releases); pin only to reproduce a specific build. Check the current
> published version with `npm view dreamd-mcp version`.

## Two patterns

An adapter is one of two things: an MCP server registration, or — for harnesses
without MCP — a documentation snippet that teaches the agent the same behavior.

### 1. MCP-first (recommended)

For any MCP-capable harness:

1. Initialize the store in the project: `npx -y dreamd-mcp init` (or `dreamd init`).
   > First run prompts once — press `y`, or use `npx -y dreamd-mcp`.
   This scaffolds `.agent/`. Harnesses that require an existing `.agent/` (e.g.
   Cline) fail with `no agent root found` until this runs.
2. Register an MCP server whose `command` is `npx` with `args` `["-y", "dreamd-mcp"]`.
   Leave it floating — npx re-resolves `latest` on each fresh spawn. Pin a version
   only to reproduce a specific build, never in a general copy-paste example.
3. Confirm the two tools appear: `search_nodes` and `append_node`. The names are
   fixed (see below) — do not rename or alias them.
4. Multi-writer setups: run `dreamd watch` so several agents share one daemon
   instead of each spawning an in-process server.

Point at an existing adapter rather than duplicating full JSON — each ships a
`.mcp.json.example` and a verification walkthrough:

- Claude Code → [`../adapters/claude-code/`](../adapters/claude-code/README.md)
- Cursor → [`../adapters/cursor/`](../adapters/cursor/README.md) (also ships a
  `.cursor/rules/` recall rule and a `--project-root` global example)
- Cline → [`../adapters/cline/`](../adapters/cline/README.md)

### 2. Documentation-first (no MCP)

For a harness that can't speak MCP, ship a documentation snippet — a `SKILL.md`,
a `CONVENTIONS.md`, or an agent-rule file — that tells the agent to:

- **Recall** by reading `.agent/semantic/LESSONS.md` (and, if needed, the episodic
  log `.agent/episodic/AGENT_LEARNINGS.jsonl`) before starting work.
- **Append** new learnings in the `AgentLearning` shape from [`SPEC.md`](../SPEC.md),
  including `source_harness` and a `skill_action` cluster key.

In-repo patterns to copy from:

- Root [`SKILL.md`](../SKILL.md) — the MCP-first agent skill file.
- [`../adapters/cursor/.cursor/rules/dreamd-recall.mdc`](../adapters/cursor/.cursor/rules/dreamd-recall.mdc)
  — a documentation-first rule file already in the tree.
- [`../adapters/claude-code/AGENTS.md.snippet`](../adapters/claude-code/AGENTS.md.snippet)
  — a drop-in Claude Code snippet.
- [`../adapters/aider/CONVENTIONS.md.template`](../adapters/aider/CONVENTIONS.md.template)
  — a documentation-first CONVENTIONS.md template for Aider (no MCP tools; append via UDS HTTP).

Describing how to add a harness that isn't shipped yet (Goose, Continue, …) is fine.
Do **not** claim an `adapters/<harness>/` tree exists before it does.

## Locked tool names

`search_nodes` (recall) and `append_node` (write) — an intentional match to
Anthropic's reference memory server so agents already trained on it need no
relearning. These names are locked; do not rename them or document aliases. See
[`AGENTS.md`](../AGENTS.md) and [`SKILL.md`](../SKILL.md).

## `skill_action` convention

`append_node` requires a `skill_action` cluster key of the form
`language::domain::specific` — `[a-z0-9_]` segments joined by `::`, lowercase,
≤ 256 bytes. Dots, hyphens, and slashes are rejected; the dream cycle clusters on
exact match.

- Good: `rust::error_handling::axum_rejection`
- Bad: `rust/error-handling` (slashes and hyphens are rejected)

This is a summary — the full rules live in [`SPEC.md`](../SPEC.md) (`AgentLearning`
table) and [`SKILL.md`](../SKILL.md) (`skill_action` naming rules). Don't fork a
second ruleset into your adapter; link those.

## When to reset the scratchpad

`dreamd reset workspace` clears `working/WORKSPACE.md` — the shared scratchpad —
back to its freshly-initialized state (DR-113). Use it between tasks or after a bad
session dump has polluted the scratchpad. It is **not** an episodic wipe: it leaves
`episodic/AGENT_LEARNINGS.jsonl` and `semantic/LESSONS.md` untouched. Pass `--yes`
to skip the confirmation prompt in non-interactive contexts.

`workspace` is the only `reset` subcommand today — don't document others.

## Related

- [`SPEC.md`](../SPEC.md) — on-disk contract (`.agent/` layout, JSON schema, scoring, dream cycle)
- [`SKILL.md`](../SKILL.md) — agent-facing tool and `skill_action` guide
- [`GUIDE.md`](../GUIDE.md) — end-user tutorial
- [`AGENTS.md`](../AGENTS.md) — repository conventions for agent harnesses
- Per-harness READMEs under [`adapters/`](../adapters/)
