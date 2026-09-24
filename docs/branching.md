# Branch storage format (`branches/1.0`)

**Status: not implemented.** This format is not part of the v0.1 `.agent/` contract in `SPEC.md`. dreamd does not read or write this tree. The directory `.agent/.dreamd/snapshots/` is the decay archive (`<YYYY-MM-DD>.jsonl`) and is not this format.

This page specifies how a future implementation stores snapshots of a project's memory and names them as branches. It is the storage format only: there is no CLI and no HTTP route for it. The snapshot model and branch commands are separate, later work, and they build on this page.

Conformance keywords (MUST, SHOULD, MAY, MUST NOT) are used per [RFC 2119](https://www.rfc-editor.org/rfc/rfc2119), same as [`SPEC.md`](../SPEC.md).

## Where it lives

Branch state lives under `<project>/.agent/.dreamd/branches/`. `.dreamd/` is already gitignored ([`SPEC.md` §Folder layout](../SPEC.md#folder-layout); `dreamd init` writes the rule), so this tree is gitignored too.

Branch objects MUST NOT live under `.agent/.dreamd/snapshots/`. That directory belongs to the episodic decay pruner, which archives pruned records into one date-stamped JSONL file per day.

```
<project>/.agent/.dreamd/branches/
  HEAD
  refs/
    <name>
  objects/
    <id>/
      manifest.json
      episodic/AGENT_LEARNINGS.jsonl
      semantic/LESSONS.md
      semantic/recurrence_counts.json
      personal/PREFERENCES.md
```

## Snapshot objects

A snapshot object is the directory `branches/objects/<id>/`, where `<id>` is 64 lowercase hex digits.

### Object id

`<id>` is the SHA-256 of the canonical manifest bytes (below). The manifest does not contain `<id>` and does not contain a timestamp. Two snapshots of identical file sets MUST produce the same `<id>`. When a snapshot was taken is recorded on the ref that points at it, not in the object.

### Canonical manifest

`manifest.json` is UTF-8, compact JSON, with keys in exactly this order, the `files` array sorted by `path` ascending (byte order), no whitespace outside strings, and a single trailing newline:

```json
{"schema_version":"branches/1.0","files":[{"path":"episodic/AGENT_LEARNINGS.jsonl","sha256":"<64 lowercase hex>","size":0}]}
```

- `schema_version` MUST be exactly `branches/1.0`. It is not the episodic record token `"1.0.0"` and not the daemon state token `"1.0"`.
- Each `files` entry has keys in the order `path`, `sha256`, `size`.
- `path` is relative to `.agent/`, with `/` separators.
- `size` is the byte length of the file.
- `sha256` is the lowercase-hex SHA-256 of those bytes.

The object id is computed over these exact bytes, trailing newline included. Any other serialization of the same data is a different id, so a writer MUST emit the canonical form.

### What a manifest lists

A manifest MAY list only these paths. Each is omitted when the live file is absent at snapshot time:

| `path` | Live file |
|---|---|
| `episodic/AGENT_LEARNINGS.jsonl` | the episodic log |
| `semantic/LESSONS.md` | lessons |
| `semantic/recurrence_counts.json` | recurrence sidecar |
| `personal/PREFERENCES.md` | preferences |

The manifest MUST NOT list the Tantivy index, `state.json`, `dream_in_progress.wal`, `working/`, `skills/`, `protocols/`, or anything under `snapshots/`. The index is rebuildable from the episodic log and lessons. The state file and WAL describe the daemon, not the memory.

### Object bytes and dedup

Next to `manifest.json`, the object directory holds each listed file's bytes at the same relative path.

When two objects contain a file with the same `sha256`, an implementation SHOULD hardlink the bytes rather than store a second copy. It MUST fall back to a full copy if the hardlink fails (for example across filesystems, or where hardlinks are unsupported).

A snapshot MUST be a stable byte image. It MUST NOT be a hardlink, symlink, or other alias of a live file the writer still has open. The episodic log is append-only and held open by the coordinator, so aliasing it would let later appends change a snapshot's bytes after its id was fixed. Dedup applies between objects only.

## Refs

A branch ref is the file `branches/refs/<name>`.

- `<name>` is one path segment matching `[a-z0-9][a-z0-9._-]{0,63}`.
- A ref name MUST NOT contain `..` or a slash.
- The file is two UTF-8 lines: the 64-hex object `<id>`, then the ISO-8601 UTC timestamp of when the ref was last moved (for example `2026-09-24T17:03:00Z`).

```
3f5c…(64 hex)…a91e
2026-09-24T17:03:00Z
```

## HEAD

`branches/HEAD` is one UTF-8 line, in one of two forms:

- Attached to a branch: `ref: refs/<name>`
- Detached: the 64-hex object `<id>`

## No merge

`branches/1.0` has no merge. An implementation of this version MUST NOT present merge, conflict markers, or merge commits as available.

Checkout replaces the four live files from the object. If the object does not list a file, checkout removes that live file. Checkout is a whole-set replace, never a combination of two branches.
