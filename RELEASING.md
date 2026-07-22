# Releasing dreamd

This is the canonical procedure for cutting a `dreamd` + `dreamd-mcp` release. It
exists because two silent-drift classes shipped a broken package once (see the
["Why"](#why-this-procedure-exists) section): a version constant that drifted from
`package.json`, and a bundled `manifest.json` whose binary checksums lagged a
release behind. **Follow every step in order.** The version number below is written
as `X.Y.Z` (e.g. `0.1.0-rc.5`); substitute throughout.

## 0. Prerequisites

- Push access to `botzrDev/dreamd` and permission to publish GitHub Releases.
- **npm publish requires the `dataprime1` account's passkey 2FA** — it CANNOT run in
  CI and CANNOT be done by an automated agent. A human with the passkey performs
  step 6.
- A clean, green `main` (`gh run list --branch main --limit 1` → success). CI's
  `test` job is gated `needs: lint`, so a red `lint` **hides all test failures** —
  never cut a release off a `main` whose lint is red.

## 1. Bump every version-coupled surface (ONE commit)

The version lives in **six** places that MUST move together. Missing any one is how
drift ships. Bump `X.Y.Z-1` → `X.Y.Z`:

| # | File | What to change |
|---|------|----------------|
| 1 | `Cargo.toml` | `[workspace.package] version` (all crates inherit via `version.workspace = true`) — and the `# RC:` comment above it |
| 2 | `Cargo.lock` | run `cargo check --workspace` to re-sync the three workspace crate entries (do not hand-edit) |
| 3 | `packages/dreamd-mcp/package.json` | `"version"` |
| 4 | `packages/dreamd-mcp/server.json` | BOTH `version` fields (top-level + `packages[0].version`) |
| 5 | `packages/dreamd-mcp/manifest.json` | `"version"` only — set the three `sha256` values to `PENDING_*` placeholders (they are filled in step 4 from the build) |
| 6 | `crates/dreamd-cli/tests/snapshots/cli_help__version_short.snap` and `…__version_long.snap` | the embedded `dreamd X.Y.Z` string (the binary reports `CARGO_PKG_VERSION`) |

Also:
- Add a `## [X.Y.Z] - YYYY-MM-DD` section to `CHANGELOG.md` (release notes are
  auto-extracted from it by `release.yml`; a missing section falls back to a generic
  link).

Do NOT leave real-but-stale shas in `manifest.json` (that is the exact bug this
procedure prevents) — use `PENDING_*`, which the manifest test rejects, so a
half-finished release cannot pass CI.

Verify locally before committing:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features --workspace            # cli_help version snaps must be green
( cd packages/dreamd-mcp && node --test )        # will FAIL on PENDING shas — expected until step 4
```

Commit on a `release/vX.Y.Z` branch (keep it OFF `main` until step 5 so `main` never
holds a `PENDING` manifest):

```sh
git checkout -b release/vX.Y.Z
git commit -am "chore: release vX.Y.Z (manifest shas pending build)"
```

## 2. Tag → trigger the build

Pushing the tag runs `.github/workflows/release.yml`, which builds all targets,
regenerates `manifest.json` from those exact binaries, and creates a **draft**
GitHub Release with the tarballs, `checksums.txt`, and `manifest.json` attached.

```sh
git tag vX.Y.Z              # points at the step-1 commit (PENDING manifest — fine; the build ignores it)
git push origin vX.Y.Z      # --no-verify is fine if clippy already passed locally
```

Watch it: `gh run watch $(gh run list --workflow release.yml --limit 1 --json databaseId --jq '.[0].databaseId') --exit-status`

## 3. (nothing — wait for the draft release)

When the run finishes, `gh release view vX.Y.Z` shows a **draft** with 6 assets.

## 4. Fill the bundled manifest FROM the release build

The `manifest.json` attached to the release was generated from the exact binaries in
the release, so its shas are guaranteed to match what users download. **Take it from
the release — do not regenerate separately** (local rebuilds are not byte-reproducible
and would mismatch):

```sh
gh release download vX.Y.Z --repo botzrDev/dreamd --pattern manifest.json \
  --output packages/dreamd-mcp/manifest.json --clobber
git commit -am "chore: fill vX.Y.Z manifest shas from release build"
```

Now the npm package's bundled manifest matches the published binaries.

## 5. Merge to main + publish the GitHub Release

```sh
git checkout main && git merge --ff-only release/vX.Y.Z && git push origin main
git branch -d release/vX.Y.Z
# main CI must be green (real shas → node --test passes):
gh run watch $(gh run list --branch main --limit 1 --json databaseId --jq '.[0].databaseId') --exit-status

gh release edit vX.Y.Z --repo botzrDev/dreamd --draft=false   # publish; download URLs now resolve
```

(The tag stays at the step-1 commit; `main` HEAD is the step-4 fill commit. This gap
is inherent — the manifest can only be filled after the build. The published npm
package comes from `main`, so it carries the correct shas.)

## 6. Publish to npm — HUMAN ONLY (2FA)

```sh
cd packages/dreamd-mcp
npm publish                      # prompts for the dataprime1 passkey 2FA; moves the `latest` dist-tag
npm view dreamd-mcp dist-tags    # confirm latest = X.Y.Z
```

If replacing a broken prior release, deprecate it:

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

## Why this procedure exists

- **VERSION drift:** `bin/dreamd-mcp.js` once hardcoded `VERSION`, which drove the
  download URL and cache path. When `package.json` moved and the constant didn't, the
  package downloaded and ran an **older** binary with no error. `VERSION` is now
  `require('../package.json').version`; a guard test (`test/version-consistency.test.js`)
  and the CI `mcp-shim` job keep it honest.
- **Manifest sha drift:** `release.yml` regenerates `packages/dreamd-mcp/manifest.json`
  on the runner and uploads it as a release asset, **but never commits it back**. The
  checked-in copy that ships in the npm package is hand-synced — step 4 is the sync.
  A future hardening is to have `release.yml` open a PR with the regenerated manifest;
  until then, step 4 is mandatory.
- **Masked CI:** the `test` job's `needs: lint` means a fmt error skips every test.
  Always confirm `main` lint is green (step 0) and re-run the full suite locally.
