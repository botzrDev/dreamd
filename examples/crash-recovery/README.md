# crash-recovery

A **frozen mid-cycle state** showing what dreamd leaves behind when a dream cycle is interrupted — and how recovery cleans it up on the next start.

## What's in this fixture

```
.agent/
  episodic/
    AGENT_LEARNINGS.jsonl       # two valid lines + a torn tail (no trailing newline on partial JSON)
    AGENT_LEARNINGS.jsonl.tmp   # partial rewrite from interrupted prune
  .dreamd/
    dream_in_progress.wal       # PruneEpisodicMemory intent, no Commit
    state.json                  # last_dream_cycle_status: "in_progress"
```

This mirrors the `recover_incomplete_deletes_tmp_and_marks_failed` test in `crates/dreamd-core/src/wal.rs`.

## Recovery path

1. **Detect** — `recover_on_startup()` sees `dream_in_progress.wal` on `dreamd watch` startup (boot project) or on first request to a lazy-loaded project.
2. **Clean** — Delete temp files referenced by WAL intents (the `.jsonl.tmp` file).
3. **Finalize** — Remove the WAL; set `state.json` → `last_dream_cycle_status: "failed"`.
4. **Serve** — Daemon accepts traffic; JSONL retains the last valid lines (torn tail truncated on next coordinator append or `doctor --repair`).

## Try it

From this directory (treat `crash-recovery/` as the project root — add a sentinel if running live commands):

```bash
# Inspect the broken state
cat .agent/.dreamd/dream_in_progress.wal | jq .
cat .agent/.dreamd/state.json | jq .
tail -c 80 .agent/episodic/AGENT_LEARNINGS.jsonl | xxd   # torn tail visible

# Run recovery via daemon startup
dreamd watch
```

To exercise live recovery, copy this `.agent/` tree into a temp project with a `Cargo.toml` sentinel, run `dreamd watch`, and confirm the WAL disappears.

## Prevention

The dream cycle writes WAL intents **before** any destructive rename. A single-writer daemon (`dreamd watch`) serializes cycles — do not run `dreamd dream` in two terminals against the same project while the daemon is cycling (HTTP 409 guard).
