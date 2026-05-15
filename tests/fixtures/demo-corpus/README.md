# demo-corpus

Frozen demo fixture consumed by DR-908 (canonical demo replay), DR-206 (recall API
golden), and DR-208 (criterion benchmarks). The 20 episodic entries in
`.agent/episodic/AGENT_LEARNINGS.jsonl` plus the empty `working/`, `semantic/`,
and `personal/` layers form a complete `.agent/` shaped exactly like what
`dreamd init` would produce.

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
notes live in `docs/demo-corpus.md` (gitignored — local-only).

## Recurrence note

`recurrence` is **not** stored on episodic events. WEG-42 derives it per
cluster at index time from the count of events sharing a `skill_action`.
EXPECTED.md cites the derived values as they apply to this fixture (cluster
sizes 4 / 2 / 2 / 3 / 2 / 2 / 2 / 2 / 1).
