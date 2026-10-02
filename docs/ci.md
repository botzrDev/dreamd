# CI/CD pipeline reference

dreamd runs four GitHub Actions workflows:

| Workflow | File | Triggers |
|---|---|---|
| **CI** | [`.github/workflows/ci.yml`](../.github/workflows/ci.yml) | Push and PR to `main` |
| **Release** | [`.github/workflows/release.yml`](../.github/workflows/release.yml) | Version tags (`v[0-9]+.[0-9]+.[0-9]+*`, e.g. `v1.0.0`, `v0.1.0-rc.5`) |
| **Nightly** | [`.github/workflows/nightly.yml`](../.github/workflows/nightly.yml) | Daily at 06:00 UTC, or manual dispatch |
| **MCP manifest** | [`.github/workflows/mcp-manifest.yml`](../.github/workflows/mcp-manifest.yml) | Manual dispatch only |

This document mostly covers the **CI** workflow. Contributors hit these gates on every pull request. The other three are summarized [at the end](#release-workflow-tags-only).

Every Rust job in all four workflows installs the same pinned toolchain, `dtolnay/rust-toolchain@1.95.0`. [`rust-toolchain.toml`](../rust-toolchain.toml) pins the same `1.95.0` (with `rustfmt` and `clippy`) for local builds, so `cargo` in a checkout uses the CI compiler.

---

## Job overview

| Job | Merge gate? | One-liner |
|---|---|---|
| [Lint](#lint) | **Yes** | `cargo fmt` + `cargo clippy -D warnings` |
| [Security audit](#security-audit) | **Yes** | `cargo audit` against RustSec advisory DB |
| [License & dependency policy](#license--dependency-policy) | **Yes** | `cargo deny check` (licenses + advisories) |
| [Test](#test) | **Yes** (Linux + macOS) | `cargo test --all-features --workspace` on three OSes |
| [Binary size gate](#binary-size-gate) | **Yes** | Stripped release binary must be ≤ 20 MB (20,971,520 bytes) |
| [Idle-RSS gate](#idle-rss-gate) | **Yes** | Daemon idle memory < 30 MB (Linux) |
| [Tarball build sentinel](#tarball-build-sentinel) | **Yes** | `cargo build` without `.git/` must not leak vergen sentinels |
| [DCO sign-off](#dco-sign-off) | **Yes** (PRs only) | Every commit must have `Signed-off-by` trailer |
| [MCP shim](#mcp-shim) | **Yes** | `node --test` in `packages/dreamd-mcp` |
| [Install funnel](#install-funnel) | **Yes** | `scripts/alpha/install-funnel-suite.sh` (setup / doctor / update / conflicts) |
| [Binary size report (vectors feature)](#binary-size-reporting) | No | Informational stripped size of a `--features vectors` build; no limit |
| [Binary size (macOS)](#binary-size-reporting) | No | Informational size report |
| [Binary size (Windows)](#binary-size-reporting) | No | Informational, unstripped; `continue-on-error` |
| [Idle-RSS (macOS)](#idle-rss-macos-informational) | No | Informational `ps -o rss=` (no limit) |
| [Test coverage](#test-coverage) | No | HTML + lcov report; warnings only |
| [Alpha suite](#alpha-suite) | No (`continue-on-error`) | `scripts/alpha/alpha-suite.sh` — cross-harness append → recall against a real daemon |
| [Notify on main CI failure](#notify-on-main-ci-failure) | No | Slack webhook on `main` push failure |

Jobs marked **Yes** block merge when they fail. Informational jobs either use `continue-on-error` (Windows test, Windows size, alpha) or carry no threshold, so only a build failure can turn them red (vectors size, macOS size, macOS idle-RSS, coverage).

Lint, security audit, license policy, MCP shim and DCO start immediately. Every other job declares `needs: lint`, so a red lint **skips** test, the size and RSS gates, the tarball sentinel, coverage, alpha and the install funnel — fix lint first, then read the rest.

---

## Lint

**Runs on:** `ubuntu-latest`  
**Blocks merge:** Yes

```bash
cargo fmt --all -- --check
bash scripts/check-npx-floating.sh
cargo clippy --all-targets --all-features -- -D warnings
```

Lint also runs `scripts/check-npx-floating.sh` (no hard-pinned `dreamd-mcp@` in user-facing copy). The script shells out to ripgrep and swallows its errors, so without `rg` on `PATH` it passes without checking anything. `--all-features` compiles the optional `vectors` and `mcp-http` features too; the `vectors` build downloads a prebuilt ONNX Runtime at compile time, so this needs network on a cold cache.

### Common failures

| Output | Fix |
|---|---|
| `Diff in …` from `cargo fmt` | Run `cargo fmt --all` locally and commit |
| `warning: …` from clippy | Fix the lint; CI treats warnings as errors (`-D warnings`) |

---

## Security audit

**Runs on:** `ubuntu-latest`  
**Blocks merge:** Yes

```bash
cargo install cargo-audit   # one-time
cargo audit
```

Fails when a dependency matches an advisory in the [RustSec database](https://rustsec.org/). No advisory is ignored today: there is no `.cargo/audit.toml`, and `deny.toml` (which the next job reads, not this one) has no `ignore` entries either.

---

## License & dependency policy

**Runs on:** `ubuntu-latest`  
**Blocks merge:** Yes

```bash
cargo install cargo-deny
cargo deny check
```

Enforces license allowlist and advisory policy defined in `deny.toml`. CI pins `cargo-deny@0.19.8` — bump deliberately if upgrading.

---

## Test

**Runs on:** `ubuntu-latest`, `macos-latest`, `windows-latest`  
**Blocks merge:** Linux + macOS yes; Windows **no** (`continue-on-error`)

```bash
cargo test --all-features --workspace
```

Windows is in the matrix for visibility but is non-gating (`continue-on-error`). The crate **compiles** on Windows (AILAB-174). `dreamd watch` boots there over loopback TCP with a bearer token (AILAB-192) and `POST /api/v1/learn` works; the dream cycle and Tantivy index do not, because `io::write_atomic` is still `ErrorKind::Unsupported`. `uds` / `uds_server` / `client` stay Unix-only. Windows CI jobs remain informational and gate nothing.

### Reproduce a specific OS locally

You cannot run macOS CI on Linux. For Linux:

```bash
cargo test --all-features --workspace
```

---

## Binary size gate

**Runs on:** `ubuntu-latest`  
**Blocks merge:** Yes  
**Limit:** Stripped `target/release/dreamd` ≤ **20 MB** (20,971,520 bytes, NFR-2)

```bash
cargo build --release --workspace
strip target/release/dreamd
stat -c%s target/release/dreamd   # fails when > 20971520
```

This is the default-features build. CI emits a soft warning above **16 MB** (16,777,216 bytes); the current binary is over that line, so the warning fires on every run and is not a failure. Check the job summary for the measured size.

---

## Idle-RSS gate

**Runs on:** `ubuntu-latest`  
**Blocks merge:** Yes  
**Limit:** Daemon idle VmRSS < **30 MB** (NFR-1, Linux only)

```bash
cargo build --release -p dreamd
scripts/idle-rss.sh
```

The script spawns `dreamd watch` in a throwaway workspace, waits for the socket, samples `/proc/<pid>/status`, and prints MB to stdout. Override with `LIMIT_MB=30` or `SETTLE_SECS=2`. It uses your real `$HOME`: it removes `~/.agent/dreamd.sock` before starting and binds it again, so stop any `dreamd watch` of your own first (or run it with `HOME` pointed at a scratch directory).

### Idle-RSS (macOS, informational)

**Runs on:** `macos-latest`  
**Blocks merge:** No (informational; not in `notify-failure`)  
**Job:** `idle-rss-report-macos`  
**Metric:** `ps -o rss=` of the `dreamd watch` child (KiB → MB), no limit

Same build and measure commands as the Linux gate above. `scripts/idle-rss.sh` branches on `uname -s`: on Darwin it reuses the spawn / socket-readiness / 2 s settle path, samples `ps -o rss=`, prints MB, and exits 0 without comparing to `LIMIT_MB` (the env var is ignored on macOS). A build or daemon-spawn failure still fails the job; the RSS number never does. Read the value from the job's step summary and record it in `PERF.md` by hand.

`ps -o rss=` is not `phys_footprint` (Activity Monitor "Memory" uses a different accounting that excludes clean file-backed pages such as shared dylib text). The macOS number is recorded on its own and is not compared to the Linux 30 MB gate; a macOS threshold will be calibrated from the observed value + 20% headroom on a later ticket (AILAB-176).

---

## Tarball build sentinel

**Runs on:** `ubuntu-latest`  
**Blocks merge:** Yes

Simulates a crates.io tarball install (no `.git/`):

```bash
mv .git .git.disabled
cargo build --release --workspace
target/release/dreamd --version    # must contain "unknown", must NOT contain "VERGEN_"
target/release/dreamd version      # must NOT contain "VERGEN_"
mv .git.disabled .git
```

Catches regressions where vergen git metadata leaks into release binaries.

---

## DCO sign-off

**Runs on:** `ubuntu-latest`  
**Blocks merge:** Yes (pull requests only)

Every commit in the PR range must include a line that starts at column 0 and carries a name and an email:

```
Signed-off-by: Your Name <you@example.com>
```

```bash
# Sign the last N commits
git rebase --signoff HEAD~N
git push --force-with-lease
```

Or sign at commit time: `git commit -s -m "…"`

---

## MCP shim

**Runs on:** `ubuntu-latest`  
**Blocks merge:** Yes

```bash
cd packages/dreamd-mcp
node --test
```

Version ↔ manifest ↔ `server.json` consistency, arg routing, and redirect/tar safety for the `npx` shim. No Rust required; CI uses Node 20. `manifest.test.js` rejects `PENDING_*` sha placeholders, so this job is red on any commit that carries a version bump whose manifest has not been filled yet (see [`RELEASING.md`](../RELEASING.md)).

---

## Install funnel

**Runs on:** `ubuntu-latest`  
**Blocks merge:** Yes

```bash
cargo build -p dreamd
timeout 180 bash scripts/alpha/install-funnel-suite.sh
```

Cold `setup`, floating npx pin, `doctor` on the scaffold, idempotent re-run, `update --dry-run`, and the AILAB-548 MCP-conflict taxonomy. Starts no daemon.

---

## Binary size reporting

Three jobs report a size to the step summary and compare it to nothing. None is in `notify-failure`.

| Job | Runs on | Build | Notes |
|---|---|---|---|
| `size-report-vectors` | `ubuntu-latest` | `cargo build --release -p dreamd --bin dreamd --features vectors`, stripped | Own cache key, so the feature build never lands in the default NFR-2 measurement |
| `size-report-macos` | `macos-latest` | `cargo build --release`, stripped | |
| `size-report-windows` | `windows-latest` | `cargo build --release`, **unstripped** `dreamd.exe` | `continue-on-error: true` |

The CI NFR-2 gate (stripped `dreamd` ≤ 20 MB) measures the default Linux build only. The `vectors` feature is off by default and is not a release asset; its size is recorded, never gated. The Windows figure is never a gate. (The release workflow separately enforces the same 20 MB limit on its Linux and macOS release binaries.)

---

## Alpha suite

**Runs on:** `ubuntu-latest`  
**Blocks merge:** No (`continue-on-error: true`; not in `notify-failure`)

```bash
cargo build -p dreamd
timeout 180 bash scripts/alpha/alpha-suite.sh
```

Drives the real MCP stdio protocol in a sandboxed `HOME` with a real daemon: a learning appended as one harness must be recalled by an independent process, on both the daemon path and the JSONL-replay path. See [`scripts/alpha/README.md`](../scripts/alpha/README.md).

---

## Test coverage

**Runs on:** `ubuntu-latest`  
**Blocks merge:** No (report-only)

```bash
cargo install cargo-llvm-cov
rustup component add llvm-tools-preview
scripts/coverage.sh
```

Open `target/coverage/html/index.html` locally. CI uploads the HTML + `lcov.info` as a 14-day artifact named `coverage-report`.

| Threshold | Behavior |
|---|---|
| < 90% line coverage | Soft warning in job summary |
| < 85% line coverage | Stronger warning; does not fail (yet) |

To promote 85% to a blocking gate, uncomment `exit 1` in the workflow's "Summarize + threshold warnings" step.

---

## Notify on main CI failure

Posts to Slack when a **push to `main`** fails any of: lint, test, size-gate, tarball-sentinel, idle-rss-gate, audit, deny. Requires `SLACK_WEBHOOK_URL` secret. Does not run on PRs.

---

## Release workflow (tags only)

Triggered by pushing a tag matching `v[0-9]+.[0-9]+.[0-9]+*`. Not a PR gate for ordinary contributor PRs, and it does **not** run the test suite — a tag is only as green as the `main` commit it points at.

1. **Build** — `cargo build --release --target <triple>` (default features) for five targets, each packaged as `<asset>.tar.gz` holding the binary at its root:

   | Target | Asset | Notes |
   |---|---|---|
   | `x86_64-unknown-linux-gnu` | `linux-x86_64` | consumed by the `dreamd-mcp` shim |
   | `x86_64-unknown-linux-musl` | `linux-x86_64-musl` | static extra; the shim does not download it |
   | `x86_64-apple-darwin` | `darwin-x86_64` | consumed by the shim |
   | `aarch64-apple-darwin` | `darwin-aarch64` | consumed by the shim |
   | `x86_64-pc-windows-msvc` | `windows-x86_64` | `continue-on-error`; not stripped, no size check, no manifest entry |

   Linux and macOS binaries are stripped and fail the build above 20 MB (20,971,520 bytes, NFR-2).
2. **Release** — regenerates `packages/dreamd-mcp/manifest.json` from those exact binaries with `scripts/update-mcp-manifest.sh` (sha256 of the *extracted* `dreamd`, three shim platforms), writes `checksums.txt`, pulls the `## [X.Y.Z]` block out of `CHANGELOG.md` as the release notes, and creates a **draft** GitHub Release with the tarballs, `checksums.txt`, and `manifest.json` attached. A tag whose name contains a hyphen (`v0.1.0-rc.5`) is marked a prerelease; a hyphen-free tag (`v1.0.0`) is a stable release once the draft is published.
3. **Manifest PR** — the same job pushes a `chore/mcp-manifest-vX.Y.Z` branch with the regenerated manifest (a DCO-signed bot commit) and opens a PR to `main`. It never pushes to `main`.

Nothing here publishes to npm or the MCP Registry, and nothing takes the release out of draft. Those are human steps; the full procedure is [`RELEASING.md`](../RELEASING.md).

See [`.github/workflows/release.yml`](../.github/workflows/release.yml).

---

## Nightly workflow

Daily at 06:00 UTC (and on manual dispatch), on `ubuntu-latest`. One job runs three property-test suites at `PROPTEST_CASES=1024`, kept out of PR CI to keep the merge cycle short:

```bash
PROPTEST_CASES=1024 cargo test -p dreamd-core --test episodic_scan_proptest
PROPTEST_CASES=1024 cargo test -p dreamd-core --test salience_proptest
PROPTEST_CASES=1024 cargo test -p dreamd-protocol --test roundtrip_proptest
```

Nightly does not run the size gate, lint, or the full suite, so a green nightly says nothing about those.

---

## MCP manifest workflow

Manual dispatch only, with a `version` input (semver, no leading `v`). A pre-release helper: it builds the three shim targets (`linux-x86_64`, `darwin-x86_64`, `darwin-aarch64`), runs `scripts/update-mcp-manifest.sh`, and uploads the resulting `manifest.json` as the `mcp-manifest` artifact. It commits nothing and opens no PR. The normal release path does not need it — `release.yml` produces the manifest that ships.

---

## Quick local pre-push checklist

Run these before opening a PR to catch most CI failures in one pass:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features --workspace
cargo build --release -p dreamd && strip target/release/dreamd
```

`--all-features` compiles `vectors` and `mcp-http`. The `vectors` build downloads a prebuilt ONNX Runtime, so a cold cache needs network. Offline, drop `--all-features` (default features only; that is not the CI lint or test job):

```bash
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Optional deeper checks:

```bash
cargo audit
cargo deny check
scripts/idle-rss.sh
scripts/coverage.sh
```

---

## See also

- [../CONTRIBUTING.md](../CONTRIBUTING.md) — DCO, commit conventions, dev setup
- [../RELEASING.md](../RELEASING.md) — release procedure around `release.yml`
- [../PERF.md](../PERF.md) — last recorded size / RSS / latency numbers
- [../deny.toml](../deny.toml) — license and advisory policy
- [README.md](./README.md) — documentation index
