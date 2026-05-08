# `.agent/` — Portable Memory for AI Coding Agents

**Status:** v0.1-draft · 2026-05-08

Conformance keywords (MUST, SHOULD, MAY, MUST NOT) are used per [RFC 2119](https://www.rfc-editor.org/rfc/rfc2119).

## Thesis

`.agent/` is a directory convention any AI coding agent or harness can read from and write to in order to share runtime memory across tools. It sits beside [`AGENTS.md`](https://agents.md) and [`SKILL.md`](https://www.anthropic.com/news/skills) in a project root. This spec defines the folder layout, the JSON record schema, a salience formula for ranking recall, and a consolidation contract called the *dream cycle*. It does not mandate a storage engine, an LLM, a transport, or a daemon — those are implementation choices. **AGENTS.md is what you wrote down. `.agent/` is what your agent learned.**

## Relationship to `AGENTS.md` and `SKILL.md`

| File | Author | Role |
|---|---|---|
| [`AGENTS.md`](https://agents.md) | Human | Project rules, conventions, build/test commands. |
| [`SKILL.md`](https://www.anthropic.com/news/skills) | Human | Capability bundles invoked on demand. |
| `.agent/` | Machine | Evolving runtime memory: episodes, lessons, preferences. |

`.agent/` does not replace either. A project may use any combination of the three.

## Folder layout

A compliant `init` scaffolds:

```
.agent/
  working/      # Short-lived scratchpad for the active session. Format and lifecycle are implementation-defined.
  episodic/     # Append-only JSONL log of timestamped events. Canonical file: AGENT_LEARNINGS.jsonl.
  semantic/     # Lessons distilled from episodic by the dream cycle. Canonical file: LESSONS.md.
  personal/     # User preferences scoped to the human, not the project. Markdown. Implementations MUST NOT include `personal/` contents in any network call without explicit per-call user consent.
```

All text files MUST be UTF-8. `.agent/` is checked into the project's repo. Implementations MAY keep derived state (indexes, snapshots, write-ahead logs) under a hidden subfolder such as `.<impl>/`; such state MUST be `.gitignore`d.

## Node schema (episodic)

Each line in `episodic/AGENT_LEARNINGS.jsonl` MUST deserialize into the following JSON shape:

```json
{
  "schema_version": "1.0.0",
  "id": "01HZ8K2X9P3M7Q5R4S6T8V0W2Y",
  "timestamp": "2026-05-08T10:55:00Z",
  "source_harness": "claude-code",
  "skill_action": "rust::error_handling::axum_rejection",
  "content": "Axum requires custom Error types to implement IntoResponse. Do not use `unwrap()` in route handlers.",
  "pain": 7.5,
  "importance": 8.0,
  "pinned": false
}
```

**Required fields**

| Field | Type | Notes |
|---|---|---|
| `schema_version` | string | Exactly `"1.0.0"` for this revision. |
| `id` | string | Time-sortable identifier (ULID or UUIDv7). Assigned by the writer. |
| `timestamp` | string | ISO 8601, UTC. |
| `source_harness` | string | Lowercase ASCII identifier. Reference values: `claude-code`, `cursor`, `cline`, `opencode`, `aider`, `continue`. New values added by RFC. |
| `skill_action` | string | Hierarchical clustering key; segments separated by `::`. Implementations SHOULD lowercase. The dream cycle clusters on exact match. |
| `content` | string | Lesson body. Markdown allowed. |
| `pain` | number | 0.0–10.0. Severity of the moment that produced this entry. |
| `importance` | number | 0.0–10.0. Strategic weight. |
| `pinned` | boolean | Default `false`. The dream cycle sets it to `true` when the event is cited by a promoted lesson; pinned events MUST be skipped by pruning. |

**Optional fields**

| Field | Type | Notes |
|---|---|---|
| `client_dedup_key` | string | Idempotency key; implementations MAY use it to drop duplicate appends. |

`recurrence` is **not** an event field. It is a per-cluster count (events sharing a `skill_action`) computed by the dream cycle and stored separately by the implementation.

## Salience formula

Recall ranks results by:

```
salience    = exp(-age_days / 14) * (pain / 10) * (importance / 10) * (1 + ln(1 + recurrence))
final_score = bm25 * salience
```

`bm25` is the standard full-text relevance score for the query against `content`. `age_days` is derived from `timestamp` at query time; salience MUST NOT be pre-computed and stored, because the decay term changes continuously. `recurrence` is the count of events sharing the candidate's `skill_action` over a window the implementation defines.¹

## Dream cycle

The *dream cycle* is the consolidation pass that turns `episodic/` into `semantic/`. Inputs: the JSONL log plus per-cluster recurrence counts. A cluster is promoted when its recurrence exceeds an implementation-defined threshold over a recent window; the reference implementation uses ≥ 3 events in either a 7-day or 30-day window. Output: `semantic/LESSONS.md`, where every lesson MUST cite the source episodic `id`s it was distilled from. The cycle MUST be idempotent — running it twice on the same input produces the same output — and pinned events MUST NOT be pruned. An implementation MAY use an LLM for distillation but MUST also support a deterministic, network-free fallback.

## Scope

**In scope.** Folder layout, node schema, salience formula, dream-cycle contract.

**Out of scope.** Which LLM (or none) is used; how content is indexed; whether there is a long-running daemon; transport (stdio, HTTP, Unix socket, [MCP](https://modelcontextprotocol.io/)); service lifecycle; authentication. Implementations are free to differ on all of these.

## Versioning

This is **v0.1-draft**. Breaking changes are possible before v1.0. Proposals: open a GitHub issue with the prefix `[RFC]` on the reference implementation. After v1.0, `schema_version` follows semver, and any breaking change requires a migration path.

## Reference implementation

[`dreamd`](https://github.com/botzrDev/dreamd) — a local-first, single-binary tool that implements this spec via MCP, CLI, and an optional dream-cycle service. Other implementations are welcome and intended; `.agent/` is a contract, not a product.

---

¹ The shape — exponential decay, multiplicative pain × importance, log-scaled recurrence — follows the activation function in [ACT-R](http://act-r.psy.cmu.edu/about/) and the memory-stream retrieval used in [Park et al., *Generative Agents* (2023)](https://arxiv.org/abs/2304.03442).
