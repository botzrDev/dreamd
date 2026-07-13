# Operator handbook

Runbook-style notes for operators maintaining a `dreamd` store by hand. This is
the seed of a larger handbook; today it covers the one manual maintenance op that
`dreamd` ships.

## Unpinning episodic entries (`dreamd archive --force-unpin`)

Pinned episodic entries are sticky: they survive dream-cycle pruning, and the
dream cycle only ever *adds* pins (it unions the pins cited by `LESSONS.md` with
whatever an external writer already set — it never removes one). So an entry that
was pinned — by a harness, an import, or a hand edit — stays in the store forever
unless an operator clears the flag deliberately.

`dreamd archive --force-unpin` is that escape hatch. It clears the `pinned` flag
on episodic entries so the **next** pruning pass (`dreamd dream`) can decay and
remove them. It does not delete anything itself; it only makes an entry eligible
for the normal decay path again.

### Two target modes

```bash
# Unpin a single entry by its event id.
dreamd archive --force-unpin evt_01ARZ3NDEKTSV4RRFFQ69G5FAV

# Unpin every currently-pinned entry in the resolved project's store.
dreamd archive --force-unpin --all
```

Exactly one target is required:

- `--force-unpin` with **neither** an event id nor `--all` is refused
  (`specify an event id or --all`) — `--all` is opt-in so you never wipe every
  pin by accident.
- An event id **and** `--all` together is refused (`cannot combine an id with --all`).
- An event id that is not in the log is refused (`no entry with id …`) and the
  log is left untouched.

The command prints a summary (`N entr{y,ies} unpinned`). Clearing an already
unpinned entry is a harmless no-op, so a run that changes nothing reports
`0 entries unpinned` and does not rewrite the file.

### Stop the daemon first

`dreamd archive` rewrites `AGENT_LEARNINGS.jsonl` in place (atomic temp + rename).
The daemon is the store's single writer: while it is running it holds an open file
descriptor on the log and appends by byte offset. If you rewrite the log
underneath a live daemon, it keeps writing to the old file and your unpin is
silently lost.

So the command **refuses to run while the daemon is live**:

```
dreamd: error — daemon is running; stop it first — dreamd cannot safely
rewrite the log while the daemon holds it.
```

Stop the daemon (end the `dreamd watch` / MCP process holding the socket), run the
unpin, then start it again. Check daemon liveness with `dreamd status`.

### Audit trail

Every id that is actually cleared is recorded with a `WARN`-level log line so there
is a durable record of a deliberate, destructive-adjacent action:

```
WARN archive: force-unpinned episodic entry event_id=evt_01ARZ3NDEKTSV4RRFFQ69G5FAV
```

These land wherever the daemon log is configured (`~/.agent/dreamd.log` by
default; see [configuration.md](./configuration.md)).
