# dreamd — product story

Marketing and positioning content. For install and usage, see the [README](../README.md).

---

## Same memory in every IDE

dreamd makes Claude Code, Cursor, and Cline remember the same things. Drop a `.agent/` folder in your repo. Every coding agent you use reads and writes to it.

Every coding agent ships its own memory format. dreamd is what they could share.

AGENTS.md is what you wrote down. dreamd is what your agent learned, across every tool.

---

## The moment it earns its name

```text
~/project $ npx dreamd-mcp init

# In Claude Code, Tuesday afternoon:
you   ▸ axum keeps blowing up when I unwrap in route handlers
claude▸ filed under rust::error_handling::axum_rejection

# In Cursor, Friday morning, fresh session:
you   ▸ why is this build failing?
cursor▸ You're unwrapping in a route handler. dreamd has a
        lesson from Tuesday -- axum needs IntoResponse on
        custom Error types. Try `?` and a typed error.
```

No re-explaining. No re-pasting. No "as I mentioned before."

---

## What dreamd is — and isn't

| dreamd is | dreamd isn't |
|---|---|
| A portable memory format (`.agent/`) checked into your repo | A vector database |
| A reference MCP server for reading and writing it | A knowledge graph engine |
| Local-first by default — zero network calls in v0.1 | A hosted SaaS |
| One source of truth across every coding agent you use | A replacement for `AGENTS.md` or `SKILL.md` |

If you need graph multi-hop reasoning, use [Cognee](https://github.com/topoteretes/cognee). If you need a single-file portable memory capsule, use [Memvid](https://github.com/Olow304/memvid). dreamd does the one thing they don't: makes your memory follow you between coding agents.

---

## Why natural language, not embeddings

Most "AI memory" products store your memory as embeddings — vectors computed by one model's encoder. That works right up until a *different* model reads it back. An embedding lives in the geometry of the model that produced it; hand it to another model and it's off-manifold — the information is technically retrievable but not in a shape the new model can use. (Recent mechanistic work on cross-boundary reasoning shows exactly this: a representation can decode to the right token at ~100% confidence yet sit far from the clean embedding the next consumer expects. **Decodable is not portable.**)

dreamd stores the one representation every model was trained to read: **natural language.** A lesson written by Claude Code is plain text in `.agent/episodic/AGENT_LEARNINGS.jsonl` — Cursor, Cline, or a model that doesn't exist yet reads it with zero translation. That's not a limitation of v0.1; it's the substrate bet. Portable memory can't be model-specific vectors by construction.

And because the substrate is text *with provenance*, recall is **attributable**: every `search_nodes` hit comes back with its `skill_action` cluster and the `source_harness` that authored it — you see which agent taught each lesson and when. Cross-harness memory with receipts, not an opaque nearest-neighbor lookup.
