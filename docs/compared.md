# dreamd compared

Honest v0.1 comparison. Concede competitor strengths; name our gaps.
For the product story, see [marketing.md](./marketing.md). For install and usage, see the [README](../README.md).

---

## TL;DR

| Dimension | dreamd v0.1 | Mem0 | Letta Code | Anthropic MCP memory (ref) | Cline Memory Bank |
|---|---|---|---|---|---|
| Cross-harness portability | ✓ Claude Code / Cursor / Cline share one `.agent/` | ✓ MCP + broad framework integrations; hosted path common | — harness-native (stateful Letta agent) | ✓ any MCP-capable client | — Cline-native markdown ritual |
| Salience-aware recall | ✓ BM25 × published formula ([SPEC.md](../SPEC.md)); `dreamd recall` / score explain | extraction + retrieval (managed / hosted options) | agent-managed memory / sleep-time reflection | string search over knowledge-graph nodes | LLM reads structured markdown; no ranking formula |
| Vector embeddings | — at v0.1 (lexical only; embeddings deferred) | ✓ typically vector / hybrid in product surface | not claimed here | — (graph JSON, not embeddings) | — |
| Schema versioning | ✓ `schema_version: "1.0.0"` on episodic records | product-specific (verify in their docs) | MemFS / context-repo layout (evolving) | tool/schema of the reference server | informal markdown file set |
| File-system source of truth | ✓ JSONL + Markdown in-repo ([SPEC.md](../SPEC.md)) | often DB / service-backed; local options vary | ✓ git-backed MemFS / context repositories | local JSON knowledge graph file | ✓ markdown files in the project |
| LLM consolidation | — deterministic dream cycle only at v0.1 | extraction / update pipelines (typically LLM-assisted) | ✓ sleep-time / dreaming subagents | — no dream-cycle contract | manual / prompt-driven “update memory bank” |
| Maturity / stars | small OSS — **4★** on `botzrDev/dreamd` (2026-07-14) | ~60.8k★ `mem0ai/mem0` (2026-07-14) | ~2.8k★ `letta-ai/letta-code` (2026-07-14) | ref lives in `modelcontextprotocol/servers` (~88.5k★ monorepo, 2026-07-14; not memory-only) | pattern inside Cline (~64.7k★ `cline/cline`, 2026-07-14); not a separate star counter |
| OS support | Linux + macOS only at v0.1 | cross-platform / cloud clients | verify current CLI/platform matrix | Node/`npx` (broad host support) | wherever Cline runs |

Cell legend: ✓ = strength for that dimension; — = not offered (or not at v0.1 for dreamd); short prose = partial / different shape. Competitor cells lean cautious when unverified from this repo.

---

## Weaknesses we own (v0.1)

Pulled from [AGENTS.md](../AGENTS.md) v0.1 scope — not a marketing softener:

- **No vector embeddings at v0.1.** Recall is Tantivy BM25 × salience. Semantic / embedding recall is out of scope until later.
- **Linux + macOS only at v0.1.** Windows is deferred.
- **Deterministic-only consolidation at v0.1.** The dream cycle does not call an LLM; LLM-assisted consolidation is later.
- **No auto dream cycle at v0.1.** `dream_cycle_mode = "auto"` is parsed but not acted on; cycles run when you invoke them (`dreamd dream`, HTTP, MCP). See [configuration.md](./configuration.md).

Maturity is also a gap: dreamd is a small open-source project (star count date-stamped in the table). On dense-vector recall benchmarks today, we lose — that is intentional substrate work, not a denied shortfall. See [partner-one-pager.md](../partner-one-pager.md) framing and [marketing.md](./marketing.md) on natural language vs embeddings.

---

## Per-competitor notes

### Mem0

**What they own:** A mature memory product with extraction-based pipelines, MCP and framework integrations, and a managed / hosted path when you want memory without operating a local daemon.

**What dreamd offers instead:** Local-first by default — no API key required for v0.1 — with memory as plain JSONL and Markdown under `.agent/` that you can `git diff` and hand-edit. If you want managed memory, use Mem0. If you want memory you own in the repo, use dreamd.

### Letta Code

**What they own:** A memory-first *coding agent harness*: long-lived agents, MemFS / context repositories, sleep-time dreaming, and self-editing memory. That is a full agent product, not a sidecar.

**What dreamd offers instead:** A thin memory layer beside Claude Code, Cursor, or Cline — not a replacement runtime. dreamd does not ask you to switch harnesses; it gives those harnesses a shared `.agent/` store via MCP.

### Anthropic MCP reference memory server

**What they own:** The canonical MCP memory *reference*: knowledge-graph tools (`search_nodes`, entity/relation CRUD), local JSON graph, drop-in `npx` packaging. Ideal when you want graph-shaped memory and the reference tool surface.

**What dreamd offers instead:** Episodic learnings with a published salience formula and a dream-cycle contract ([SPEC.md](../SPEC.md)). Tool names stay intentionally familiar (`search_nodes` / `append_node` — see [AGENTS.md](../AGENTS.md) / [ARCHITECTURE.md](../ARCHITECTURE.md)) so an MCP client can swap servers with a config edit, while the on-disk shape remains `.agent/` rather than a knowledge graph.

### Cline Memory Bank

**What they own:** A proven, harness-native documentation ritual — structured markdown files plus custom instructions so Cline reloads project context after every session reset. Zero daemon, human-editable, and deeply integrated with how Cline users already work.

**What dreamd offers instead:** The same “memory in the repo” instinct, but as a shared protocol across harnesses: scored recall, append-only episodic log, and adapters for Claude Code / Cursor / Cline ([adapters/](../adapters/)). Use Memory Bank when you stay inside Cline and want a docs workflow; use dreamd when the lesson must travel with you to the next IDE.

---

## When challenged (pointers)

For claims that draw HN heat, prefer live evidence over paste-walls (no separate rebuttal pack):

| Challenge | Evidence |
|---|---|
| “Salience is marketing fog” | Formula and contract in [SPEC.md](../SPEC.md); query-time computation in [ARCHITECTURE.md](../ARCHITECTURE.md); walkthrough in [GUIDE.md](../GUIDE.md) |
| “Just use embeddings” | Substrate bet in [marketing.md](./marketing.md) (“Why natural language, not embeddings”) and [ARCHITECTURE.md](../ARCHITECTURE.md) |
| “Unsafe local daemon” | Threat model and socket UID checks in [SECURITY.md](../SECURITY.md) |
| “How is this different from Mem0 / a graph memory?” | This page + [marketing.md](./marketing.md) positioning table |
| “Show me inspectability” | `dreamd recall` / explain path in [GUIDE.md](../GUIDE.md); plain JSONL under `.agent/` |
