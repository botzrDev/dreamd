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

`recurrence` is **not** stored on episodic events. Recall counts it from the
index: the number of live events whose `skill_action` equals the candidate's or
extends it with `::`. No key in this fixture extends another, so that is the
count of fixture records sharing the exact `skill_action`, and EXPECTED.md cites
those values (cluster sizes 4 / 2 / 2 / 3 / 2 / 2 / 2 / 2 / 1). The integration
test indexes each record's `skill_action` and reads recall's count. The dream
cycle's `semantic/recurrence_counts.json` is its promotion report; recall does
not read it, and this fixture ships no such file.
