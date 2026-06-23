# pinned-events

Shows which episodic events **survive dream-cycle decay** — and which do not — when `pinned` is mixed.

## The story

| Event | `pinned` | Age | Fate after `dreamd dream` |
|---|---|---|---|
| `evt_…01` | `true` | 120 days | **Kept** — pinned events are never pruned |
| `evt_…02` | `false` | 120 days | **Archived** to `.agent/.dreamd/snapshots/` (below salience threshold) |
| `evt_…03` | `false` | 2 days | **Kept** — too recent to decay |
| `evt_…04` | `true` | 95 days | **Kept** — pinned regardless of age |

After a dream cycle with `SOURCE_DATE_EPOCH=1780056000` (2026-05-29):

```bash
SOURCE_DATE_EPOCH=1780056000 dreamd dream --no-commit
```

Only unpinned, stale events decay. Pinned rows stay in `AGENT_LEARNINGS.jsonl` even when ancient.

## Inspect

```bash
grep pinned .agent/episodic/AGENT_LEARNINGS.jsonl
ls .agent/.dreamd/snapshots/    # after running dream against a copy with sentinel
```

## Lesson promotion interaction

When a cluster promotes to `LESSONS.md`, the exemplar event is set `pinned: true` automatically — so distilled lessons are never pruned on the next cycle. See [solo-rust-dev/](../solo-rust-dev/) for a full promotion example.
