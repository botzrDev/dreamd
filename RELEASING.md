# Releasing dreamd

This is the canonical procedure for cutting a `dreamd` + `dreamd-mcp` release. It
exists because two silent-drift classes shipped a broken package once (see the
["Why"](#why-this-procedure-exists) section): a version constant that drifted from
`package.json`, and a bundled `manifest.json` whose binary checksums lagged a
release behind. **Follow every step in order.** The version number below is written
as `X.Y.Z` (e.g. `1.0.0`, or `0.1.0-rc.5` for a pre-release); substitute throughout.

## 0. Prerequisites

- Push access to `botzrDev/dreamd` and permission to publish GitHub Releases.
- **npm publish requires the `dataprime1` account's passkey 2FA** — it CANNOT run in CI
  and CANNOT be done by an automated agent. A human with the passkey performs step 6.
  Pushing the tag (step 2), taking the release out of draft (step 5), and the MCP
  Registry publish (step 8) are human steps too; no workflow and no agent session does them.
- The `gh` CLI, authenticated for `botzrDev/dreamd`.
- A clean, green `main` (`gh run list --branch main --limit 1` → success). CI's
  `test` job is gated `needs: lint`, so a red `lint` **hides all test failures** —
  never cut a release off a `main` whose lint is red.

## 1. Bump every version-coupled surface (ONE commit)

The version lives in **eight** places that MUST move together. Missing any one is how
drift ships. Bump `X.Y.Z-1` → `X.Y.Z`:

| # | File | What to change |
|---|------|----------------|
| 1 | `Cargo.toml` | `[workspace.package] version` (all crates inherit via `version.workspace = true`) — and the one-line comment above it that describes the release |
| 2 | `Cargo.lock` | run `cargo check --workspace` to re-sync the three workspace crate entries (do not hand-edit) |
| 3 | `packages/dreamd-mcp/package.json` | `"version"` |
| 4 | `packages/dreamd-mcp/server.json` | BOTH `version` fields (top-level + `packages[0].version`) |
| 5 | `packages/dreamd-mcp/manifest.json` | `"version"` only — set the three `sha256` values to `PENDING_*` placeholders (they are filled in step 4 from the build) |
| 6 | `crates/dreamd-cli/tests/snapshots/cli_help__version_short.snap` and `…__version_long.snap` | the embedded `dreamd X.Y.Z` string (the binary reports `CARGO_PKG_VERSION`) |
| 7 | `crates/dreamd-core/Cargo.toml` and `crates/dreamd-cli/Cargo.toml` | the three path-dependency `version` fields (`dreamd-core` on `dreamd-protocol`; `dreamd-cli` on `dreamd-core` and `dreamd-protocol`) — set each to the same string as the workspace version |
| 8 | `CITATION.cff` | `version` and `date-released` (the day the release is published, `YYYY-MM-DD`) |

A bare `version = "0.1.0-rc.2"` is a caret requirement (`^0.1.0-rc.2`), which stops
below `0.2.0`. `cargo check` rejects a workspace version of `0.2.0-alpha.1` until
those three fields move. A later `0.2.x` may still satisfy a caret written as
`0.2.0-alpha.1`; a new `0.x` minor will not. Set them to the workspace string on
every bump anyway.

Also:
- Add a `## [X.Y.Z] - YYYY-MM-DD` section to `CHANGELOG.md` (release notes are
  auto-extracted from it by `release.yml`; a missing section falls back to a generic
  link). Only that one block becomes the release notes — everything up to the next
  `## [` heading — so it must stand on its own; a line such as "see the previous
  section" leaves the GitHub Release without the content. Add the `[X.Y.Z]` link
  reference at the bottom of the file and repoint `[Unreleased]` at `vX.Y.Z...HEAD`.

- Move the version in six places no test checks: the root `Dockerfile`'s
  `ARG DREAMD_MCP_VERSION` (the MCP-directory build image), the version line in `GUIDE.md`'s first section, the `Version:`
  line in `llms.txt`, the version note and scope heading in `llms-full.txt`, and both
  fields in `plugins/dreamd/.claude-plugin/plugin.json`: `"version"` and the
  `dreamd-mcp@X.Y.Z` pin in `mcpServers`, and the release line in `SECURITY.md`'s
  "Supported versions" table and the sentence under it (`X.Y.x`, which a patch
  release leaves as it is). A plugin left on the old pin keeps its users on the old
  binary.

Do NOT leave real-but-stale shas in `manifest.json` (that is the exact bug this
procedure prevents) — use `PENDING_*`, which the manifest test rejects, so a
half-finished release cannot pass CI.

Verify locally before committing:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
bash scripts/check-npx-floating.sh                 # no hard-pinned dreamd-mcp@ in user-facing docs
cargo test --all-features --workspace            # cli_help version snaps must be green
( cd packages/dreamd-mcp && node --test )        # will FAIL on PENDING shas — expected until step 4
```

Commit on a `release/vX.Y.Z` branch (keep it OFF `main` until the step-4 PR merges, so
`main` never holds a `PENDING` manifest). If the bump is pushed to `main` instead, the
`mcp-shim` CI job is red on `main` from that push until step 4 lands:

```sh
git checkout -b release/vX.Y.Z
git commit -am "chore: release vX.Y.Z (manifest shas pending build)"
```

CI runs on pushes to `main` and to `release/**` branches, and on pull requests into
either, so pushing `release/vX.Y.Z` tests the bump commit on the branch's own code.
Before cutting the branch, check `Cargo.toml` on `origin/main`: a fix branch can be
merged into `main` ahead of its bump commit (1.2.1's fixes landed with the version
still at 1.2.0), and the bump has to be made on top of what `main` holds.

## 2. Tag → trigger the build

Pushing a tag that matches `v[0-9]+.[0-9]+.[0-9]+*` runs
`.github/workflows/release.yml`, which builds all targets with default features
(no `vectors`, no `mcp-http`), regenerates `manifest.json` from those exact binaries,
and creates a **draft** GitHub Release with the tarballs, `checksums.txt`, and
`manifest.json` attached.

- The workflow does **not** run the test suite. The tag is only as tested as the
  commit it points at (step 1's local run, and CI on that commit if it was pushed).
- The Linux and macOS builds fail if the stripped binary is over 24 MB
  (25,165,824 bytes, NFR-2). The Windows build is `continue-on-error`, unstripped and
  unchecked.
- A tag name containing a hyphen (`v0.2.0-alpha.1`, `v0.1.0-rc.5`) is created as a
  **prerelease**. A hyphen-free tag (`v1.0.0`) is a **stable** release: once it leaves
  draft in step 5 it becomes the repository's "Latest" release.

```sh
git tag vX.Y.Z              # points at the step-1 commit (PENDING manifest — fine; the build ignores it)
git push origin vX.Y.Z      # --no-verify is fine if clippy already passed locally
```

Watch it: `gh run watch $(gh run list --workflow release.yml --limit 1 --json databaseId --jq '.[0].databaseId') --exit-status`

## 3. (nothing — wait for the draft release)

When the run finishes, `gh release view vX.Y.Z` shows a **draft** with 8 assets: five
tarballs (`linux-x86_64`, `linux-x86_64-musl`, `darwin-x86_64`, `darwin-aarch64`,
`windows-x86_64`, each `.tar.gz`), `checksums.txt`, `manifest.json`, and
`dreamd-vX.Y.Z-provenance.sigstore.json` (the signed build provenance for the tarballs;
`gh attestation verify <tarball> --repo botzrDev/dreamd` checks one).
`linux-x86_64` is the static musl build, and `linux-x86_64-musl` is the same binary
under its old name; there is no glibc Linux asset (BZR-1438). If the
Windows build failed (it is `continue-on-error`) there are 7 and the release is still
usable — the npm shim has no Windows manifest entry.

## 4. Fill the bundled manifest FROM the release build (manifest PR)

The `release.yml` run already regenerated `manifest.json` from the exact release
binaries, attached it to the draft release, and pushed the branch
`chore/mcp-manifest-vX.Y.Z` with that fill. GitHub Actions is not allowed to open pull
requests in this repository, so the run does not open the PR. Its summary page has an
**Open the PR** link (also shown as a notice on the run) that opens the compare view
with the title filled in. Open the PR from that link, or with the command below. Review
the PR: the shas in the diff must
match the `manifest.json` attached to the draft release (they are the same build —
**take them from the release, do not regenerate separately**; local rebuilds are not
byte-reproducible and would mismatch). Merge it with a merge commit. The PR branch is
cut from the tag, so when the step-1 commit lives only on `release/vX.Y.Z` the PR carries
that bump commit to `main` along with the fill.

The `main` ruleset requires one review and signed commits and allows merge or rebase
only, with an admin bypass. Use `--merge`, never `--rebase`: a rebase rewrites the
bump commit's sha, and the tag then points at a commit that is not an ancestor of
`main` (that is why `v1.0.1` is not). An agent session cannot run the `--admin` merge;
run it yourself:

```sh
gh pr merge --repo botzrDev/dreamd chore/mcp-manifest-vX.Y.Z --merge --admin
git merge-base --is-ancestor vX.Y.Z origin/main && echo "tag is on main"   # after git fetch
```

To open the PR from a terminal instead of the link:

```sh
gh pr create --repo botzrDev/dreamd --base main --head chore/mcp-manifest-vX.Y.Z \
  --title "chore: fill vX.Y.Z dreamd-mcp manifest shas" \
  --body "Fills packages/dreamd-mcp/manifest.json from the binaries attached to draft release vX.Y.Z."
```

A PR opened by a human runs CI. Releases up to v1.4.0 ended red at the old "Open PR
with regenerated manifest" step for this reason (`GitHub Actions is not permitted to
create or approve pull requests`); the draft release and branch were fine.

## 5. Publish the GitHub Release

The merged PR already landed the fill on `main`, so the ff-only merge of a local
`release/vX.Y.Z` is no longer the fill path. Confirm `main` CI is green (real shas →
`node --test` passes):

```sh
gh run watch $(gh run list --branch main --limit 1 --json databaseId --jq '.[0].databaseId') --exit-status

gh release edit vX.Y.Z --repo botzrDev/dreamd --draft=false --latest   # publish and mark Latest; download URLs now resolve
```

(The tag stays at the step-1 commit; `main` HEAD is the step-4 fill commit. This gap
is inherent — the manifest can only be filled after the build. The published npm
package comes from `main`, so it carries the correct shas.)

## 6. Publish to npm — HUMAN ONLY (2FA)

npm auth expires between releases; **log in FIRST**. The `dataprime1` account uses a
**passkey** (WebAuthn), so the CLI OTP flow does not work — use the web flow. A
`404 Not Found` on `npm publish` (or `401` on `npm whoami`) means "not logged in /
not authorized" — npm deliberately hides package existence from unauthenticated
callers; it is NOT a missing package.

Run every command in this step in your own terminal. Under WSL, npm ignores
`BROWSER=`; `--no-browser` makes it print the auth URL and poll until you finish in a
browser. Every command here that asks for auth takes it, not only `npm login`: without
it, `npm publish` offers "Press ENTER to open in the browser", and pressing ENTER aborts
the publish (`Set the BROWSER environment variable`). A non-TTY runner (an agent
session, or Claude Code's `!` prefix) gets the auth URL redacted and npm aborts.

```sh
npm login --auth-type=web --no-browser # open the printed URL; authenticate as dataprime1 with the passkey
npm whoami                              # must print: dataprime1

git checkout main && git pull --ff-only   # must contain the step-4 fill commit
grep -c PENDING packages/dreamd-mcp/manifest.json   # must print 0

cd packages/dreamd-mcp
npm publish --no-browser                            # open the printed URL; passkey; moves `latest` to X.Y.Z
npm dist-tag add dreamd-mcp@X.Y.Z next --no-browser # keep `next` off any older/broken release
npm view dreamd-mcp dist-tags          # confirm latest = X.Y.Z (and next)
```

A new version can take about 5 minutes to reach the registry's read side. Until
`npm view dreamd-mcp@X.Y.Z version` prints it, `npm dist-tag add` fails; wait and run
it again.

If replacing a broken prior release, deprecate it (also needs the login above):

```sh
npm deprecate dreamd-mcp@X.Y.Z-1 "Broken; upgrade to X.Y.Z+"
```

## 7. End-to-end verification (clean machine / fresh caches)

```sh
npx --yes dreamd-mcp@X.Y.Z version    # downloads the native binary, sha-verifies, prints X.Y.Z build info
npx --yes dreamd-mcp@X.Y.Z init       # scaffolds .agent/ in a project with a root sentinel
```

Then run the published package on two older Linux userlands. Both must print
`dreamd X.Y.Z`:

```sh
docker run --rm node:22-bookworm npx --yes dreamd-mcp@X.Y.Z version
docker run --rm -e PATH=/node/bin:/usr/bin:/bin -v "$(dirname "$(dirname "$(readlink -f "$(command -v node)")")")":/node:ro ubuntu:22.04 npx --yes dreamd-mcp@X.Y.Z version
```

`init` scaffolds `.agent/` only (no `AGENTS.md`). Agent usage guidance reaches
clients automatically via the MCP `initialize` response's `server.instructions`; the
`adapters/*/AGENTS.md.snippet` files are optional manual copy-ins.

## 8. Publish / update MCP Registry metadata — owner-only (not CI)

Once the npm version is public, refresh the official MCP Registry entry
(`io.github.botzrDev/dreamd`) so it points at the new version: from
`packages/dreamd-mcp`, `mcp-publisher login github` (a GitHub identity
authorized for `botzrDev` — org membership must be public), then
`mcp-publisher publish server.json`. Run `mcp-publisher validate server.json` first: the
registry refuses a `description` longer than 100 characters. The login token expires within hours, so log in
again right before you publish, even if you logged in earlier the same day. Full procedure, validation
(`just mcp-registry-validate`), and the confirming `curl` live in
`packages/dreamd-mcp/README.md` §Official MCP Registry. Deliberately not
automated in CI — no workflow holds registry credentials.

## 9. Publish to crates.io — HUMAN ONLY

crates.io is a second install path (`cargo install dreamd`). It does not replace
npm or the GitHub Release. Do this after step 6, from the commit that is on
`main` for `X.Y.Z`, with a clean tree. An agent session does not publish.

All three crates are on crates.io, owned by `botzrDev`, since 1.3.1
(2026-11-16). The `0.0.0` reservation of `dreamd` was yanked that day.

Publish in dependency order. `cargo publish` uploads the crate and yanks nothing.
A version can be published once.

```sh
cargo package -p dreamd-protocol -p dreamd-core -p dreamd   # one run packages all three; a dirty tree fails it
# log in once: https://crates.io/settings/tokens — not a CI secret
cargo publish -p dreamd-protocol   # may time out waiting for the index; check it before the next one
cargo publish -p dreamd-core
cargo publish -p dreamd
cargo install dreamd --version X.Y.Z
dreamd version
```

`cargo install dreamd` with no version requirement then selects `X.Y.Z`, because
it is the highest version. Publish all three crates for every release, in that
order: each path dependency's `version` names the same release (step 1, row 7).

docs.rs builds `dreamd` and `dreamd-core` without the `vectors` feature
(`[package.metadata.docs.rs]`). Give it a few minutes, then open
`https://docs.rs/dreamd-protocol` and `https://docs.rs/dreamd`.

## Why this procedure exists

- **VERSION drift:** `bin/dreamd-mcp.js` once hardcoded `VERSION`, which drove the
  download URL and cache path. When `package.json` moved and the constant didn't, the
  package downloaded and ran an **older** binary with no error. `VERSION` is now
  `require('../package.json').version`; a guard test (`test/version-consistency.test.js`)
  and the CI `mcp-shim` job keep it honest.
- **Manifest sha drift:** `release.yml` regenerates `packages/dreamd-mcp/manifest.json`
  on the runner and uploads it as a release asset, then pushes a `chore/mcp-manifest-v*`
  branch and links the PR that writes it back to `main`. Step 4 is now "open, review and merge that PR" rather
  than a hand-sync, so the checked-in copy that ships in the npm package always matches
  the published binaries.
- **Masked CI:** the `test` job's `needs: lint` means a fmt error skips every test.
  Always confirm `main` lint is green (step 0) and re-run the full suite locally.
