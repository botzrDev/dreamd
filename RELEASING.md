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

The version lives in **seven** places that MUST move together. Missing any one is how
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

## 2. Tag → trigger the build

Pushing a tag that matches `v[0-9]+.[0-9]+.[0-9]+*` runs
`.github/workflows/release.yml`, which builds all targets with default features
(no `vectors`, no `mcp-http`), regenerates `manifest.json` from those exact binaries,
and creates a **draft** GitHub Release with the tarballs, `checksums.txt`, and
`manifest.json` attached.

- The workflow does **not** run the test suite. The tag is only as tested as the
  commit it points at (step 1's local run, and CI on that commit if it was pushed).
- The Linux and macOS builds fail if the stripped binary is over 20 MB
  (20,971,520 bytes, NFR-2). The Windows build is `continue-on-error`, unstripped and
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

When the run finishes, `gh release view vX.Y.Z` shows a **draft** with 7 assets: five
tarballs (`linux-x86_64`, `linux-x86_64-musl`, `darwin-x86_64`, `darwin-aarch64`,
`windows-x86_64`, each `.tar.gz`), `checksums.txt`, and `manifest.json`. If the
Windows build failed (it is `continue-on-error`) there are 6 and the release is still
usable — the npm shim has no Windows manifest entry.

## 4. Fill the bundled manifest FROM the release build (CI PR)

The `release.yml` run already regenerated `manifest.json` from the exact release
binaries, attached it to the draft release, and opened a PR
`chore/mcp-manifest-vX.Y.Z` with that fill. Review the PR: the shas in the diff must
match the `manifest.json` attached to the draft release (they are the same build —
**take them from the release, do not regenerate separately**; local rebuilds are not
byte-reproducible and would mismatch). Merge it. The PR branch is cut from the tag,
so when the step-1 commit lives only on `release/vX.Y.Z` the PR carries that bump commit
to `main` along with the fill.

**If the run ends red at "Open PR with regenerated manifest":** the draft release and
the pushed branch `chore/mcp-manifest-vX.Y.Z` both exist, and only `gh pr create`
failed. That is what happens when the repository setting *Allow GitHub Actions to
create and approve pull requests* is off (`GitHub Actions is not permitted to create or
approve pull requests`). Open the PR by hand from the branch the workflow pushed, then
review and merge as above:

```sh
gh pr create --repo botzrDev/dreamd --base main --head chore/mcp-manifest-vX.Y.Z \
  --title "chore: fill vX.Y.Z dreamd-mcp manifest shas" \
  --body "Fills packages/dreamd-mcp/manifest.json from the binaries attached to draft release vX.Y.Z."
```

A PR opened by a human this way does run CI.

A PR opened by the workflow's `GITHUB_TOKEN` does not run CI (GitHub's recursion guard); `node --test`
in `packages/dreamd-mcp` still runs on the merge to `main` (step 5 watch). If branch
protection refuses the merge because checks never started, that is a founder
PAT/GitHub-App follow-up — do not invent a secret here.

## 5. Publish the GitHub Release

The merged PR already landed the fill on `main`, so the ff-only merge of a local
`release/vX.Y.Z` is no longer the fill path. Confirm `main` CI is green (real shas →
`node --test` passes):

```sh
gh run watch $(gh run list --branch main --limit 1 --json databaseId --jq '.[0].databaseId') --exit-status

gh release edit vX.Y.Z --repo botzrDev/dreamd --draft=false   # publish; download URLs now resolve
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

```sh
npm login --auth-type=web              # browser flow; authenticate as dataprime1 with the passkey
npm whoami                              # must print: dataprime1

git checkout main && git pull --ff-only   # must contain the step-4 fill commit
grep -c PENDING packages/dreamd-mcp/manifest.json   # must print 0

cd packages/dreamd-mcp
npm publish                            # passkey-prompts; moves the `latest` dist-tag to X.Y.Z
npm dist-tag add dreamd-mcp@X.Y.Z next # keep `next` off any older/broken release
npm view dreamd-mcp dist-tags          # confirm latest = X.Y.Z (and next)
```

If replacing a broken prior release, deprecate it (also needs the login above):

```sh
npm deprecate dreamd-mcp@X.Y.Z-1 "Broken; upgrade to X.Y.Z+"
```

## 7. End-to-end verification (clean machine / fresh caches)

```sh
npx --yes dreamd-mcp@X.Y.Z version    # downloads the native binary, sha-verifies, prints X.Y.Z build info
npx --yes dreamd-mcp@X.Y.Z init       # scaffolds .agent/ in a project with a root sentinel
```

`init` scaffolds `.agent/` only (no `AGENTS.md`). Agent usage guidance reaches
clients automatically via the MCP `initialize` response's `server.instructions`; the
`adapters/*/AGENTS.md.snippet` files are optional manual copy-ins.

## 8. Publish / update MCP Registry metadata — owner-only (not CI)

Once the npm version is public, refresh the official MCP Registry entry
(`io.github.botzrDev/dreamd`) so it points at the new version: from
`packages/dreamd-mcp`, `mcp-publisher login github` (a GitHub identity
authorized for `botzrDev` — org membership must be public), then
`mcp-publisher publish server.json`. Full procedure, validation
(`just mcp-registry-validate`), and the confirming `curl` live in
`packages/dreamd-mcp/README.md` §Official MCP Registry. Deliberately not
automated in CI — no workflow holds registry credentials.

## Why this procedure exists

- **VERSION drift:** `bin/dreamd-mcp.js` once hardcoded `VERSION`, which drove the
  download URL and cache path. When `package.json` moved and the constant didn't, the
  package downloaded and ran an **older** binary with no error. `VERSION` is now
  `require('../package.json').version`; a guard test (`test/version-consistency.test.js`)
  and the CI `mcp-shim` job keep it honest.
- **Manifest sha drift:** `release.yml` regenerates `packages/dreamd-mcp/manifest.json`
  on the runner and uploads it as a release asset, then opens a `chore/mcp-manifest-v*`
  PR that writes it back to `main`. Step 4 is now "review and merge that PR" rather
  than a hand-sync, so the checked-in copy that ships in the npm package always matches
  the published binaries.
- **Masked CI:** the `test` job's `needs: lint` means a fmt error skips every test.
  Always confirm `main` lint is green (step 0) and re-run the full suite locally.
