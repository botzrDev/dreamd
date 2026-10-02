# pinned-events

Shows which episodic events **survive dream-cycle decay** — and which do not — when `pinned` is mixed.

## The story

| Event | `pinned` | Age | Fate after `dreamd dream` |
|---|---|---|---|
| `evt_…01` | `true` | 117 days | **Kept** — pinned events are never pruned |
| `evt_…02` | `false` | 116 days | **Archived** to `.agent/.dreamd/snapshots/2026-05-29.jsonl` and removed from the live log (past the decay age, below the salience threshold) |
| `evt_…03` | `false` | 2 days | **Kept** — too recent to decay |
| `evt_…04` | `true` | 103 days | **Kept** — pinned regardless of age |

Ages are measured at the pinned cycle clock, `SOURCE_DATE_EPOCH=1780056000` (2026-05-29T12:00:00Z). The cycle rewrites the episodic log, so run it on a copy of this directory, not in your checkout:

```bash
cp -r . /tmp/pinned-events-demo && cd /tmp/pinned-events-demo
SOURCE_DATE_EPOCH=1780056000 dreamd dream --no-commit
```

It ends with `dream cycle complete (1 events decayed, 3 kept)`. Only unpinned, stale events decay. Pinned rows stay in `AGENT_LEARNINGS.jsonl` even when ancient. No cluster reaches the promotion threshold here, so no `LESSONS.md` is written.

## Inspect

```bash
grep pinned .agent/episodic/AGENT_LEARNINGS.jsonl
ls .agent/.dreamd/snapshots/    # in the copy, after the dream cycle: 2026-05-29.jsonl
```

## Lesson promotion interaction

When a cluster promotes to `LESSONS.md`, every event the lesson cites, including its exemplar, is set `pinned: true` automatically — so distilled lessons are never pruned on the next cycle. See [solo-rust-dev/](../solo-rust-dev/) for a full promotion example.
