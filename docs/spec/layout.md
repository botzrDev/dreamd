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

What dreamd actually puts there: `dreamd init` creates the four directories,
an empty `episodic/AGENT_LEARNINGS.jsonl`, and `working/WORKSPACE.md` (a
placeholder the dream cycle neither reads nor writes). A dream cycle that
promotes a cluster writes `semantic/LESSONS.md` and, beside it,
`semantic/recurrence_counts.json` — dreamd's per-cluster counts, which the spec
leaves to the implementation. `personal/` starts empty; dreamd reads
`personal/PREFERENCES.md` for `GET /api/v1/preferences` if you create it. If
startup recovery ever finds a malformed line in the log, it moves that line to
`episodic/.corrupt-<YYYY-MM-DD>.jsonl` beside it.

All four are plain UTF-8 text. `personal/` is the one directory with a hard
privacy rule attached: its contents MUST NOT reach any LLM, local or remote,
without explicit per-call user consent, and the dream cycle MUST NOT distill it
into `semantic/`. In dreamd that consent is `dreamd dream --share-personal`, or
the `x-dreamd-share-personal: 1` header on `POST /api/v1/dream`; it authorizes
exactly the one cycle it is passed to and is never persisted.

## Derived state is hidden and gitignored

An implementation may keep indexes, snapshots, and write-ahead logs under a
hidden subfolder named for itself — `.<impl>/` — and that state MUST be
gitignored. dreamd uses `<project>/.agent/.dreamd/`, and `dreamd init` writes
the ignore rule (`/.agent/.dreamd/`) for you:

```
<project>/.agent/.dreamd/
  config.toml              # Per-project settings (all keys optional). Written by init.
  state.json               # Daemon state: version, last dream-cycle status and time. Written by init.
  dream_in_progress.wal    # Present only while a dream cycle (or a forget) is mid-flight.
  index/                   # Tantivy index — a rebuildable cache of the log and LESSONS.md.
  index_manifest.json      # Index schema version.
  index_progress.json      # Newest indexed event id.
  semantic_pass.json       # Which lessons the last index pass indexed or skipped.
  snapshots/<YYYY-MM-DD>.jsonl   # Decay archive: events pruned from the log, never deleted.
  branches/
    HEAD                     # Current ref name. A dream cycle does not write this.
    refs/<name>              # Two lines: 64-hex object id, then a UTC timestamp.
    objects/<id>/            # <id> is the SHA-256 of manifest.json's canonical bytes.
      manifest.json
      episodic/AGENT_LEARNINGS.jsonl
      semantic/LESSONS.md
      semantic/recurrence_counts.json
      personal/PREFERENCES.md
  provenance/
    ledger.jsonl             # Append-only. One compact JSON edge per line.
```

Only `config.toml` and `state.json` exist after `init`; the rest appear the
first time the daemon, a dream cycle, or `dreamd memory branch` needs them.
Three of these are easy to confuse. `snapshots/` holds only decay archives.
`branches/` ([`docs/branching.md`](../branching.md)) holds whole-store copies:
every dream cycle saves one as a `snap-*` ref before it changes anything, and
`dreamd memory branch` names one. `provenance/`
([`docs/provenance.md`](../provenance.md)) holds the ledger that
`dreamd doctor --provenance` checks.

Treat `.dreamd/` as a cache belonging to another process. It is rebuildable, it
is not the memory, and an adapter should never read it to answer a recall.

## Two roots: project store vs. daemon home

A common wiring bug is collapsing these into one directory. They are separate,
and in the reference implementation they are separate *types*
([`crates/dreamd-core/src/layout.rs`](../../crates/dreamd-core/src/layout.rs)):

| | `AgentRoot` — per **project** | `DaemonHome` — per **user** |
|---|---|---|
| Path | `<project>/.agent/` | `~/.agent/` |
| Holds | `working/`, `episodic/`, `semantic/`, `personal/` | `registry.toml` (+ `registry.toml.lock`), `dreamd.log`, and the daemon's endpoint: `dreamd.sock` on Linux/macOS, or `auth.json` + `server.json` on Windows |
| In git | Yes (except `.dreamd/`) | No — it is outside every repo |
| Count | One per project, many coexist | Exactly one per user |
| Defined by | `SPEC.md` | dreamd only; the portable spec says nothing about it |

The daemon home is deliberately never inside a project store: co-locating them
would let a repo's git history leak the auth token. `dreamd service uninstall
--purge` removes the daemon home and, by design, no project's memory.

## dreamd's extras

Everything dreamd adds beyond the four directories is either under the hidden
`.dreamd/` folder above or is the one checked-in sidecar,
`semantic/recurrence_counts.json`. None of it is part of the spec's required
layout, a second implementation is free to ignore all of it, and a conformance
claim must not depend on it.

The reference implementation's path helpers also name `skills/` and
`protocols/` under `.agent/`, but `dreamd init` does not create them, nothing
reads them, and `dreamd doctor` does not check for them. `SPEC.md` names four
directories; treat those two as unused.

---

Next: [Page 2 — Schemas](./schemas.md).
