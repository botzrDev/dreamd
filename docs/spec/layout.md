# Page 1 — Layout

What a `.agent/` store looks like on disk, and what is *not* part of it.
Normative text: [`SPEC.md` §Folder layout](../../SPEC.md#folder-layout).

## The project store

One `.agent/` directory per project, in the project root, beside `AGENTS.md`.
It is checked into the project's repo — that is the whole point, memory travels
with the code.

```
<project>/.agent/
  working/      # Session scratchpad. Format and lifecycle are implementation-defined;
                # an implementation MAY omit this directory entirely.
  episodic/     # Append-only event log, one JSON object per line. Canonical file:
                # AGENT_LEARNINGS.jsonl — never hand-edited; appends go through a writer.
  semantic/     # Lessons distilled from episodic by the dream cycle. Canonical file: LESSONS.md.
  personal/     # Preferences scoped to the human, not the project. Markdown.
```

All four are plain UTF-8 text. `personal/` is the one directory with a hard
privacy rule attached: its contents MUST NOT reach any LLM, local or remote,
without explicit per-call user consent, and the dream cycle MUST NOT distill it
into `semantic/`. In dreamd that consent is `dreamd dream --share-personal`, or
the `x-dreamd-share-personal: 1` header on `POST /api/v1/dream`; it authorizes
exactly the one cycle it is passed to and is never persisted.

## Derived state is hidden and gitignored

An implementation may keep indexes, snapshots, and write-ahead logs under a
hidden subfolder named for itself — `.<impl>/` — and that state MUST be
gitignored. dreamd uses `<project>/.agent/.dreamd/`: `state.json`, the dream-cycle
WAL, the Tantivy index, `snapshots/`, and a per-project log. `dreamd init` writes
the ignore rule for you.
Decay archives are `<YYYY-MM-DD>.jsonl` files in `snapshots/`. The unimplemented
branch format in [`docs/branching.md`](../branching.md) uses `branches/`, not
`snapshots/`.

Treat `.dreamd/` as a cache belonging to another process. It is rebuildable, it
is not the memory, and an adapter should never read it to answer a recall.

## Two roots: project store vs. daemon home

A common wiring bug is collapsing these into one directory. They are separate,
and in the reference implementation they are separate *types*
([`crates/dreamd-core/src/layout.rs`](../../crates/dreamd-core/src/layout.rs)):

| | `AgentRoot` — per **project** | `DaemonHome` — per **user** |
|---|---|---|
| Path | `<project>/.agent/` | `~/.agent/` |
| Holds | `working/`, `episodic/`, `semantic/`, `personal/` | `dreamd.sock`, `registry.toml`, `auth.json`, `server.json`, `dreamd.log` |
| In git | Yes (except `.dreamd/`) | No — it is outside every repo |
| Count | One per project, many coexist | Exactly one per user |
| Defined by | `SPEC.md` | dreamd only; the portable spec says nothing about it |

The daemon home is deliberately never inside a project store: co-locating them
would let a repo's git history leak the auth token. `dreamd service uninstall
--purge` removes the daemon home and, by design, no project's memory.

## dreamd's extra directories

`dreamd init` also scaffolds `skills/` and `protocols/` under `.agent/`. These
are reference-implementation conveniences, **not** part of the spec's required
layout — `SPEC.md` names four directories, and `dreamd doctor` passes on a store
that has neither. A second implementation is free to ignore them, and a
conformance claim must not depend on them.

---

Next: [Page 2 — Schemas](./schemas.md).
