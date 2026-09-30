# Provenance ledger format (`provenance/1.0`)

**Status: recording, a library check, and `dreamd forget` are implemented; signed proofs are not.** dreamd appends `lesson_citation`, `index_doc`, and `recurrence` edges to the ledger (BZR-155). `provenance::verify` in `dreamd-core` (BZR-148) recomputes the Merkle root from the well-formed lines and lists orphan edges, missing lesson and recurrence edges, and corrupt line numbers; it only reads, and it does not rewrite the file. `dreamd doctor --provenance` prints the `provenance::verify` report and does not rewrite the ledger. `dreamd doctor --repair` rebuilds the Tantivy index and does not touch the ledger. Ledger repair is not implemented. `forget::cascade` in `dreamd-core` (BZR-158) removes one event from the live JSONL and leaves existing ledger lines in place, so the forgotten id's old edge stays and `verify` reports it as an orphan; `dreamd forget <id>` removes one event through `forget::cascade` and refuses while a daemon is live. `--dry-run` writes nothing. `--proof <path>` writes a `forget-receipt/1.0` file (event id, lesson effect, recomputed root, and the orphan edges whose `from` is that id). That receipt is not the signed membership proof in the Proof object section below. Signatures and `embedding` edges are not implemented. This format is not part of the v0.1 `.agent/` contract in `SPEC.md`. It is not `.agent/.dreamd/snapshots/` (decay archives) and not `.agent/.dreamd/branches/` (branch objects and refs; see [`branching.md`](./branching.md)).

This page specifies how dreamd records which derived artifacts each episodic event feeds, how that set is committed to with a Merkle root, and how one event's membership is proved with a signed proof. It is the format. The signed proof and the verifier program are still later work. `dreamd forget --proof` writes the forget receipt named in the status paragraph and does not write the Proof object below.

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
| `index_doc` | event id | the Tantivy `event_id` of that episodic document | the document is in the index |
| `recurrence` | the lesson's exemplar event id | `skill_action` cluster key | one edge for the exemplar, not one edge per event in the count |
| `embedding` | — | — | reserved. A writer MUST NOT emit this kind. Vectors are not part of this format's implemented set. |

Notes on each kind:

- **`lesson_citation`** names the file-level `citations` field that `LESSONS.md` frontmatter already carries ([`SPEC.md`](../SPEC.md), [schemas digest](./spec/schemas.md)). This format adds no lesson schema. The lesson id is the `id` attribute on the `<!-- dreamd:lesson -->` tag, which is the exemplar event's id.
- **`recurrence`** mirrors `semantic/recurrence_counts.json`, which maps a `skill_action` cluster key to a count. The count is a property of the cluster, so the ledger records one edge from the exemplar to the key. A writer MUST NOT emit one `recurrence` edge per event that contributed to the count.
- A reader that meets an unknown `kind` SHOULD skip the line and continue.

## Merkle root

The root commits to the set of events that have at least one edge.

1. **Leaves.** Collect every `from` value in the ledger. Remove duplicates. Sort ascending, comparing the raw strings byte by byte. Each leaf hash is the SHA-256 of that event id's UTF-8 bytes.
2. **Internal nodes.** Pair nodes left to right. A parent is `SHA-256(left || right)`, where `left` and `right` are the raw 32-byte child hashes, not their hex text.
3. **Odd node.** If a level has an odd count, the last node is promoted to the next level unchanged. It is not hashed with itself.
4. **Root.** Repeat until one node remains. The empty ledger's root is the SHA-256 of the empty byte string (`e3b0c442…b855`).

Roots and hashes are written as 64 lowercase hex characters.

## Proof object

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

A verifier does the following. This page states the procedure; it does not ship the program.

1. Check `schema_version` is `provenance/1.0` and `signature.alg` is `ed25519`. Reject otherwise.
2. Set `h = SHA-256(UTF-8 bytes of event_id)`.
3. For each entry in `path`, in order: if `side` is `left`, set `h = SHA-256(hash || h)`; if `right`, set `h = SHA-256(h || hash)`. Both operands are raw 32 bytes decoded from hex.
4. Compare `h` with `root`. Reject on mismatch.
5. Rebuild the canonical bytes of the proof without the `signature` key, and verify `sig` over them with `public_key`. Reject on failure.

A proof that passes both checks shows the event id was a leaf under that root, signed by the holder of that key. Whether the root matches the current ledger is a separate check: recompute the root from `ledger.jsonl` as above and compare.
