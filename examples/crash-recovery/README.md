# crash-recovery

A **frozen mid-cycle state** showing what dreamd leaves behind when a dream cycle is interrupted — and how recovery cleans it up on the next start.

## What's in this fixture

```
.agent/
  episodic/
    AGENT_LEARNINGS.jsonl       # two valid lines + a third, incomplete JSON line
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
4. **Repair the log** — When the coordinator opens the episodic log, it keeps every well-formed record. A malformed line that ends in a newline is moved to a sidecar, `.agent/episodic/.corrupt-<YYYY-MM-DD>.jsonl`, and logged at WARN. A torn final fragment with no trailing newline is truncated in place. The broken third line in this fixture does end in a newline, so it takes the sidecar path.
5. **Serve** — Daemon accepts traffic; the JSONL holds the two valid lines.

## Try it

Inspect the broken state from this directory. These commands only read:

```bash
cat .agent/.dreamd/dream_in_progress.wal | jq .
cat .agent/.dreamd/state.json | jq .
tail -c 80 .agent/episodic/AGENT_LEARNINGS.jsonl | xxd   # the incomplete third line
```

Recovery rewrites the fixture, so run it on a copy, not in your checkout:

```bash
cp -r . /tmp/crash-recovery-demo && cd /tmp/crash-recovery-demo
dreamd watch        # Ctrl-C once it is up
```

`dreamd watch` finds the `.agent/` store in the current directory; no `dreamd init` is needed. Afterwards `dream_in_progress.wal` and `AGENT_LEARNINGS.jsonl.tmp` are gone, `state.json` says `"last_dream_cycle_status": "failed"`, `AGENT_LEARNINGS.jsonl` has two lines, and the third line is in `.agent/episodic/.corrupt-<today>.jsonl`. The daemon also builds its index under `.agent/.dreamd/`.

## Prevention

The dream cycle writes WAL intents **before** any destructive rename. A single-writer daemon (`dreamd watch`) serializes cycles — do not run `dreamd dream` in two terminals against the same project while the daemon is cycling (HTTP 409 guard).
