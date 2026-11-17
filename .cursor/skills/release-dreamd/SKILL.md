---
name: release-dreamd
description: >-
  Cuts a dreamd and dreamd-mcp release on a release/vX.Y.Z branch: every
  version-coupled surface, the changelog, and local verification, then the
  human gates in RELEASING.md. Use when the user is ready to release, cut or
  ship a version, tag vX.Y.Z, publish dreamd-mcp, or fill the release manifest.
---

# Release dreamd

`RELEASING.md` is the contract. Read it at the start of every release. If it disagrees with this skill, follow `RELEASING.md` and say what differed.

Substitute `X.Y.Z` throughout. A hyphen in the tag (`vX.Y.Z-rc.1`) makes a prerelease. A hyphen-free tag becomes Latest once it leaves draft.

The bump commit lives on `release/vX.Y.Z`, cut from current `origin/main`. `main` must not hold `PENDING_*` shas. Do not bump on `main`.

## What you do, and where you stop

You edit, verify, commit the bump, watch CI, check shas, and open the fill PR if the workflow did not. You then stop and print **Your step** with the commands below. Do not push, tag, merge, or publish.

These stay the user's own terminal. An agent session cannot pass them: `gh` admin merge is refused, and npm redacts the auth URL and aborts.

| After | Your step |
|---|---|
| Bump commit exists locally | Tag and push. See [Tag](#tag). |
| Draft release exists and shas match | Merge the fill PR. See [Fill](#fill). |
| Fill is on `main` and that CI is green | Publish the GitHub Release. See [Publish the GitHub Release](#publish-the-github-release). |
| Release is public | npm. See [npm](#npm). |
| `npm view` prints `X.Y.Z` | MCP registry, then crates.io. See [Registry and crates.io](#registry-and-cratesio). |

One gate per turn. Do not skip ahead.

## Before editing

Stop, and say why, unless every line is true:

- The user named this release as the one to cut, or said they are ready to release. A mention of a version in some other task is not a release.
- `X.Y.Z` and the changelog date are confirmed. Ask for a missing version. Do not invent one. Default the date only after they accept today.
- `git status` is clean. `HEAD` is `origin/main`, or they named another commit and it is an ancestor they intend to ship.
- `Cargo.toml` on `origin/main` is the version you are bumping from. Fixes can land before the bump.
- `gh run list --branch main --limit 1` is success. A red `lint` job hides the `test` job (`needs: lint`). Do not cut from a red lint.
- No `release/vX.Y.Z` branch and no `vX.Y.Z` tag on the remote.

`[Unreleased]` in `CHANGELOG.md` is the notes. If it has bullets, move that text and show the section before committing. If it is empty, stop and ask for the notes. Do not write release notes from memory or from `AGENTS.md`.

The new `## [X.Y.Z] - YYYY-MM-DD` section must stand alone. `release.yml` uses only that block, up to the next `## [` heading. Add a short intro. Add an **Upgrading from …** paragraph when a running daemon, client, or on-disk file changes behavior; a patch that only changes docs does not need one. A security fix goes under `### Security`. Repoint the link references at the bottom: `[Unreleased]` becomes `vX.Y.Z...HEAD`, and add `[X.Y.Z]` from the previous tag to `vX.Y.Z`.

On a patch inside the current `SECURITY.md` line (`1.3.x` stays `1.3.x`), leave that table and the sentence under it. On a new minor or major, update both. Do not add a second supported line unless the user says so.

## Bump

One commit. Set the workspace version everywhere below. In `packages/dreamd-mcp/manifest.json`, set `"version"` and set each `sha256` to a `PENDING_…` string (`PENDING_LINUX`, `PENDING_DARWIN_X86_64`, `PENDING_DARWIN_AARCH64`). Never leave the previous release's digests.

| File | Change |
|---|---|
| `Cargo.toml` | `[workspace.package] version`, and the one-line comment above it |
| `crates/dreamd-core/Cargo.toml` | path dep `dreamd-protocol` version |
| `crates/dreamd-cli/Cargo.toml` | path deps `dreamd-core` and `dreamd-protocol` |
| `packages/dreamd-mcp/package.json` | `"version"` |
| `packages/dreamd-mcp/server.json` | both `"version"` fields |
| `packages/dreamd-mcp/manifest.json` | `"version"` and the three `PENDING_…` shas |
| `crates/dreamd-cli/tests/snapshots/cli_help__version_short.snap` | `dreamd X.Y.Z` |
| `crates/dreamd-cli/tests/snapshots/cli_help__version_long.snap` | `dreamd X.Y.Z` |
| `CHANGELOG.md` | section and link references, as above |
| `Dockerfile` | `ARG DREAMD_MCP_VERSION` |
| `README.md` | version badge, a new top row in **Release history**, and the **Status** line that names the tagged version |
| `GUIDE.md` | the version sentence in the first section |
| `llms.txt` | the `Version:` line |
| `llms-full.txt` | the version note and the `Current scope (vX.Y.Z)` heading |
| `plugins/dreamd/.claude-plugin/plugin.json` | `"version"` and the `dreamd-mcp@X.Y.Z` pin |
| `SECURITY.md` | only when the supported line changes, as above |

Leave the README cargo sentence that names the first installable version. Leave `cargo install dreamd --version ^1.3` on any 1.x release. Do not edit `AGENTS.md`.

`cargo check --workspace` rewrites `Cargo.lock`. Do not hand-edit it.

Search the tree for the old version string and account for every hit. `scripts/check-npx-floating.sh` forbids a hard-pinned `dreamd-mcp@` in user-facing docs; the plugin pin is the exception that file already allows. Do not add a pin anywhere else.

## Verify, then commit

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
bash scripts/check-npx-floating.sh
cargo test --all-features --workspace
( cd packages/dreamd-mcp && node --test )   # fails on PENDING shas — that failure only
```

`node --test` must fail only in `manifest.test.js` on the placeholders. Any other failure stops the release.

```sh
git checkout -b release/vX.Y.Z
git add -u
git commit -s -m "$(cat <<'EOF'
chore: release vX.Y.Z (manifest shas pending build)

EOF
)"
```

Do not `--no-verify`. Do not push. Print [Tag](#tag) and stop.

Commit locally only. The user runs any push.

## Tag

Your step, from a checkout of the bump commit:

```sh
git tag vX.Y.Z
git push origin vX.Y.Z
git push -u origin release/vX.Y.Z
```

The tag points at the bump commit. `release.yml` does not run the test suite. Linux and macOS builds fail if the stripped binary exceeds 24 MB (25,165,824 bytes). Windows is `continue-on-error`; a draft with 6 assets and no Windows tarball is still releasable.

When the user says the tag is pushed, watch the run. Do not push the tag yourself.

```sh
gh run watch $(gh run list --workflow release.yml --limit 1 --json databaseId --jq '.[0].databaseId') --exit-status
gh release view vX.Y.Z
```

A finished draft has the tarballs, `checksums.txt`, and `manifest.json`. `linux-x86_64` is the static musl binary. `linux-x86_64-musl` is the same binary under the old name.

## Fill

The workflow attaches `manifest.json` and pushes `chore/mcp-manifest-vX.Y.Z`. If **Open PR with regenerated manifest** is red, the draft and that branch still exist. Open the PR:

```sh
gh pr create --repo botzrDev/dreamd --base main --head chore/mcp-manifest-vX.Y.Z \
  --title "chore: fill vX.Y.Z dreamd-mcp manifest shas" \
  --body "Fills packages/dreamd-mcp/manifest.json from the binaries attached to draft release vX.Y.Z."
```

Review before handing off:

- The PR changes `manifest.json` shas (and brings the bump onto `main` when the bump is not already there).
- Those shas equal the `manifest.json` asset on the draft. Download that asset and diff. Do not rebuild locally to obtain shas.
- `grep PENDING` on the filled file prints nothing.

Your step:

```sh
gh pr merge --repo botzrDev/dreamd chore/mcp-manifest-vX.Y.Z --merge --admin
git fetch origin
git merge-base --is-ancestor vX.Y.Z origin/main && echo "tag is on main"
```

`--merge` only. A rebase rewrites the bump, and the tag then sits off `main`.

## Publish the GitHub Release

After the fill is on `main`:

```sh
gh run watch $(gh run list --branch main --limit 1 --json databaseId --jq '.[0].databaseId') --exit-status
```

`node --test` must pass on that run. Then your step:

```sh
gh release edit vX.Y.Z --repo botzrDev/dreamd --draft=false --latest
```

The tag stays on the bump commit. `main` HEAD is the fill commit. The npm package is published from `main`.

## npm

Do not run `npm login`, `npm publish`, or `npm dist-tag`. Print this and stop. Under WSL every auth command needs `--no-browser`. Account is `dataprime1` (passkey). `npm whoami` must print `dataprime1`.

```sh
npm login --auth-type=web --no-browser
npm whoami
git checkout main && git pull --ff-only
grep -c PENDING packages/dreamd-mcp/manifest.json   # 0
cd packages/dreamd-mcp
npm publish --no-browser
npm dist-tag add dreamd-mcp@X.Y.Z next --no-browser
npm view dreamd-mcp dist-tags
```

`dist-tag add` fails until `npm view dreamd-mcp@X.Y.Z version` prints `X.Y.Z`. Wait and retry. Do not deprecate the previous version unless the user says that release is broken.

## Verify the published package

After `npm view` prints `X.Y.Z`, run:

```sh
npx --yes dreamd-mcp@X.Y.Z version
docker run --rm node:22-bookworm npx --yes dreamd-mcp@X.Y.Z version
docker run --rm -e PATH=/node/bin:/usr/bin:/bin -v "$(dirname "$(dirname "$(readlink -f "$(command -v node)")")")":/node:ro ubuntu:22.04 npx --yes dreamd-mcp@X.Y.Z version
```

Each must print `dreamd X.Y.Z`. `npx --yes dreamd-mcp@X.Y.Z init` only in a scratch directory that has a root sentinel.

## Registry and crates.io

Do not log in or publish either. From `packages/dreamd-mcp`, after npm is public, the user's step is `packages/dreamd-mcp/README.md` §Official MCP Registry: `mcp-publisher validate server.json`, then `mcp-publisher login github`, then `mcp-publisher publish server.json`. Log in again the same day. Confirm with the `curl` in that section.

crates.io is next, from the `X.Y.Z` commit on `main`, clean tree, dependency order:

```sh
cargo package -p dreamd-protocol --allow-dirty=false
cargo package -p dreamd-core
cargo package -p dreamd
cargo publish -p dreamd-protocol
cargo publish -p dreamd-core
cargo publish -p dreamd
cargo install dreamd --version ^1.3
dreamd version
```

`cargo publish` can publish a version once. Yank `dreamd` `0.0.0` only if `cargo install` of `X.Y.Z` works and `0.0.0` is still yankable. Then check `https://docs.rs/dreamd-protocol` and `https://docs.rs/dreamd`.

## Done

Report the tag sha, the fill sha on `main`, the GitHub release URL, `npm view dreamd-mcp dist-tags`, the MCP registry curl, and `dreamd version` from crates.io. Note any gate the user has not run yet.
