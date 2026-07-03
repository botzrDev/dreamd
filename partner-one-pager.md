# dreamd

**Shared memory for AI coding agents, with receipts.**

A plain `.agent/` folder in your repo. Every MCP-aware coding agent (Claude Code, Cursor, Cline, OpenCode) reads from it and writes to it. What one agent learns, the next already knows.

The whole system fits in a single Rust binary that talks MCP. `npx dreamd-mcp` and it's running. No service to host, no database to migrate, no token quota to babysit. The folder is the source of truth: `cat` it, `grep` it, `git diff` it, hand-edit it. Your editor never has to know dreamd exists.

---

## The scenario

You're working in Claude Code. It hits an obscure build flag and figures out the workaround. The agent writes a node to `.agent/episodic/AGENT_LEARNINGS.jsonl`: what it tried, what failed, what worked. You close Claude Code.

Tomorrow you open Cursor. You ask it about that same flag. Cursor queries `search_nodes`, gets the node Claude Code wrote yesterday, and answers correctly on the first try, with the `source_harness` field showing exactly which agent learned it and when.

That's the whole pitch. Two different agents, one shared memory, full attribution.

---

## What's actually in v0.1

| In | Out (v0.2 or later) |
|---|---|
| Single binary, `npx dreamd-mcp` | Multi-agent dream-cycle running concurrently across harnesses |
| Two MCP tools (`search_nodes`, `append_node`) | Cross-repo memory consolidation (paid tier) |
| JSONL-backed `.agent/` folder, git-trackable | Trial-balance linter UI |
| BM25 lexical retrieval (deliberately not benchmarking recall in v0.1) | Salience scoring beyond write-time pain/importance |
| Salience signals on write (`pain` 0–10, `importance` 0–10) | A polished CLI; v0.1 looks like a daemon |
| `source_harness` attribution on every node | Telemetry. Nothing phones home. Ever. |

If you want a benchmark fight on recall today, we lose to anything dense-vector. That's a v0.2 problem and we know it. v0.1 is about proving the substrate.

---

## How dreamd differs from what's already out there

We're not first to "cross-harness AI memory." [agentmemory](https://github.com/rohitg00/agentmemory) shipped first, has ~23k★, runs locally, is Apache-2.0. The category is real and there are good options. Here is the actual difference:

**Most systems keep a database canonical and treat your files as a disposable export.** Open the database, the truth lives there; the files mirror it.

**dreamd inverts that.** The `.agent/` folder *is* the database. Open the files, the truth lives there; the daemon serves them. You can delete the daemon, keep the folder, and your memory survives: readable by `cat`, diffable in git, hand-editable in any text editor.

This is the bet: that as multi-agent workflows mature, sovereign file-native memory beats DB-canonical memory the same way git beat centralized version control.

*(For context: agentmemory is the strongest cross-harness competitor at ~23k★ as of June 2026, Apache-2.0, well-maintained. We mention them by name rather than hide from the comparison.)*

---

## Receipts: what a node looks like

A node is one plain JSONL line — this is the shape (we run dreamd on our own work; its `.agent/` store looks exactly like this):

```jsonl
{"schema_version":"1.0.0","id":"evt_01KSFKPXEDYR7NSJQNQGHFN60N","timestamp":"2026-05-25T13:03:12.845198614Z","source_harness":"claude-code","skill_action":"rust::error_type::non_exhaustive_enum","content":"Error enums in library crates should be #[non_exhaustive] so callers cannot pattern-match all variants exhaustively — adding new error cases later is a non-breaking change.","pain":3.0,"importance":8.0,"pinned":false}
```

That's one line in a project's `.agent/episodic/AGENT_LEARNINGS.jsonl` — plain text you can `git blame`, paste into a teammate's Slack, or `rg "non_exhaustive" .agent/` to find every related lesson across every agent that ever wrote there.

---

## The business model (disclosed upfront)

dreamd is **open-core on the GitLab model.**

- **Free, Apache-2.0:** the open `.agent/` format, the reference implementation, the daemon, every single-repo feature. If your firm only runs out of one repo, you never pay anything.
- **Paid (self-hosted, never multi-tenant SaaS):** cross-repo memory consolidation and governance/policy layers. Delivered as a self-hosted team daemon or locally-licensed modules. **No hosted service.** Sovereignty is the bet; we don't get to defect from it for revenue.

---

## What dreamd is NOT

- A model. We don't train anything, don't host inference, don't compete with the harness you use.
- A vector DB. The substrate is natural-language JSONL in your repo — portable by construction, because text is the one representation every model reads. No model-specific embedding index to manage or re-encode when you switch harnesses.
- A multi-tenant SaaS. Never.
- A replacement for the agent's working context. dreamd is long-term memory the harness consults; it doesn't try to be the harness's prompt-stuffer.
- A replacement for your memory system if you already have one. dreamd touches only `.agent/`, removes cleanly with `rm -rf .agent/`, and is silent if you don't query it.

---

## What we're asking design partners for

**A 45-minute screen-share** at a time of your choosing, in which you:

1. `npx dreamd-mcp` on a clean repo of your choice.
2. Run the cross-harness scenario above (we'll bring two harnesses if your setup only has one).
3. Tell us where the demo lied, where the install pissed you off, where `search_nodes` results were noticeably worse than naive `grep`, and where the JSONL schema doesn't fit something your existing system does cleanly.

No commitment to integrate. No quote in our launch post (unless you want one). No NDA. If you want to keep going after the screen-share, the v0.2 design discussion is where partners shape the multi-agent dream-cycle and the linter UX.

We're sending this to two builders we respect specifically. We are not running a beta program.

---

## Links + setup

- Repo: [github.com/botzrDev/dreamd](https://github.com/botzrDev/dreamd)
- Install: `npx dreamd-mcp` (Apache-2.0, ≤30s on a clean machine)
- Spec: [`SPEC.md`](https://github.com/botzrDev/dreamd/blob/main/SPEC.md): RFC-2119 conformance for folder layout, node schema, dream cycle
- Roadmap: [`ROADMAP.md`](https://github.com/botzrDev/dreamd/blob/main/ROADMAP.md): what shipped, what's next, and the v0.2 direction
- Contact: Austin at `uveddi@pm.me` for slow-thread, `@wgi_dev` on X for fast
