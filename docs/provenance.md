# Provenance ledger format (`provenance/1.0`)

**Status: recording, a library check, and `dreamd forget` are implemented; signed proofs are not.** dreamd appends `lesson_citation`, `index_doc`, and `recurrence` edges to the ledger (BZR-155). `provenance::verify` in `dreamd-core` (BZR-148) recomputes the Merkle root from the well-formed lines and lists orphan edges, missing lesson and recurrence edges, and corrupt line numbers; it only reads, and it does not rewrite the file. `dreamd doctor --provenance` prints the `provenance::verify` report and does not rewrite the ledger. `dreamd doctor --repair` rebuilds the Tantivy index and does not touch the ledger. Ledger repair is not implemented. `forget::cascade` in `dreamd-core` (BZR-158) removes one event from the live JSONL and leaves existing ledger lines in place, so the forgotten id's old edge stays and `verify` reports it as an orphan; `dreamd forget <id>` removes one event through `forget::cascade`, along with the copies dreamd keeps of it under `.agent/`, and refuses while a daemon is live. It does not rewrite git history; it reports the commits that hold the event. `--dry-run` writes nothing. `--proof <path>` writes a `forget-receipt/1.1` file (event id, lesson effect, recomputed root, the orphan edges whose `from` is that id, and the places cleared and remaining). That receipt is not the signed membership proof in the Proof object section below. Signatures and `embedding` edges are not implemented. This format is not part of the v0.2 `.agent/` contract in `SPEC.md`. It is not `.agent/.dreamd/snapshots/` (decay archives) and not `.agent/.dreamd/branches/` (branch objects and refs; see [`branching.md`](./branching.md)).

This page specifies how dreamd records which derived artifacts each episodic event feeds, how that set is committed to with a Merkle root, and how one event's membership is proved with a signed proof. It is the format. The signed proof and the verifier program are still later work. `dreamd forget --proof` writes the forget receipt named in the status paragraph and does not write the Proof object below.

### Implementation status

| Part of this page | In the tree |
|---|---|
| Edge records (`lesson_citation`, `index_doc`, `recurrence`) | Written (`dreamd-core::provenance`). `index_doc` edges are appended after every indexer commit: for a live batch, and for every log id a coverage pass covers (at open and on resync). One appender per process and project writes the file. It opens the file with `O_APPEND`, and a torn final line is cut when the project writer lock is taken. `lesson_citation` and `recurrence` edges come from the plan of a dream cycle that writes or keeps a `LESSONS.md`, or of a `dreamd forget`. The plan's redo WAL carries them, and its commit appends them after the renames, so a rolled-forward cycle appends the same edges. A `(kind, from, to)` triple already in the file is not appended twice: the appender reads the file's triples once, at its first append, and keeps them. The file does not exist until the first edge. |
| `embedding` edge kind | Reserved. Nothing writes it, and the checker counts such a line as corrupt. |
| Merkle root | Recomputed on demand by `dreamd doctor --provenance` and `dreamd forget --proof`. It is not stored anywhere, so there is no earlier root to compare against. |
| Proof object (membership path + Ed25519 signature) | Reserved. Not implemented: there is no provenance key, nothing signs, and no command writes this object. |
| Verification procedure for a proof | Reserved. No verifier program or crate ships. |
| Forget receipt (`forget-receipt/1.1`) | Written by `dreamd forget --proof`. It is a different, unsigned file, described under [Commands](#commands) below. dreamd 1.4 and earlier wrote `forget-receipt/1.0`, whose keys 1.1 keeps. |
| Ledger repair | Not implemented. No command rewrites or truncates complete ledger lines. |

**Why this format also matters beyond forgetting (future).** The ledger records which derived artifacts each event feeds. The same edges are the substrate a trust label would ride on: a scalar per event saying where it came from and how far to trust it, joined along these edges on derivation, so that whatever releases an effect can refuse one built on data below its label. That rule is direction, not shipped (see `ROADMAP.md`, "Direction — provenance and trust"). This page specifies no label, no join rule, and no consumer of one; a trust label MUST NOT be inferred from any field defined here.

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

Removes one event from the live episodic log, pinned or not, the lesson state built from it or naming it, and the copies of it that dreamd keeps under `.agent/`. It does not rewrite git history: it reports the commits that still hold the event and prints a command you can run (see [Git history](#git-history)).

```text
$ dreamd forget --dry-run evt_01M3YEGYSZVX7594HBPM64XHR6
would_remove=true lesson=unlinked
would_clear: live_log lessons recurrence_sidecar branch_objects index
would_touch: .agent/episodic/AGENT_LEARNINGS.jsonl
would_touch: .agent/semantic/LESSONS.md
would_touch: .agent/semantic/recurrence_counts.json
would_touch: .agent/.dreamd/branches/objects/3f5c…a91e
would_touch: .agent/.dreamd/branches/refs/snap-20261126t081300z
would_touch: .agent/.dreamd/index/
remaining: git_history commits=2 first=a7cde9d… last=461bf14… paths=.agent/episodic/AGENT_LEARNINGS.jsonl
$ dreamd forget --proof receipt.json evt_01M3YEGYSZVX7594HBPM64XHR6
removed=true lesson=unlinked proof=receipt.json
cleared: live_log lessons recurrence_sidecar branch_objects index
remaining: git_history commits=2 first=a7cde9d… last=461bf14… paths=.agent/episodic/AGENT_LEARNINGS.jsonl
verified: complete_in_store files=41 index_documents=212 text=checked
not_checked: git_history_contents git_reflogs_and_unreachable_objects git_remotes_and_clones mcp_client_transcripts llm_providers daemon_home backups_and_sync freed_disk_blocks
```

These lines go to stdout (commit ids are shortened here; the command prints them in full). The note about git history goes to stderr.

- Line 1 is the result. `removed` is `true` when the id was in the live log. `lesson` is described below.
- `cleared:` lists the places the command removed the event from, in the order of the table below. There is no `cleared:` line when it found the event nowhere it can clear; then it writes nothing, not even the receipt.
- Each `remaining:` line names one place where a copy is still kept.
- `verified:`, `still_present:` and `not_checked:` report the rescan; see [Rescan](#rescan).

| `cleared` | What the command did there |
|---|---|
| `live_log` | Removed the event's line from `episodic/AGENT_LEARNINGS.jsonl`. Every other line keeps its bytes. |
| `lessons` | Rewrote or deleted `semantic/LESSONS.md`; see `lesson` below. |
| `recurrence_sidecar` | Recounted `semantic/recurrence_counts.json` without the event. The file holds counts per cluster, never ids or content. |
| `decay_archive` | Removed the event's line from each `.agent/.dreamd/snapshots/<YYYY-MM-DD>.jsonl` that held it, where the decay pruner archives events. A day file left empty is deleted. |
| `migrate_backup` | Removed the event's line from `episodic/AGENT_LEARNINGS.jsonl.bak`, which `dreamd migrate` writes. A backup left empty is deleted. |
| `branch_objects` | Every autosnap and memory branch object under `.agent/.dreamd/branches/objects/` that held the event was rewritten without it, its refs were moved to the new object, and the old object was deleted. Objects that held the event and that no ref named were deleted once more than an hour old, the grace period object collection uses: `dreamd memory branch` names its object only after writing it. See [`branching.md`](./branching.md). |
| `index` | Deleted the event's document from `.agent/.dreamd/index/` and re-indexed the lesson layer, then merged every segment holding a deleted document and deleted the files the merge replaced. A Tantivy delete only marks a document deleted; until a merge, its stored text stays in the segment files. |

The changes to the live files, the decay archive, the migrate backup, the refs and `HEAD` commit through one redo WAL, the same one a dream cycle uses: after a crash the next writer rolls it forward, so those files are either as they were or fully forgotten. New branch objects are written before that commit, as a dream cycle writes its autosnap object, and nothing names them until it lands; the old objects are deleted after it, and a crash in between leaves an unnamed old object that the next forget of the id, or the next dream cycle, deletes once it is an hour old. The index pass runs after the commit, and the git history check only reads.

`lesson` is one of:

| `lesson=` | What happened to `semantic/LESSONS.md` |
|---|---|
| `untouched` | There is no lesson file, or it does not name the id and is not a model-written lesson over the id's cluster. |
| `citation_dropped` | The lesson is deterministic (`prompt_version` `deterministic-only`), and the id was in `citations` but was not its exemplar. Only a hand-edited file has that shape. The id was removed from `citations`, and the file was rewritten. |
| `unlinked` | The id was the lesson's exemplar. Or the lesson was written by a model, and the event was in its cluster (its `skill_action` is the lesson's `cluster_key` or under it) or among its citations: the model saw every event of the cluster, so its prose can hold any of them. The file was deleted, and no replacement lesson is selected until the next dream cycle. |

The same rule decides what happens to the `LESSONS.md` inside each branch object. Pins on the lesson's other citations stay; `dreamd archive --force-unpin <id>` removes one. The command does not run a dream cycle and does not decay other events.

| `remaining` | Meaning |
|---|---|
| `git_history` | Commits in the project's git repository that hold the event. See [Git history](#git-history). `unchecked` instead of a count means the repository could not be read. |
| `branch_object` | An object the command could not clear. `reason` is `failed_hash_check` (the object was edited or is corrupt), `unfinished_write` (a `.partial` directory left by a crash), or `not_cleared` (no ref names it and it is less than an hour old, or it could not be rewritten or deleted, or it holds the id only in a file the command does not rewrite). `refs` and `head` say what names it. |
| `decay_archive`, `migrate_backup` | The file still holds the event after the commit. |
| `index` | `refresh_failed`: the delete or the re-index failed, and recall may still return the event. `merge_pending`: the merge failed or left a segment holding deleted documents. `files_not_removed`: the merge worked, but some replaced files could not be deleted. `dreamd doctor --repair` deletes the index and rebuilds it from the log. |

#### Rescan

After the index pass, the command checks its own work instead of trusting its plan. It reads every regular file under `.agent/` (symlinks are not followed) except `.agent/.dreamd/writer.lock`, which the forget holds and which names only the holder's pid and kind, and every stored document of every index segment, deleted documents included, because segment files are compressed and their bytes alone do not show what they hold. It looks for two things: the event id, and the event's text (its `content`, read before the forget, held in memory only, and matched both raw and JSON-escaped). Text shorter than 16 bytes is not matched, because a short phrase matches unrelated files; then only the id is checked and `text=too_short` says so. `text=not_captured` means no copy of the record was found to read the text from.

`verified: complete_in_store` means the rescan read all of that and found neither the id nor the text, apart from the provenance ledger, which keeps the id in its old edges by design so the forget stays traceable (the receipt lists those files under `kept_by_design`; the ledger's text is still checked). `verified: partial` means at least one file still holds one of them, or could not be read. Then there is one line per file:

```text
still_present: agent_files .agent/working/WORKSPACE.md match=text
```

| `still_present` area | Where |
|---|---|
| `live_log`, `lessons`, `recurrence_sidecar`, `decay_archive`, `migrate_backup` | The files of the first table above. |
| `branch_objects` | `.agent/.dreamd/branches/`, any object, named or not. |
| `index` | `.agent/.dreamd/index/`. `match=index_document` is a segment's stored document, live or deleted; `match=unreferenced_segment_file` is a segment file no searchable segment uses, which the rescan cannot read and so does not count as clear. |
| `stage_and_wal` | `.agent/.dreamd/stage/` or the redo WAL, left by a crash. |
| `ledger` | The provenance ledger holding the text, which it never should. |
| `agent_files` | `working/`, `personal/`, `skills/`, `protocols/`: files a person or an agent wrote. The command never edits them. Remove the text yourself. |
| `other` | Any other file under `.agent/`, such as `.agent/.dreamd/agent.log`. |

`match` is `id`, `text`, `index_document`, `unreferenced_segment_file` or `unreadable`. A line can hold the text without the id: another event whose content repeats it is reported with `match=text`, though `forget` removes only the one event.

`not_checked:` names, every time, what the rescan does not read: the contents of git history (counted separately, see [Git history](#git-history)), reflogs and unreachable git objects, remotes and clones, MCP client transcripts, model providers that received the text, the daemon home `~/.agent/`, backups and sync, and freed disk blocks. `complete_in_store` is a statement about `.agent/` only.

The rescan finds exact text. It does not find a paraphrase, such as a model or an agent rewording the memory into another file. It does not stop the event from coming back: a `git checkout`, revert or merge of an older commit can restore the line, and nothing records that the id was forgotten.

- `--dry-run` prints `would_remove=`, `would_clear:` and one `would_touch:` line per file, object or ref it would change, plus the `remaining:` lines and, on stderr, the git note and `dreamd: dry run; nothing written`. It does not rescan. It writes nothing and does not check for a running daemon. It cannot be combined with `--proof`. `would_touch:` lists memory files, copies, objects and refs; every forget also updates `state.json`, and may append ledger edges for a lesson that stays.
- `--proof <PATH>` writes a forget receipt to `PATH` after a forget that cleared anything.
- Stop `dreamd watch` first. With a live daemon the command refuses: `dreamd: error — daemon is running; stop it first`.

| Exit | When |
|---|---|
| 0 | Something was cleared and the rescan says `complete_in_store`, the id was found nowhere (`removed=false` and no `cleared:` line), or a dry run. Git history listed as `remaining` does not change the exit code. |
| 1 | Daemon running, a dream cycle is marked in progress, or an I/O, index refresh, or receipt-write failure. An index or receipt failure is reported after the result lines; the removal itself is not rolled back. |
| 2 | The argument is not an event id (`evt_` + 26 Crockford base32 characters), no `.agent/` store was found, or a usage error. |
| 3 | The forget committed, and the rescan says `partial`: a file under `.agent/` still holds the id or the text, or could not be read. The `still_present:` lines name them. Exit 1 takes precedence. |

`dreamd forget` is not supported on Windows, where the atomic file replace it depends on is unavailable.

#### Git history

`dreamd forget` never rewrites git history. When the project is in a git repository, it reads every commit reachable from `HEAD` and from every ref (branches, tags, remote-tracking branches, the stash) and counts the ones whose `.agent/episodic/AGENT_LEARNINGS.jsonl` or `.agent/semantic/LESSONS.md` holds the event id. A dream cycle commits both files after each cycle, so an event that lived through one cycle is usually in at least one commit. The count, the first and last of those commits (by ancestry), and the paths go to stdout and the receipt. Reflogs and unreachable objects are not read.

On stderr it prints a command you can run yourself:

```sh
git filter-repo --blob-callback 'blob.data = (b"" if blob.data.startswith(b"---\nlast_updated:") else b"".join(l for l in blob.data.splitlines(True) if b"\"id\":\"evt_01M3YEGYSZVX7594HBPM64XHR6\"" not in l)) if b"evt_01M3YEGYSZVX7594HBPM64XHR6" in blob.data else blob.data'
```

In every file of every commit that holds the id, it empties a `LESSONS.md` (as `forget` deletes a lesson built from the event) and drops the line holding the record's `"id":"<id>"` from any other file, which in dreamd's files is the event's own log line. Before you run it:

- It needs [git-filter-repo](https://github.com/newren/git-filter-repo), a separate install.
- It rewrites the id of every commit on every branch, and ends with `git reset --hard`: commit or stash your work first.
- It refuses a repository that is not a fresh clone unless you add `--force`.
- Clones, forks and remotes keep their copies until each of them rewrites too, and pushing the rewrite needs `--force`. A hosting service can keep copies longer (pull request refs, caches).

The receipt never states that the event is gone from git. `remaining` lists the commits as they were when the command ran.

**The forget receipt.** One line of compact JSON with a trailing newline. The `forget-receipt/1.0` keys come first, in their 1.0 order and meaning, then the 1.1 keys `cleared`, `remaining` and `verification`:

```json
{"schema_version":"forget-receipt/1.1","event_id":"evt_01M3YEGYTBD28RKHZKAEDN1BVD","removed":true,"lesson":"untouched","root":"947fd36f0f499ed5c8264760bed5f7dc24ee2dd216c5334946e5981897e04969","orphans":[{"line":2,"kind":"index_doc","from":"evt_01M3YEGYTBD28RKHZKAEDN1BVD","to":"evt_01M3YEGYTBD28RKHZKAEDN1BVD"}],"cleared":["live_log","recurrence_sidecar","branch_objects","index"],"remaining":[{"place":"git_history","commits":2,"first":"a7cde9dc6e15a637a3264dc9abf0964b6aa4a6a2","last":"461bf14c8ec229de9cc9e429784eab056f0fb9a0","paths":[".agent/episodic/AGENT_LEARNINGS.jsonl"]}],"verification":{"status":"complete_in_store","files_read":41,"index_documents_read":212,"text":"checked","still_present":[],"kept_by_design":[{"area":"ledger","path":".agent/.dreamd/provenance/ledger.jsonl","match":"id"}],"not_checked":["git_history_contents","git_reflogs_and_unreachable_objects","git_remotes_and_clones","mcp_client_transcripts","llm_providers","daemon_home","backups_and_sync","freed_disk_blocks"]}}
```

`root` is the Merkle root recomputed from the ledger after the removal. `orphans` lists the ledger lines whose `from` is the forgotten id. `removed` is `false` only when the id was not in the live log and only copies of it were cleared (for example, an event the decay pruner had archived). `cleared` holds the tokens of the first table above. Each `remaining` entry is an object with a `place` key and the fields of the second table: `git_history` has `commits`, `first`, `last` and `paths` (or `unchecked`); `branch_object` has `object`, `refs`, `head` and `reason`; `decay_archive` and `migrate_backup` have `file`; `index` has `reason`. `verification` is the [Rescan](#rescan): `status`, `files_read`, `index_documents_read` (`null` without an index), `text`, `still_present` and `kept_by_design` (objects with `area`, `path` and `match`), and `not_checked`. The receipt holds ids, commit ids, paths relative to the project and counts. It never holds the memory's content. It is a record of what the command did and found. It is not signed, it has no `signature` key, and it is not the Proof object above. Because ledger lines are never removed, the forgotten id is still a leaf under `root`; the receipt shows the id's edges are now orphans, not that the id left the ledger.

**What `dreamd forget` does not remove.**

- Git history, until you rewrite it, and every clone, fork and remote until each one rewrites too.
- The messages of dream cycle commits, which list cited and pinned ids (`cited:`, `pinned:`). The printed command does not rewrite messages. They hold ids, not content.
- The ledger keeps the event id in its old edge lines (ids only, never content).
- Anything listed under `remaining` or `still_present`. Files under `working/`, `personal/`, `skills/` and `protocols/` are reported, never edited.
- The logs. The daemon log (`~/.agent/dreamd.log`, and `dreamd.log.1`) can hold the id in warning lines. With `DREAMD_LOG=debug` or `trace`, `dreamd mcp` logs each request in full, content included, to its stderr, which your harness may keep ([SECURITY.md](../SECURITY.md)).
- Another event whose own content repeats the text. `forget` removes one event, by id.
- Copies outside dreamd's files: another checkout of the project, backups, and any text already read out of dreamd, such as an agent's context or transcript, or another tool's memory.
- A branch object's recurrence sidecar keeps counts that include the event (counts only).
