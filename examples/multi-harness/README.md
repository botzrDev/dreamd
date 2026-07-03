# multi-harness

The **same Rust project** as [`solo-rust-dev`](../solo-rust-dev/), but worked
across two AI harnesses — Claude Code and Cursor. The learnings in
`episodic/AGENT_LEARNINGS.jsonl` are split between them:

| `skill_action` | `source_harness` |
|----------------|------------------|
| `rust::clippy::needless_borrow` | **cursor** |
| `rust::clippy::derive_partial_eq_without_eq` | claude-code |
| `rust::clippy::redundant_clone` | **cursor** |
| `rust::clippy::manual_let_else` | claude-code |

## The story — "across every tool"

The `rust::clippy` cluster recurs four times and gets promoted exactly as in the
solo scenario (`semantic/recurrence_counts.json`: `rust::clippy`, count `4`). The
difference is *where the lesson came from*.

The highest-salience learning — `needless_borrow` — was first captured **in
Cursor**. The dream cycle distilled it into `semantic/LESSONS.md` and pinned its
source event. Because every harness reads and writes the one shared `.agent/`
store, that lesson is now available to **Claude Code too**, even though Claude
Code never logged it. The promoted cluster draws from both tools (two
`cursor` events, two `claude-code` events) — memory that follows the work, not
the editor.

Recall now surfaces `source_harness` on every hit, so a learning written under
one harness is recalled with its origin attached — Claude Code can see that
`needless_borrow` was first taught by Cursor.

That is what "across every tool" buys you: a lesson learned once, anywhere,
surfaces everywhere.

## Regenerating this fixture

Identical recipe to the solo scenario — deterministic, offline, no API key. From
a copy of this `.agent/` tree:

```sh
SOURCE_DATE_EPOCH=1780056000 dreamd dream --no-commit
```

`SOURCE_DATE_EPOCH` pins the clock to `2026-05-29T12:00:00Z`; `--no-commit` skips
the git autobiography commit. Re-running over the committed tree is a no-op.
