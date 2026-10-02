# Page 2 — Schemas

The two file formats a `.agent/` store contains. Normative text:
[`SPEC.md` §Node schema](../../SPEC.md#node-schema-episodic) and
[`SPEC.md` §Dream cycle](../../SPEC.md#dream-cycle).

## Episodic: one JSON object per line

`episodic/AGENT_LEARNINGS.jsonl` is newline-delimited JSON — never a JSON array,
never pretty-printed, one whole record per `\n`-terminated line:

```json
{"schema_version":"1.0.0","id":"evt_01ARZ3NDEKTSV4RRFFQ69G5FAV","timestamp":"2026-05-08T10:55:00Z","source_harness":"claude-code","skill_action":"rust::error_handling::axum_rejection","content":"Axum requires custom Error types to implement IntoResponse.","pain":7.5,"importance":8.0,"pinned":false}
```

| Field | Type | What a writer must get right |
|---|---|---|
| `schema_version` | string | Exactly `"1.0.0"`. This is the record schema, versioned independently of the SPEC (currently v0.1). |
| `id` | string | Lexically sortable by creation time — ULID or UUIDv7. Assigned by the writer. |
| `timestamp` | string | ISO 8601 with an explicit UTC offset. |
| `source_harness` | string | Lowercase `[a-z0-9_-]+` naming your harness. `claude-code`, `cursor`, `cline`, `opencode`, `aider`, `continue` are reserved for their owners. dreamd stores the value you send as-is; it does not check the pattern or the reserved names. |
| `skill_action` | string | Cluster key: `[a-z0-9_]` segments joined by `::`, ≤ 256 bytes. dreamd lowercases it and turns spaces into `_`, then rejects dots, hyphens and slashes. SPEC says the dream cycle clusters on exact match; dreamd also counts every `::` prefix, so `rust::errors::a` and `rust::errors::b` can promote together as `rust::errors`. |
| `content` | string | The lesson, in natural language. Keep it under 4 KiB; a reader accepts up to 64 KiB. dreamd rejects an append whose whole serialized line exceeds 4096 bytes (HTTP 413). |
| `pain` / `importance` | number | 0.0–10.0 each. Roughly: routine success ≈ 2, recoverable failure ≈ 5, hard failure ≈ 7, security-relevant ≈ 8–10. |
| `pinned` | boolean | `false` unless the event must survive pruning. Do not unset a `true` another writer set. |

`client_dedup_key` is the one optional field. dreamd does not write it into
the record: it takes the key per request (the `X-Client-Dedup-Key` header, or
the `client_dedup_key` argument of `append_node`) and uses it only to drop a
duplicate append for as long as that writer process keeps running. `recurrence` is **not** a field on
the record — it is a per-cluster count the dream cycle computes. dreamd keeps
those counts in `semantic/recurrence_counts.json`.

dreamd writes the fields in the order `schema_version`, `id`, `timestamp`,
`pain`, `importance`, `pinned`, `skill_action`, `source_harness`, `content`, and
writes `timestamp` with nanoseconds (`2026-05-08T10:55:00.123456789Z`). Field
order carries no meaning; parse the line as JSON.

Three rules that decide whether your reader is conformant:

- **Forward compatibility.** Ignore unknown fields; do not halt on them. Prefix
  your own experimental fields with `x_`.
- **Corrupt input.** Skip a line that fails validation, log the line number and
  reason, and keep going — never stop at the first invalid line.
- **Secret redaction.** Strip obvious high-entropy tokens from `content` before
  you append. The spec mandates the duty, not the algorithm.

## Semantic: `LESSONS.md`

The dream cycle promotes a cluster once its recurrence passes an
implementation-defined threshold; dreamd uses **≥ 3 events in either a 7-day or
a 30-day window**. The output is YAML frontmatter followed by one
HTML-comment-delimited block per promoted lesson. dreamd writes exactly one
lesson per cycle, for the highest-scoring promoted cluster:

```
---
last_updated: "2026-09-17T18:04:11Z"
prompt_version: "deterministic-only"
cluster_key: "rust::error_handling::axum_rejection"
citations: "evt_01ARZ3NDEKTSV4RRFFQ69G5FAV"
---
<!-- dreamd:lesson id="evt_01ARZ3NDEKTSV4RRFFQ69G5FAV" cluster="rust::error_handling::axum_rejection" -->
Implement IntoResponse on your error type; a handler that unwraps panics the worker.
<!-- /dreamd:lesson -->
```

`last_updated`, `prompt_version` and `cluster_key` are required. `citations` is
**optional** — a space-separated list of episodic `evt_` ids inside one quoted
scalar, which is how dreamd records the events a lesson was composed from. A
reader must tolerate its absence; a writer need not emit it. dreamd always
emits it.

`prompt_version` says who wrote the body. `"deterministic-only"` means the body
is the exemplar event's own `content`, copied without a model, and `citations`
is that one event. When an API key is configured and `--no-llm` is not passed,
dreamd asks a model to compose the body instead; `prompt_version` is then a
versioned prompt id of the form `dream-cycle/v<N>@<date>`, and `citations` lists
every cluster event the model drew on. dreamd pins every cited event.

The opening tag carries `id` (the exemplar event) and `cluster` (the full
`skill_action`); the closing tag sits on its own line right after the body. If a
cycle promotes nothing, `LESSONS.md` is absent — an empty-frontmatter
placeholder is invalid, and a file left by an earlier cycle is removed. Running
the cycle twice on identical input produces byte-identical output. In dreamd
that holds for the deterministic body at a fixed clock: `last_updated` is the
time the cycle ran, so two runs a second apart differ on that one line.

## `personal/` and consent

`personal/` is Markdown the human owns. It MUST NOT be included in any LLM call,
local or remote, without explicit per-call consent, and the dream cycle MUST NOT
distill it into `semantic/`. dreamd's surface for that consent is
`dreamd dream --share-personal`, or `x-dreamd-share-personal: 1` on
`POST /api/v1/dream`; it covers exactly one cycle and is never persisted.

## Writing through dreamd

> A record reaches the log through the coordinator, which is the single writer.
> Two supported doors:
>
> - **MCP** — call the `append_node` tool (recall is `search_nodes`). Register
>   the server as the floating `npx -y dreamd-mcp` (`command: npx`,
>   `args: ["-y", "dreamd-mcp"]`) so a fresh spawn re-resolves `latest`; do not
>   hard-pin a version in a copy-paste example. Send `source_harness`,
>   `skill_action`, `content`, `pain` and `importance` (the two scores are
>   optional over MCP and default to 5.0); dreamd assigns `id`, `timestamp` and
>   `schema_version` itself.
> - **HTTP** — `POST /api/v1/learn` over the Unix socket (loopback TCP with a
>   bearer token on Windows), for a harness with no MCP support. Same fields,
>   with both scores required; needs a running `dreamd watch`. See
>   [`../http-api.md`](../http-api.md).
>
> Never `echo >>` the JSONL log or hand-edit it in an editor. A direct append
> races the coordinator, skips redaction and dedup, and leaves the derived index
> disagreeing with the file.

---

Back to [Page 1 — Layout](./layout.md) · hub: [`docs/spec/`](./README.md).
