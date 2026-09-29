# Using memory branches

This page covers how to use memory branches. The on-disk byte format (objects, refs, `HEAD`) is in [`branching.md`](branching.md).

A branch is a named snapshot of a project's four live memory files: the episodic log, `LESSONS.md`, the recurrence sidecar, and `PREFERENCES.md`. There is one live store. A branch is a saved copy of it that you can return to.

To try every command below in a throwaway project, run [`scripts/branch-demo.sh`](../scripts/branch-demo.sh).

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

This replaces the four live files with the branch's copies and makes it current. A file the snapshot does not have is removed. It is a whole-set replace, not a combination of two branches.

Stop `dreamd watch` first. The daemon holds the live episodic log open, so checkout refuses while its socket exists, and the error says so:

```
daemon socket /home/you/.agent/dreamd.sock exists; stop `dreamd watch` before checkout (it holds the live episodic log open)
```

The files are replaced one at a time. If the process dies partway through a checkout, the live store can be left with files from both branches. Run the checkout again.

## Learning after a checkout

New learns keep appending to the live episodic log. The branch ref does not move when they do. To save what you have learned since, make a new branch.

Durable appends go through the daemon: start `dreamd watch`, then `POST /api/v1/learn`. The request shape is in [`http-api.md`](http-api.md#post-apiv1learn). Do not edit the episodic log by hand. Remember that checkout needs the daemon stopped, so stop `dreamd watch` again before you switch branches.

## Delete a branch

```bash
dreamd memory delete before-refactor
```

This removes the ref. It refuses to delete the current branch; check out another one first. The snapshot object stays on disk under `.agent/.dreamd/branches/objects/`.

## Find when memory went bad: bisect

`dreamd memory bisect` binary-searches the `snap-*` refs for the first bad snapshot. It orders refs by the UTC timestamp on the second line of each ref file. There is no parent pointer, so a named branch is one point on that timeline, not a chain of history.

```bash
dreamd memory bisect start --good snap-20260920t090000z --bad snap-20260929t140300z
```

- `start --good <ref> --bad <ref>` begins the search and checks out the midpoint. Endpoints are ref names, or 64-hex object ids that some ref names. The good ref's timestamp must be strictly earlier than the bad ref's. If there is no `snap-*` ref between them, `start` prints the bad ref and does not start a search.
- `good` and `bad` mark the snapshot now checked out and check out the next midpoint.
- `run <script>` runs the script once with `sh -c` in the current directory, and marks good on exit 0, bad otherwise. It needs a bisect in progress.
- `start --auto-test <script>` runs the script after every checkout and keeps marking until the first bad ref is found.

Every step prints one line, `<name> <id>`. When the search is done, that line is the first bad ref.

Each step is an ordinary checkout, so bisect refuses while `dreamd watch` is running, the same way checkout does.

Bisect state is one file, `.agent/.dreamd/branches/bisect`, beside `HEAD`. It is not a ref, and `dreamd memory branches` does not list it.

## Not covered here

- **Merge is not implemented.** There is no merge, no conflict markers, and no way to combine two branches.
- **`dreamd memory diff` is not covered in this guide.**
- There is no HTTP route or MCP tool for branches. They are CLI-only.
