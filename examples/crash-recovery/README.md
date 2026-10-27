# crash-recovery

A **frozen mid-cycle state** showing what a pre-1.1.0 dreamd left behind when a dream cycle was interrupted — a **legacy** (`intents`) WAL that 1.1.0 still recovers the old way — and how recovery cleans it up on the next start.

## What's in this fixture

```
.agent/
  episodic/
    AGENT_LEARNINGS.jsonl       # two valid lines + a third, incomplete JSON line
    AGENT_LEARNINGS.jsonl.tmp   # partial rewrite from interrupted prune
  .dreamd/
    dream_in_progress.wal       # legacy WAL: PruneEpisodicMemory intent, no Commit
    state.json                  # last_dream_cycle_status: "in_progress"
```

This mirrors the `recover_keeps_the_legacy_meaning_of_an_intents_wal` test in `crates/dreamd-core/src/wal.rs`.

## Recovery path

1. **Detect** — `recover_on_startup()` sees `dream_in_progress.wal` when a writer takes the project writer lock: `dreamd watch` startup (boot project), the first request to a lazy-loaded project, an in-process `dreamd mcp`, or a CLI writer such as `dreamd dream --no-commit`.
2. **Clean** — Delete temp files referenced by WAL intents (the `.jsonl.tmp` file).
3. **Finalize** — Remove the WAL; set `state.json` → `last_dream_cycle_status: "failed"`.
4. **Repair the log** — When the coordinator opens the episodic log, it leaves every complete line where it is. A malformed line that ends in a newline stays in the log; readers skip it and `dreamd doctor` lists it. A final fragment with no trailing newline is truncated, or ends with a newline if it parses as a record. The broken third line in this fixture does end in a newline, so it stays.
5. **Serve** — Daemon accepts traffic; readers see the two valid lines.

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

`dreamd watch` finds the `.agent/` store in the current directory; no `dreamd init` is needed. Afterwards `dream_in_progress.wal` and `AGENT_LEARNINGS.jsonl.tmp` are gone, `state.json` says `"last_dream_cycle_status": "failed"`, `AGENT_LEARNINGS.jsonl` still has three lines: readers skip the broken third one, and `dreamd doctor` reports it. The daemon also builds its index under `.agent/.dreamd/`.

## Prevention

Since 1.1.0 a cycle stages its outputs and writes one redo WAL as its commit point. A single-writer daemon (`dreamd watch`) serializes cycles — do not run `dreamd dream` in two terminals against the same project while the daemon is cycling (HTTP 409 guard).
