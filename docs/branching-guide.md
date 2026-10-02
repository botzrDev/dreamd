# Using memory branches

This page covers how to use memory branches. The on-disk byte format (objects, refs, `HEAD`) is in [`branching.md`](branching.md).

A branch is a named snapshot of a project's four live memory files: the episodic log, `LESSONS.md`, the recurrence sidecar, and `PREFERENCES.md`. There is one live store. A branch is a saved copy of it that you can return to.

To see `branch`, `branches`, `checkout`, `delete`, and a `bisect start` run in a throwaway project, run [`scripts/branch-demo.sh`](../scripts/branch-demo.sh) with `dreamd` on your `PATH`. It works in a temp directory with its own `HOME` and removes it on exit. It does not run `diff` or the bisect `good` / `bad` / `run` steps.

Every `dreamd memory` command runs from inside a project that has an `.agent/` store. They exit 0 on success, 2 when no `.agent/` is found, and 1 on any other error; errors go to stderr as `dreamd: error — <message>`.

## Make a branch

```bash
dreamd memory branch before-refactor
```

This snapshots the live files, writes the ref `before-refactor`, and makes it the current branch. It prints the snapshot's 64-hex object id. The command fails if the name already exists. Names match `[a-z0-9][a-z0-9._-]{0,63}` and may not contain `..`.

Each `memory branch` also writes an automatic `snap-<timestamp>` ref for the same snapshot. That is why the list below shows more refs than you named.

## List branches

```bash
dreamd memory branches
```

One line per ref. `*` marks the current branch:

```
* before-refactor
  snap-20260929t140300z
```

`snap-*` names are automatic refs. Each dream cycle writes one before it changes lessons or prunes the log, and each `memory branch` writes one. You can check out a `snap-*` ref like any other.

## Check out a branch

```bash
dreamd memory checkout before-refactor
```

This replaces the four live files with the branch's copies, makes it current, and prints the object id. A file the snapshot does not have is removed. It is a whole-set replace, not a combination of two branches.

Checkout does not save what it replaces. Whatever changed in the live files since the last snapshot was taken is in no snapshot and is gone after the checkout. Run `dreamd memory branch <new-name>` first if you want to keep it.

Stop `dreamd watch` first. The daemon holds the live episodic log open. On Linux and macOS, checkout refuses while the socket file exists, including a socket left behind after the process died, and the error says so:

```
daemon socket /home/you/.agent/dreamd.sock exists; stop `dreamd watch` before checkout (it holds the live episodic log open)
```

On Windows the same commands refuse only while a loopback connect to the address in `server.json` succeeds. A `server.json` left behind after the process is gone does not block checkout.

The check is that the socket file exists, not that a daemon answers on it. If a crashed daemon left the file behind, `dreamd doctor --repair` unlinks an orphaned socket.

The files are replaced one at a time. If the process dies partway through a checkout, the live store can be left with files from both branches. Run the checkout again.

### After a checkout: rebuild the recall index

Checkout replaces the memory files only. It does not touch the recall index under `.agent/.dreamd/index/`, so `dreamd recall` can keep returning events and lessons from the store you just left. Restarting `dreamd watch` does not remove them. Rebuild the index from the checked-out log:

```bash
dreamd doctor --repair
```

The provenance ledger (`.agent/.dreamd/provenance/ledger.jsonl`, see [`provenance.md`](provenance.md)) is not part of a snapshot either. After you check out an older snapshot, `dreamd doctor --provenance` lists the edges of events that snapshot does not contain as orphans and exits 1.

## Learning after a checkout

New learns keep appending to the live episodic log. The branch ref does not move when they do. To save what you have learned since, make a new branch.

Durable appends go through the daemon: start `dreamd watch`, then `POST /api/v1/learn`. The request shape is in [`http-api.md`](http-api.md#post-apiv1learn). Do not edit the episodic log by hand. Remember that checkout needs the daemon stopped, so stop `dreamd watch` again before you switch branches.

## Delete a branch

```bash
dreamd memory delete before-refactor
```

This removes the ref. It refuses to delete the current branch; check out another one first. The snapshot object stays on disk under `.agent/.dreamd/branches/objects/`.

## Compare two snapshots

```bash
dreamd memory diff before-refactor after-refactor
```

`diff <from> <to>` compares two snapshots. It only reads objects: it checks nothing out, and it works while `dreamd watch` is running. Each side is a ref name, `<name>:<id>` (accepted only if that ref names that 64-hex object id), or a bare 64-hex object id. To compare against the live files, make a branch first; diff reads snapshots, not the live store.

The output is one line per added event (`added <id>`), removed event (`removed <id>`), and salience change (`salience <id> <from> <to>`), in that order, then three file verdicts that are always printed. For example, from an empty snapshot to one taken after three learns and a dream cycle:

```
added evt_01M3YEGYSZVX7594HBPM64XHR6
added evt_01M3YEGYTBD28RKHZKAEDN1BVD
added evt_01M3YEGYTMBJFFZDREWTC3K680
lessons added
preferences same
recurrence added
```

- `added` / `removed` compare event ids in the two episodic logs.
- `salience <id> <from> <to>` is an event on both sides whose salience differs. Salience here is the recall formula without the BM25 term, scored at the current time for both sides.
- `lessons`, `preferences`, and `recurrence` compare `LESSONS.md`, `PREFERENCES.md`, and the recurrence sidecar byte for byte: `same`, `added`, `removed`, or `modified`.

An event that is on both sides with the same salience is not listed, even if its record changed (for example, a dream cycle set `pinned` on it).

- `--unified` adds a dump of both versions of `LESSONS.md` when its verdict is `modified`: a `---`/`+++` header, every line of the `from` version prefixed `-`, then every line of the `to` version prefixed `+`. It is two full copies, not a line-by-line diff.
- `--json` prints one compact JSON object on one line instead of the text, and ignores `--unified`:

```json
{"events_added":[],"events_removed":[],"events_salience_changed":[],"lessons":"added","preferences":"same","recurrence":"added"}
```

Each `events_salience_changed` entry is `{"id":"evt_…","from":<number>,"to":<number>}`.

## Find when memory went bad: bisect

`dreamd memory bisect` binary-searches the `snap-*` refs for the first bad snapshot. It orders refs by the UTC timestamp on the second line of each ref file. There is no parent pointer, so a named branch is one point on that timeline, not a chain of history.

```bash
dreamd memory bisect start --good snap-20260920t090000z --bad snap-20260929t140300z
```

- `start --good <ref> --bad <ref>` begins the search and checks out the midpoint. Endpoints are ref names, or 64-hex object ids that some ref names. The good ref's timestamp must be strictly earlier than the bad ref's. If there is no `snap-*` ref between them, `start` prints the bad ref and does not start a search.
- `good` and `bad` mark the snapshot now checked out and check out the next midpoint.
- `run <script>` runs the script once with `sh -c` in the current directory, and marks good on exit 0, bad otherwise. It needs a bisect in progress.
- `start --auto-test <script>` runs the script after every checkout and keeps marking until the first bad ref is found.

Every step prints one line, `<name> <id>`. When the search is done, that line is the first bad ref. The line looks the same in both cases; the search is done when the state file (below) is gone, and a further `good` or `bad` then fails with `no bisect in progress`.

Each step is an ordinary checkout, so bisect refuses while `dreamd watch` is running, the same way checkout does. It also replaces your live files the same way: make a branch before `start` if the live store holds anything no snapshot has. The note about rebuilding the recall index applies to every step whose test uses `dreamd recall`.

When the search ends, the live files and `HEAD` stay on the last midpoint it checked out, which is usually not the first bad ref and not where you started. Check out the branch you want afterwards.

Bisect state is one file, `.agent/.dreamd/branches/bisect`, beside `HEAD`. It is not a ref, and `dreamd memory branches` does not list it. There is no `bisect reset`. To abandon a search, check out the branch you want and delete that file; a new `start` also overwrites it.

## Not covered here

- **Merge is not implemented.** There is no merge, no conflict markers, and no way to combine two branches.
- There is no HTTP route or MCP tool for branches. They are CLI-only.
