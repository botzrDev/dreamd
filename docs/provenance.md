# Provenance ledger format (`provenance/1.0`)

**Status: recording, a library check, and `dreamd forget` are implemented; signed proofs are not.** dreamd appends `lesson_citation`, `index_doc`, and `recurrence` edges to the ledger (BZR-155). `provenance::verify` in `dreamd-core` (BZR-148) recomputes the Merkle root from the well-formed lines and lists orphan edges, missing lesson and recurrence edges, and corrupt line numbers; it only reads, and it does not rewrite the file. `dreamd doctor --provenance` prints the `provenance::verify` report and does not rewrite the ledger. `dreamd doctor --repair` rebuilds the Tantivy index and does not touch the ledger. Ledger repair is not implemented. `forget::cascade` in `dreamd-core` (BZR-158) removes one event from the live JSONL and leaves existing ledger lines in place, so the forgotten id's old edge stays and `verify` reports it as an orphan; `dreamd forget <id>` removes one event through `forget::cascade` and refuses while a daemon is live. `--dry-run` writes nothing. `--proof <path>` writes a `forget-receipt/1.0` file (event id, lesson effect, recomputed root, and the orphan edges whose `from` is that id). That receipt is not the signed membership proof in the Proof object section below. Signatures and `embedding` edges are not implemented. This format is not part of the v0.1 `.agent/` contract in `SPEC.md`. It is not `.agent/.dreamd/snapshots/` (decay archives) and not `.agent/.dreamd/branches/` (branch objects and refs; see [`branching.md`](./branching.md)).

This page specifies how dreamd records which derived artifacts each episodic event feeds, how that set is committed to with a Merkle root, and how one event's membership is proved with a signed proof. It is the format. The signed proof and the verifier program are still later work. `dreamd forget --proof` writes the forget receipt named in the status paragraph and does not write the Proof object below.

### Implementation status

| Part of this page | In the tree |
|---|---|
| Edge records (`lesson_citation`, `index_doc`, `recurrence`) | Written (`dreamd-core::provenance`). `index_doc` edges are appended when the indexer commits a batch (the watch daemon's commit cadence, or startup replay). `lesson_citation` and `recurrence` edges are appended at the end of a dream cycle that leaves a `LESSONS.md` on disk, and again by `dreamd forget`. A `(kind, from, to)` triple already in the file is not appended twice. The file does not exist until the first edge. |
| `embedding` edge kind | Reserved. Nothing writes it, and the checker counts such a line as corrupt. |
| Merkle root | Recomputed on demand by `dreamd doctor --provenance` and `dreamd forget --proof`. It is not stored anywhere, so there is no earlier root to compare against. |
| Proof object (membership path + Ed25519 signature) | Reserved. Not implemented: there is no provenance key, nothing signs, and no command writes this object. |
| Verification procedure for a proof | Reserved. No verifier program or crate ships. |
| Forget receipt (`forget-receipt/1.0`) | Written by `dreamd forget --proof`. It is a different, unsigned file, described under [Commands](#commands) below. |
| Ledger repair | Not implemented. No command rewrites or truncates complete ledger lines. |

**Why this format also matters beyond forgetting (future).** The ledger records which derived artifacts each event feeds. The same edges are the substrate a trust label would ride on: a scalar per event saying where it came from and how far to trust it, joined along these edges on derivation, so that whatever releases an effect can refuse one built on data below its label. That rule is recorded as direction in the dreamOS charter in the momo kernel repository (RFC-011, accepted 2026-09-26; see `ROADMAP.md`, "Direction — provenance and trust"). This page specifies no label, no join rule, and no consumer of one; a trust label MUST NOT be inferred from any field defined here.

Conformance keywords (MUST, SHOULD, MAY, MUST NOT) are used per [RFC 2119](https://www.rfc-editor.org/rfc/rfc2119), same as [`SPEC.md`](../SPEC.md).

## Where it lives

The ledger lives under `<project>/.agent/.dreamd/provenance/`. `.dreamd/` is already gitignored ([`SPEC.md` §Folder layout](../SPEC.md#folder-layout); `dreamd init` writes the rule), so this tree is gitignored too.

The ledger MUST NOT live under `.agent/.dreamd/snapshots/`, which is the decay archive (`<YYYY-MM-DD>.jsonl`). It MUST NOT live under `.agent/.dreamd/branches/`, which holds the branch objects and refs in [`branching.md`](./branching.md).

```
<project>/.agent/.dreamd/provenance/
  ledger.jsonl
```

## Edge records

Edges are append-only lines in `provenance/ledger.jsonl`. A writer MUST NOT rewrite or reorder existing lines. Each line is compact UTF-8 JSON (no insignificant whitespace), terminated by exactly one `\n`, with keys in this order:

```json
{"schema_version":"provenance/1.0","kind":"lesson_citation","from":"evt_01ARZ3NDEKTSV4RRFFQ69G5FAV","to":"lsn_<lesson id>"}
```

- `schema_version` MUST be exactly `provenance/1.0`. It is not the episodic record token `"1.0.0"` and not the daemon state token `"1.0"`.
- `from` MUST be an episodic event id (`evt_` + 26 Crockford base32 characters).
- `kind` MUST be one of the values below.

| `kind` | `from` | `to` | When |
|---|---|---|---|
| `lesson_citation` | event id | `lsn_` + the lesson id | the lesson's `citations` list names that event |
| `index_doc` | event id | the Tantivy `event_id` of that episodic document, which is the same string as `from` | the document is in the index |
| `recurrence` | the lesson's exemplar event id | `skill_action` cluster key | one edge for the exemplar, not one edge per event in the count |
| `embedding` | — | — | reserved. A writer MUST NOT emit this kind. Vectors are not part of this format's implemented set. |

Notes on each kind:

- **`lesson_citation`** names the file-level `citations` field that `LESSONS.md` frontmatter already carries ([`SPEC.md`](../SPEC.md), [schemas digest](./spec/schemas.md)). This format adds no lesson schema. The lesson id is the `id` attribute on the `<!-- dreamd:lesson -->` tag, which is the exemplar event's id.
- **`recurrence`** mirrors `semantic/recurrence_counts.json`, which maps a `skill_action` cluster key to a count. The count is a property of the cluster, so the ledger records one edge from the exemplar to the key. A writer MUST NOT emit one `recurrence` edge per event that contributed to the count.
- A reader that meets an unknown `kind` SHOULD skip the line and continue.

A real ledger after three learns and one dream cycle that promoted the `rust::clippy` cluster:

```json
{"schema_version":"provenance/1.0","kind":"index_doc","from":"evt_01M3YEGYSZVX7594HBPM64XHR6","to":"evt_01M3YEGYSZVX7594HBPM64XHR6"}
{"schema_version":"provenance/1.0","kind":"index_doc","from":"evt_01M3YEGYTBD28RKHZKAEDN1BVD","to":"evt_01M3YEGYTBD28RKHZKAEDN1BVD"}
{"schema_version":"provenance/1.0","kind":"index_doc","from":"evt_01M3YEGYTMBJFFZDREWTC3K680","to":"evt_01M3YEGYTMBJFFZDREWTC3K680"}
{"schema_version":"provenance/1.0","kind":"lesson_citation","from":"evt_01M3YEGYSZVX7594HBPM64XHR6","to":"lsn_evt_01M3YEGYSZVX7594HBPM64XHR6"}
{"schema_version":"provenance/1.0","kind":"recurrence","from":"evt_01M3YEGYSZVX7594HBPM64XHR6","to":"rust::clippy"}
```

## Merkle root

The root commits to the set of events that have at least one edge.

1. **Leaves.** Collect every `from` value in the ledger. Remove duplicates. Sort ascending, comparing the raw strings byte by byte. Each leaf hash is the SHA-256 of that event id's UTF-8 bytes.
2. **Internal nodes.** Pair nodes left to right. A parent is `SHA-256(left || right)`, where `left` and `right` are the raw 32-byte child hashes, not their hex text.
3. **Odd node.** If a level has an odd count, the last node is promoted to the next level unchanged. It is not hashed with itself.
4. **Root.** Repeat until one node remains. The empty ledger's root is the SHA-256 of the empty byte string (`e3b0c442…b855`).

Roots and hashes are written as 64 lowercase hex characters.

The implemented checker (`provenance::verify`) takes its leaves from well-formed lines of the three written kinds only. A line that does not parse, has another `schema_version`, has kind `embedding`, or has a known kind with a malformed `from` / `to` is reported as corrupt and contributes no leaf. A line with an unknown `kind` is skipped and counted, and contributes no leaf. An orphan edge (its `from` is no longer in the live episodic log) is still a leaf.

## Proof object

**Reserved format; not implemented.** Nothing in dreamd writes or reads this object. `dreamd forget --proof` writes the forget receipt under [Commands](#commands), not this.

A proof shows that one event id is a leaf under a given root. It is one JSON object with keys in this order:

```json
{
  "schema_version": "provenance/1.0",
  "event_id": "evt_01ARZ3NDEKTSV4RRFFQ69G5FAV",
  "root": "<64 lowercase hex>",
  "path": [
    { "side": "left", "hash": "<64 lowercase hex>" },
    { "side": "right", "hash": "<64 lowercase hex>" }
  ],
  "signature": {
    "alg": "ed25519",
    "public_key": "<64 lowercase hex>",
    "sig": "<128 lowercase hex>"
  }
}
```

- `path` lists sibling hashes from the leaf toward the root. `side` says which side of the running hash the sibling sits on. A level where the running node was promoted as the odd node contributes no entry.
- `signature.alg` MUST be `ed25519`. `sig` is the 64-byte Ed25519 signature over the canonical bytes of the proof object with the `signature` key omitted: compact UTF-8 JSON, keys in the order above (`schema_version`, `event_id`, `root`, `path`, and within each path entry `side`, `hash`), no insignificant whitespace, no trailing newline.
- `public_key` is the daemon's provenance key. It MUST NOT be the bearer token in `~/.agent/auth.json`, which is the Windows API credential and is not a signing key. Key generation and storage are not specified by this document.

## Verification

**Reserved; not implemented.** A verifier does the following. This page states the procedure; no program in this repository carries it out.

1. Check `schema_version` is `provenance/1.0` and `signature.alg` is `ed25519`. Reject otherwise.
2. Set `h = SHA-256(UTF-8 bytes of event_id)`.
3. For each entry in `path`, in order: if `side` is `left`, set `h = SHA-256(hash || h)`; if `right`, set `h = SHA-256(h || hash)`. Both operands are raw 32 bytes decoded from hex.
4. Compare `h` with `root`. Reject on mismatch.
5. Rebuild the canonical bytes of the proof without the `signature` key, and verify `sig` over them with `public_key`. Reject on failure.

A proof that passes both checks shows the event id was a leaf under that root, signed by the holder of that key. Whether the root matches the current ledger is a separate check: recompute the root from `ledger.jsonl` as above and compare.

## Commands

Two commands use the ledger. Both run from inside a project with an `.agent/` store.

### `dreamd doctor --provenance`

Runs the usual `dreamd doctor` checks and adds the ledger check. It reads the ledger and the live memory files and writes nothing. On a clean ledger (a missing ledger is clean, with the empty-ledger root) it prints one line:

```text
provenance: ok root=947fd36f0f499ed5c8264760bed5f7dc24ee2dd216c5334946e5981897e04969
```

Otherwise it prints the root, one line per fault, and a count, and `doctor` exits 1:

```text
provenance: root=947fd36f0f499ed5c8264760bed5f7dc24ee2dd216c5334946e5981897e04969
provenance: orphan line=2 kind=index_doc from=evt_01M3YEGYTBD28RKHZKAEDN1BVD to=evt_01M3YEGYTBD28RKHZKAEDN1BVD
provenance: 1 fault(s)  [WARNING: ledger disagrees with the live memory files]
```

| Line | Meaning |
|---|---|
| `provenance: orphan line=<n> kind=… from=… to=…` | Ledger line `<n>` is a well-formed edge whose `from` event is not in the live episodic log. |
| `provenance: missing line=0 kind=… from=… to=…` | The current `LESSONS.md` (its `citations`, or its cluster in `recurrence_counts.json`) implies a `lesson_citation` or `recurrence` edge that the ledger does not have. |
| `provenance: corrupt line=<n>` | Ledger line `<n>` is not a valid edge (see Merkle root above). |
| `provenance: skipped_unknown_kind=<count>` | Lines with an unknown `kind` were skipped. Printed only when the count is above 0. Not a fault. |

An event with no `index_doc` edge is not reported: the index may not have committed it yet.

Orphans are expected, and permanent, after anything that removes an event from the live log while its edges stay in the append-only ledger: `dreamd forget`, the dream cycle's decay pruner, or `dreamd memory checkout` of an older snapshot. No command clears them. `dreamd doctor --repair` rebuilds the Tantivy index and does not touch the ledger.

### `dreamd forget <EVENT_ID>`

Removes one event from the live episodic log, pinned or not, and updates the lesson state that names it.

```text
$ dreamd forget --dry-run evt_01M3YEGYSZVX7594HBPM64XHR6
dry-run removed=true lesson=unlinked
$ dreamd forget --proof receipt.json evt_01M3YEGYSZVX7594HBPM64XHR6
removed=true lesson=unlinked proof=receipt.json
```

The result line goes to stdout. `removed` is `false` when the id is not in the live log; nothing is written in that case, including the receipt. `lesson` is one of:

| `lesson=` | What happened to `semantic/LESSONS.md` |
|---|---|
| `untouched` | There is no lesson file, or it does not name the id. |
| `citation_dropped` | The id was in `citations` but was not the lesson's exemplar. It was removed from `citations` and the file was rewritten. |
| `unlinked` | The id was the lesson's exemplar, so the lesson body was that event's content. The file was deleted. No replacement lesson is selected until the next dream cycle. |

After a removal the command also recounts `semantic/recurrence_counts.json` and, when `.agent/.dreamd/index/` exists, deletes the event's document from the recall index and re-indexes the lesson layer. It does not run a dream cycle and does not decay other events.

- `--dry-run` prints the same line prefixed with `dry-run`, plus `dreamd: dry run; nothing written` on stderr. It writes nothing and does not check for a running daemon. It cannot be combined with `--proof`.
- `--proof <PATH>` writes a forget receipt to `PATH` after a removal.
- Stop `dreamd watch` first. With a live daemon the command refuses: `dreamd: error — daemon is running; stop it first`.

| Exit | When |
|---|---|
| 0 | Removed, the id was not in the log (`removed=false`), or a dry run. |
| 1 | Daemon running, a dream cycle is marked in progress, or an I/O, index, or receipt-write failure. An index or receipt failure is reported after the result line; the removal itself is not rolled back. |
| 2 | The argument is not an event id (`evt_` + 26 Crockford base32 characters), no `.agent/` store was found, or a usage error. |

`dreamd forget` is not supported on Windows, where the atomic file replace it depends on is unavailable.

**The forget receipt.** One line of compact JSON with a trailing newline, keys in this order:

```json
{"schema_version":"forget-receipt/1.0","event_id":"evt_01M3YEGYTBD28RKHZKAEDN1BVD","removed":true,"lesson":"untouched","root":"947fd36f0f499ed5c8264760bed5f7dc24ee2dd216c5334946e5981897e04969","orphans":[{"line":2,"kind":"index_doc","from":"evt_01M3YEGYTBD28RKHZKAEDN1BVD","to":"evt_01M3YEGYTBD28RKHZKAEDN1BVD"}]}
```

`root` is the Merkle root recomputed from the ledger after the removal. `orphans` lists the ledger lines whose `from` is the forgotten id. The receipt is a record of what the command did. It is not signed, it has no `path` or `signature` key, and it is not the Proof object above. Because ledger lines are never removed, the forgotten id is still a leaf under `root`; the receipt shows the id's edges are now orphans, not that the id left the ledger.

**What `dreamd forget` does not remove.** It rewrites the live files only. The event can still be present in:

- the ledger, as the event id in old edge lines (ids only, never content);
- every memory snapshot object taken while the event was in the log (`.agent/.dreamd/branches/objects/`, one per dream cycle and per `dreamd memory branch`; see [`branching.md`](./branching.md)), and checking out such a snapshot brings the event back;
- your git history, if a dream cycle committed the episodic log or `LESSONS.md` (the autobiography commit that `dreamd dream --no-commit` skips).

An event the decay pruner already archived to `.agent/.dreamd/snapshots/<YYYY-MM-DD>.jsonl` is no longer in the live log, so `dreamd forget` reports `removed=false` for it and leaves the archive file as it is.
