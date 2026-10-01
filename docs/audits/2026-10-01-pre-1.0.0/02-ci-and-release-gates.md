# Audit 2 — CI and release gates

**Verdict: a tag push does not run the suite that is red.** `release.yml` builds default-feature release binaries and opens a draft. It does not run `cargo test`. The workflow that would catch `version_output` is `ci.yml`, and it runs on branch pushes and pull requests, not on the tag. `bb84721` is local only (`main` is ahead of `origin/main` by 1), so GitHub Actions has not seen it.

Read from `.github/workflows/ci.yml`, `.github/workflows/release.yml`, and `.github/workflows/nightly.yml` on this commit. The npm shim was executed locally. The release build, size gate, coverage, audit, and deny jobs were not.

## What fails a branch push

`ci.yml` on ubuntu, with rustc 1.95.0:

| Job | Command or role | On `bb84721` |
| --- | --- | --- |
| `test` (ubuntu, macOS) | `cargo test --all-features --workspace` | Would fail. Same `schema:1.0)` assertion. Windows is `continue-on-error`. |
| `mcp-shim` | `node --test` in `packages/dreamd-mcp`, Node 20 | Would fail. Local Node (Cursor's v24) reproduced it: 37 passed, 1 failed. |
| `lint` | clippy `-D warnings`, fmt check | Not run this pass. |
| `size-gate` | stripped default `dreamd` must be ≤ 20,971,520 bytes | Not run this pass. |
| `tarball-sentinel`, `idle-rss-gate`, `audit`, `deny` | release-tarball sentinel, Linux RSS, cargo-audit, cargo-deny | Not run this pass. |
| `install-funnel` | `scripts/alpha/install-funnel-suite.sh` after `cargo build -p dreamd` | Local run passed 30/30. See audit 4. |

`notify-failure` depends on `lint`, `test`, `size-gate`, `tarball-sentinel`, `idle-rss-gate`, `audit`, and `deny`. It does not depend on `mcp-shim`, `coverage`, `alpha`, or `install-funnel`. A shim failure still fails the workflow; it does not, by itself, post that Slack message. The notify job only runs on a push to `main`.

## What is allowed to stay red

| Job | Why it does not block |
| --- | --- |
| `test` on `windows-latest` | `continue-on-error: true`. The comment above that flag still says `watch` / `mcp` exit 2 and that the daemon does not run on Windows. Loopback TCP + bearer has since shipped. `io::write_atomic` is still `ErrorKind::Unsupported` on Windows, and the only test of that stub is `#[cfg(windows)]` in `io.rs`, so this Linux pass did not compile it. |
| `alpha` | `continue-on-error: true`. Local run passed. See audit 4. |
| `coverage` | Report only. The 85% floor's `exit 1` is commented out. Soft warning at 90%. Not run this pass. |
| macOS size, macOS RSS, Windows size, `size-report-vectors` | Informational jobs. Not run. |
| `quality-suite.sh` | Not a job. `.github/` does not reference it. Local run passed. See audit 4. |

`nightly.yml` is cron plus `workflow_dispatch`. It runs three proptest targets at `PROPTEST_CASES=1024`: `episodic_scan_proptest`, `salience_proptest`, `roundtrip_proptest`. It is not part of the release. This pass used each file's own 512-case config and did not set the env override.

## Release workflow

`release.yml` triggers on tags matching `v[0-9]+.[0-9]+.[0-9]+*`. `v1.0.0` matches. `prerelease` is true only when the tag contains `-`, so this tag would be a stable release.

Jobs:

1. `build` — `cargo build --release` for the release matrix, default features. Windows build is `continue-on-error`. No test step.
2. `release` — draft GitHub release from the `## [1.0.0]` changelog block, then a pull request that fills `manifest.json` shas.

There is no `cargo test` in that file. A red `version_output` does not stop the draft from being created once the tag is pushed.

## Shim, measured locally

```text
node --test   # packages/dreamd-mcp, 38 tests
pass 37
fail 1
```

The failure is `manifest.test.js`: `binaries.linux-x86_64.sha256` is `PENDING_LINUX`, and the test requires `/^[a-f0-9]{64}$/`. The same placeholders exist for the two Darwin platforms. That failure is the intended state until the release workflow regenerates the manifest from the built artifacts. `version-consistency` and `registry-metadata` passed, including package version `1.0.0`. Routing tests passed for `watch`, `init`, `service`, `memory`, `forget`, `vectors`, `blame`, and `salience-drift`.

Pushing `main` at `bb84721` would fail `mcp-shim` on those placeholders and fail `test` on `version_output`. Pushing only the tag `v1.0.0` would start `release.yml` and would not start `ci.yml`.

## Hold

Two local commands are red for different reasons:

- `cargo test --workspace` — fix the assertion, then re-run. The printed line is already the 1.0.0 line.
- `node --test` in `packages/dreamd-mcp` — stays red until real shas replace `PENDING_*`. That is a release-workflow output, not a product defect in this commit.
