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

## Freeze

`EXPECTED.md` and `bm25_fastfield_integration` lock this corpus. A change to
the JSONL, the queries, or the scoring tables has to update `EXPECTED.md` in
the same commit and re-run that test. Local production notes, if any, are in
`context/video/demo-corpus.md`, which is gitignored and not part of a clone.

## Files

- `.agent/episodic/AGENT_LEARNINGS.jsonl` — 20 `AgentLearning` records, one
  per line, schema_version `"1.0.0"`.
- `.agent/working/`, `.agent/semantic/`, `.agent/personal/` — empty
  directories preserved via `.gitkeep`.
- `EXPECTED.md` — canonical top-3 scoring tables for the two demo queries.

## Recurrence note

`recurrence` is **not** stored on episodic events. For this fixture the
integration test derives it itself, as the count of fixture records sharing the
exact `skill_action`, and EXPECTED.md cites those values (cluster sizes
4 / 2 / 2 / 3 / 2 / 2 / 2 / 2 / 1). The daemon does not count at index time:
it takes recurrence from `semantic/recurrence_counts.json`, which the dream
cycle writes, and this fixture ships no such file.
