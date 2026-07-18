# `dreamd migrate`

Operator command that migrates the **durable** `.agent/` store between episodic
record schema versions. At v0.1 it is a **stub**: the only registered path is the
identity migration, which is a no-op. The trait + registry it sits on let v0.1.1
register real transforms without changing the command surface.

WEG-133 / DR-108.

## What `--from` / `--to` mean

`--from` and `--to` take an **episodic record** schema string — the
`schema_version` field stamped on every line of
`.agent/episodic/AGENT_LEARNINGS.jsonl`. That value is
`dreamd_protocol::RECORD_SCHEMA_VERSION`, which is `1.0.0` in v0.1.

dreamd carries three independent version streams. Only the first is a
`--from`/`--to` token:

| Stream | Where | Value (v0.1) | A migrate token? |
|---|---|---|---|
| Episodic record schema | `AGENT_LEARNINGS.jsonl` `schema_version` | `1.0.0` | **yes** |
| Daemon state schema | `.agent/.dreamd/state.json` `schema_version` | `1.0` | no |
| Tantivy index schema | `.agent/.dreamd/index_manifest.json` | `index/1.3` | no |

The daemon state schema (`1.0`) is what the `dreamd version` **display** line
prints as `schema:`. Do not read that as the episodic token — passing `--from`
with the state value is an unregistered path and is rejected.

## v0.1 behavior: the only registered path

The single registered migration is the identity transform of the current
episodic schema — `1.0.0` → `1.0.0` — and it does nothing but take backups (see
below). Any other pair is unregistered and exits `1`:

```bash
# v0.1 registers exactly one path; every other pair is rejected.
dreamd migrate --from 1.0.0 --to 1.0.0   # no-op; dreamd never registers another pair at v0.1
```

Output reports the three observed on-disk versions, then the result:

```
episodic schema: 1.0.0
state schema: 1.0
index schema: index/1.3
migrate: 1.0.0 → 1.0.0 (no-op)
```

An unregistered pair (for example, the daemon-state value, or a forward
transform that does not exist yet) fails:

```
dreamd: error — no migration registered for that path (…)
```

Exit codes:

| Situation | Exit |
|---|---|
| Success (registered no-op) | `0` |
| No `.agent/` store (run `dreamd init` first) | `2` |
| Unregistered `from → to` pair | `1` |

## `.bak` behavior

On the registered path, before the migration runs, `dreamd migrate` copies each
**present** durable file to a sibling `.bak`, overwriting any existing backup:

- `.agent/episodic/AGENT_LEARNINGS.jsonl` → `AGENT_LEARNINGS.jsonl.bak`
- `.agent/.dreamd/state.json` → `state.json.bak`

Missing sources are skipped. The v0.1 identity migration does not rewrite the
JSONL, so this is purely a safety net that a future non-identity transform
inherits. Because nothing rewrites the log, a running daemon can stay up during
a v0.1 `migrate`.

## The index is not migrated

The Tantivy index is **not** a migrate target and is never backed up or rewritten
by this command. When the on-disk index schema predates the binary, the daemon
self-heals it — it wipes the index and replays the JSONL under the current schema
on first open (ARCHITECTURE.md §4). `dreamd migrate` only *reads and reports* the
index schema; index rebuilds are owned entirely by that self-heal path.

## v0.1.1 and beyond

When the episodic schema next changes, v0.1.1 registers a real
`from → to` transform in the same registry. `dreamd migrate` will then rewrite
`AGENT_LEARNINGS.jsonl` in place (after the `.bak` copy) to bring older records
up to the current schema. The command surface — `--from` / `--to`, the `.bak`
safety net, the reporting lines — stays the same; only the registry gains
entries.
