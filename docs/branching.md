# Branch storage format (`branches/1.0`)

**Status: implemented, except the detached `HEAD` form, which is specified below and never written.** The commands are `dreamd memory branch|checkout|branches|delete|diff|bisect`; the user guide is [`branching-guide.md`](branching-guide.md). This format is not part of the v0.1 `.agent/` contract in `SPEC.md`. Each dream cycle writes one snapshot object under `branches/objects/` and one `snap-<yyyymmddthhmmssz>` ref under `branches/refs/` (BZR-160), taken after the cycle starts and before it changes lessons or prunes the episodic log; the dream cycle does not write `branches/HEAD`. `dreamd memory branch|checkout|branches|delete` (BZR-150) write named refs and `branches/HEAD` (attached form only) and replace the live files on checkout. Checkout refuses while the Unix daemon socket file exists (including a stale socket) and, on Windows, while a loopback connect to the address in `server.json` succeeds. It replaces the four live files one rename at a time, writing HEAD only after all four succeed, so a crash mid-checkout can leave a torn live store. Learns keep appending to the live episodic log; no ref moves when they do. `dreamd memory diff <from> <to>` (BZR-156) compares two refs or object ids and only reads objects. Merge is not implemented and is not part of this format, and there is no HTTP route or MCP tool for this tree. The directory `.agent/.dreamd/snapshots/` is the decay archive (`<YYYY-MM-DD>.jsonl`) and is not this format.

`dreamd memory bisect start|good|bad|run` (BZR-153) binary-searches the `snap-*` refs by the timestamp on each ref's second line, from a good endpoint to a strictly later bad endpoint; there is no parent pointer to walk, and a named branch is one point on that timeline. Its state is one JSON file, `branches/bisect`, beside `HEAD`; it is not a ref and `dreamd memory branches` does not list it. Each step is an ordinary checkout, so it refuses the same way checkout does, and a crash between that checkout's renames is the same torn checkout described above.

### Implementation status

| Part of this page | In the tree |
|---|---|
| Snapshot objects, canonical manifest, object id | Written by every dream cycle and every `memory branch` (`dreamd-core::snapshot`). |
| Hardlink dedup between objects | Implemented, with the copy fallback. |
| Refs (two-line files) | Written for named branches and for `snap-*` autosnaps. A ref is written once: `memory branch` fails on an existing name, and no command moves a ref. The one exception is two autosnaps in the same UTC second, which share a `snap-*` name; the later one overwrites the ref and the earlier object stays on disk with no ref. |
| `HEAD`, attached form | Written by `memory branch`, `memory checkout`, and every bisect step. |
| `HEAD`, detached form | Reserved. Nothing writes it. A reader that finds it treats no branch as current (`memory branches` marks none with `*`). |
| Checkout | Implemented. It verifies the manifest hash against the object id and each file against its `sha256` before it replaces anything. |
| Removing objects | Not implemented. `memory delete` removes the ref only, and nothing garbage-collects `objects/`. |
| Merge | Not in this format. |

What the implementation does around the format, which this page does not otherwise specify:

- Checkout and bisect do not touch the Tantivy index or the provenance ledger. After a checkout, recall still serves the previous store's documents until the index is rebuilt (`dreamd doctor --repair`).
- `dreamd forget` rewrites the live episodic log only. An event it removes is still present in every object that was taken while the event was in the log, and checking out such an object brings it back.
- An object is written to `objects/<id>.partial/` and renamed into place; a leftover `.partial` directory is a crashed write and is replaced on the next write of that id. Refs, `HEAD`, and the bisect state are written to a dot-prefixed temp file beside the target (for example `refs/.<name>.tmp`) and renamed.
- `branches/bisect` is one compact JSON object, `{"good_idx":0,"bad_idx":3,"current_idx":1,"steps":["<ref name>", …]}`: the ordered ref names in the search range and three indexes into it. It exists only while a search is in progress. It is implementation state, not part of `branches/1.0`.

This page specifies how dreamd stores snapshots of a project's memory and names them as branches. It is the storage format only.

Conformance keywords (MUST, SHOULD, MAY, MUST NOT) are used per [RFC 2119](https://www.rfc-editor.org/rfc/rfc2119), same as [`SPEC.md`](../SPEC.md).

## Where it lives

Branch state lives under `<project>/.agent/.dreamd/branches/`. `.dreamd/` is already gitignored ([`SPEC.md` §Folder layout](../SPEC.md#folder-layout); `dreamd init` writes the rule), so this tree is gitignored too.

Branch objects MUST NOT live under `.agent/.dreamd/snapshots/`. That directory belongs to the episodic decay pruner, which archives pruned records into one date-stamped JSONL file per day.

```
<project>/.agent/.dreamd/branches/
  HEAD
  bisect                  (only while a `memory bisect` is in progress; not part of the format)
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

The implementation writes only the attached form (see Implementation status). `HEAD` is absent until the first `memory branch`; a dream-cycle autosnap does not create it.

## No merge

`branches/1.0` has no merge. An implementation of this version MUST NOT present merge, conflict markers, or merge commits as available.

Checkout replaces the four live files from the object. If the object does not list a file, checkout removes that live file. Checkout is a whole-set replace, never a combination of two branches.
