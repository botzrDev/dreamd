# solo-rust-dev

One Rust developer, working only in Claude Code. Every learning here carries
`"source_harness": "claude-code"`.

## The story

Over a few days the agent kept running into the same family of clippy lints and
logged each one to `episodic/AGENT_LEARNINGS.jsonl`:

| `skill_action` | what it learned |
|----------------|-----------------|
| `rust::clippy::needless_borrow` | drop the extra `&` when the value is already a reference |
| `rust::clippy::derive_partial_eq_without_eq` | derive `Eq` alongside `PartialEq` when fields allow |
| `rust::clippy::redundant_clone` | borrow instead of `.clone()` inside a read-only loop |
| `rust::clippy::manual_let_else` | prefer `let … else { return; }` over the longer form |

Four learnings share the `rust::clippy` prefix — past the recurrence threshold —
so the dream cycle promoted that cluster:

- **`semantic/recurrence_counts.json`** records the cluster: `rust::clippy`, count `4`.
- **`semantic/LESSONS.md`** distills the single highest-salience learning
  (`needless_borrow` — most recent, highest pain × importance) into one durable
  lesson, and that source event is now `pinned: true` back in the episodic log so
  decay never prunes it.

That is the wedge in one screen: the agent didn't just *record* four clippy
gotchas, it *compounded* them into one lesson it will carry forward.

## Regenerating this fixture

The semantic files are real dream-cycle output, reproducible byte-for-byte with
no network and no API key. From a copy of this `.agent/` tree:

```sh
SOURCE_DATE_EPOCH=1780056000 dreamd dream --no-commit
```

`SOURCE_DATE_EPOCH` pins the clock to `2026-05-29T12:00:00Z` (the seeded
timestamps sit inside its recurrence window); `--no-commit` skips the git
autobiography commit. Running it again over the committed tree is a no-op — the
fixture is a fixed point of the cycle.
