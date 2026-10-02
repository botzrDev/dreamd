# demo-corpus

Frozen demo fixture for DR-908 (canonical demo replay) and DR-206 (recall
golden). Its one automated consumer is
`crates/dreamd-core/tests/bm25_fastfield_integration.rs`, which reads the
episodic log and asserts the tables in `EXPECTED.md`
(`cargo test -p dreamd-core --test bm25_fastfield_integration`). The Criterion
recall bench (DR-208, `crates/dreamd-core/benches/recall.rs`) builds its own
synthetic corpus and does not read this fixture. The 20 episodic entries in
`.agent/episodic/AGENT_LEARNINGS.jsonl` plus the empty `working/`, `semantic/`,
and `personal/` layers follow the `.agent/` layer layout. Unlike a store made
by `dreamd init`, there is no `working/WORKSPACE.md` and no `.dreamd/`.

## Freeze window

The corpus is frozen between **end of Sprint 2** and the **DR-908 record**.
Any change inside that window requires a snapshot tag (see the Sprint 2
retro entry for procedure). Edits outside the window should bump the README
note and re-author `EXPECTED.md` in the same commit.

## Hard rule

Do **not** edit during a shoot week. The on-camera takes anchor against the
canonical results listed in `EXPECTED.md`; a drift here invalidates the
reel.

## Files

- `.agent/episodic/AGENT_LEARNINGS.jsonl` — 20 `AgentLearning` records, one
  per line, schema_version `"1.0.0"`.
- `.agent/working/`, `.agent/semantic/`, `.agent/personal/` — empty
  directories preserved via `.gitkeep`.
- `EXPECTED.md` — canonical top-3 scoring tables for the two demo queries.

## Provenance

Engineering provenance, tuning rationale, and the per-entry justification
notes live in `context/video/demo-corpus.md` (gitignored — local-only).

## Recurrence note

`recurrence` is **not** stored on episodic events. For this fixture the
integration test derives it itself, as the count of fixture records sharing the
exact `skill_action`, and EXPECTED.md cites those values (cluster sizes
4 / 2 / 2 / 3 / 2 / 2 / 2 / 2 / 1). The daemon does not count at index time:
it takes recurrence from `semantic/recurrence_counts.json`, which the dream
cycle writes, and this fixture ships no such file.
