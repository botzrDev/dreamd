# `.agent/` spec digest

Two short pages for someone wiring a new harness, editor, or agent to a `.agent/`
store: **what is on disk, and what shape the bytes have.**

This is a *digest*, not the contract. The normative document — every MUST,
SHOULD, MAY and MUST NOT, plus the salience formula and the versioning promise —
is root [`SPEC.md`](../../SPEC.md). Where a sentence here and a sentence there
disagree, `SPEC.md` wins, and a change to the on-disk contract goes into
`SPEC.md` via the `[RFC]` process, never into this digest.

| Page | Read it for |
|---|---|
| [layout.md](./layout.md) | The four `.agent/` directories, the per-project store vs. the per-user daemon home, and which paths are derived state you should not read as memory. |
| [schemas.md](./schemas.md) | The episodic JSONL record, the `LESSONS.md` frontmatter and lesson blocks, when a cluster gets promoted, and how to write a record without corrupting the log. |

## Conformance in one paragraph

A conformant store is a directory named `.agent/` in a project root holding
UTF-8 text. A conformant **reader** walks `episodic/` and `semantic/`, skips
lines it cannot parse instead of halting, and ignores fields it does not know. A
conformant **writer** appends whole JSON lines atomically and redacts obvious
secrets. Nothing above requires dreamd: `.agent/` is a contract, not a product,
and a second implementation is the point.

## Related

- [`../../SPEC.md`](../../SPEC.md) — the contract itself (v0.1).
- [`../adapters.md`](../adapters.md) — how to wire a specific harness (MCP-first
  or documentation-first).
- [`../http-api.md`](../http-api.md) — dreamd's `/api/v1/*` surface.
- [`../glossary.md`](../glossary.md) — domain terms.
