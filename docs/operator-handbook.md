# Operator handbook

Runbook-style notes for operators maintaining a `dreamd` store by hand. This is
the seed of a larger handbook; today it covers unpinning in full and points at
the pages for the other maintenance commands.

## Maintenance commands at a glance

| Command | What it does | Daemon must be stopped? | Reference |
|---|---|---|---|
| `dreamd archive --force-unpin` | Clear `pinned` so decay can remove an entry | yes | below |
| `dreamd forget <id>` | Remove one episodic event and the lesson state that names it (`--dry-run`, `--proof <PATH>`) | yes (`--dry-run` does not check) | [provenance.md](./provenance.md) |
| `dreamd memory branch\|checkout\|branches\|delete\|diff\|bisect` | Snapshot, name, compare and restore the memory files | `checkout` and `bisect` only | [branching-guide.md](./branching-guide.md) |
| `dreamd doctor [--repair\|--cluster-health\|--provenance]` | Health checks; `--repair` rebuilds the Tantivy index and unlinks an orphaned socket | no | [troubleshooting.md](./troubleshooting.md) |
| `dreamd migrate --from 1.0.0 --to 1.0.0` | Schema migration stub (takes `.bak` copies) | no | [migrate.md](./migrate.md) |
| `dreamd salience-drift`, `dreamd blame <query>` | Read-only reports on salience and recall ranking | no | [observability.md](./observability.md) |
| `dreamd service …` | Login service for `dreamd watch` | n/a | [install.md](./install.md) |

Commands that rewrite the episodic log refuse while the daemon is live; check
with `dreamd status` (exit 0 and `daemon: running` when it is, exit 1 when it is
not).

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
  (`specify an event id or --all`, exit 2) — `--all` is opt-in so you never wipe
  every pin by accident.
- An event id **and** `--all` together is refused (`cannot combine an id with
  --all`, exit 2).
- An event id that is not in the log is refused (`no entry with id …`, exit 1)
  and the log is left untouched.
- `dreamd archive` without `--force-unpin` is refused (`archive requires
  --force-unpin (the only archive operation today)`, exit 2).

The command prints a summary (`N entr{y,ies} unpinned`). Clearing an already
unpinned entry is a harmless no-op, so a run that changes nothing reports
`0 entries unpinned` and does not rewrite the file.

Unpinning an entry that the current `LESSONS.md` still cites does not last: the
next dream cycle pins every cited exemplar again. The unpin sticks for entries
no lesson cites.

### Stop the daemon first

`dreamd archive` rewrites `AGENT_LEARNINGS.jsonl` in place (atomic temp + rename).
The daemon is the store's single writer: while it is running it holds an open file
descriptor on the log and appends by byte offset. If you rewrite the log
underneath a live daemon, it keeps writing to the old file and your unpin is
silently lost.

So the command **refuses to run while the daemon is live**:

```
dreamd: error — daemon is running; stop it first — dreamd cannot safely rewrite the log while the daemon holds it.
```

(exit 1)

Stop the daemon (end the `dreamd watch` / MCP process holding the socket), run the
unpin, then start it again. Check daemon liveness with `dreamd status`.

### Audit trail

Every id that is actually cleared is recorded with a `WARN`-level log line, so a
deliberate, destructive-adjacent action is at least announced:

```
{"timestamp":"2026-10-02T13:58:37.613321Z","level":"WARN","fields":{"message":"archive: force-unpinned episodic entry","event_id":"evt_01ARZ3NDEKTSV4RRFFQ69G5FAV"},"target":"dreamd::commands::archive"}
```

**This line goes to stderr only — it is not persisted.** `dreamd archive` is a
one-shot, and since AILAB-184 only the daemon (`dreamd watch`) writes to
`~/.agent/dreamd.log`; a one-shot is console-only. Nor was it durable before:
the file layer opened that log with `truncate(true)`, so the next `dreamd`
invocation of any kind erased the line. If you need to keep it, redirect stderr
yourself (`dreamd archive … 2>> unpin-audit.log`). A real append-mode/rotating
daemon log is tracked as AILAB-285.
