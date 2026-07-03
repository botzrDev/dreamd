# dreamd roadmap

dreamd is portable, cross-harness memory for coding agents: a natural-language
`.agent/` folder your tools read from and write to in common. This page is
**direction, not dated commitments** — priorities move with what design partners
hit first. The day-to-day view lives in the issue tracker; this is the shape.

## Shipped — v0.1

- The open **`.agent/` standard** — folder layout, JSONL node schema, and
  dream-cycle semantics, specified to RFC-2119 in
  [`SPEC.md`](https://github.com/botzrDev/dreamd/blob/main/SPEC.md).
- **Cross-harness memory** — the same lessons in every coding agent you use,
  proven across **Claude Code and Cursor**, out of one repo-local store.
- **MCP server** (`search_nodes` / `append_node`) plus an **HTTP-over-UDS API**
  for direct integration.
- **Deterministic dream cycle** — clustering and consolidation of raw episodic
  events into promoted lessons, reproducible from the same inputs.
- **Crash-safe durable writes** — atomic append with torn-tail recovery, on
  Linux and macOS.
- Every single-repo feature is **free and Apache-2.0**. If you only ever run out
  of one repo, you never pay.

## Next — v0.1.1

- **Windows lifecycle** — service install and crash-safe atomic writes (see
  [`docs/windows.md`](https://github.com/botzrDev/dreamd/blob/main/docs/windows.md)).
- **More harness adapters**, including OpenCode.
- **Semantic indexing** alongside lexical recall.
- **LLM-assisted dream cycle** as an opt-in alternative to the deterministic default.
- Hardening and hot-fixes from launch feedback.

## On the roadmap — v0.2

- **Vector backend and hybrid retrieval** — lexical × vector scoring, built as
  *private derived state* over the canonical natural-language record (never a
  model-specific record on the wire; see the SPEC design principle).
- **Public benchmark harness** — reproducible recall-quality numbers you can run
  yourself.
- **Multi-agent dream cycle** and the **linter UX** — surfaces we want design
  partners to shape.
- **Cross-repo memory consolidation and governance** — delivered self-hosted
  (never multi-tenant SaaS); the first features in the paid, self-hosted tier.

## Principles that won't change

- The `.agent/` store is an **open standard**; the canonical record is
  **natural-language text**, portable to any model by construction.
- **No multi-tenant SaaS.** Paid features are self-hosted or locally-licensed —
  sovereignty is the bet.
- **Single-repo stays free**, Apache-2.0, forever.
