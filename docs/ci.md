# CI/CD pipeline reference

dreamd runs two GitHub Actions workflows:

| Workflow | File | Triggers |
|---|---|---|
| **CI** | [`.github/workflows/ci.yml`](../.github/workflows/ci.yml) | Push and PR to `main` |
| **Release** | [`.github/workflows/release.yml`](../.github/workflows/release.yml) | Version tags (`v*.*.*`) |

This document covers the **CI** workflow. Contributors hit these gates on every pull request.

---

## Job overview

| Job | Merge gate? | One-liner |
|---|---|---|
| [Lint](#lint) | **Yes** | `cargo fmt` + `cargo clippy -D warnings` |
| [Security audit](#security-audit) | **Yes** | `cargo audit` against RustSec advisory DB |
| [License & dependency policy](#license--dependency-policy) | **Yes** | `cargo deny check` (licenses + advisories) |
| [Test](#test) | **Yes** (Linux + macOS) | `cargo test --workspace` on three OSes |
| [Binary size gate](#binary-size-gate) | **Yes** | Stripped release binary must be < 20 MB |
| [Idle-RSS gate](#idle-rss-gate) | **Yes** | Daemon idle memory < 30 MB (Linux) |
| [Tarball build sentinel](#tarball-build-sentinel) | **Yes** | `cargo build` without `.git/` must not leak vergen sentinels |
| [DCO sign-off](#dco-sign-off) | **Yes** (PRs only) | Every commit must have `Signed-off-by` trailer |
| [MCP shim](#mcp-shim) | **Yes** | `node --test` in `packages/dreamd-mcp` |
| [Install funnel](#install-funnel) | **Yes** | `scripts/alpha/install-funnel-suite.sh` (setup / doctor / update / conflicts) |
| [Binary size (macOS)](#binary-size-reporting) | No | Informational size report |
| [Binary size (Windows)](#binary-size-reporting) | No | Informational; Windows daemon deferred |
| [Idle-RSS (macOS)](#idle-rss-macos-informational) | No | Informational `ps -o rss=` (no limit) |
| [Test coverage](#test-coverage) | No | HTML + lcov report; warnings only |
| [Notify on main CI failure](#notify-on-main-ci-failure) | No | Slack webhook on `main` push failure |

Jobs marked **Yes** block merge when they fail. Informational jobs never fail the build (or use `continue-on-error`).

---

## Lint

**Runs on:** `ubuntu-latest`  
**Blocks merge:** Yes

```bash
cargo fmt --all -- --check
bash scripts/check-npx-floating.sh
cargo clippy --all-targets --all-features -- -D warnings
```

Lint also runs `scripts/check-npx-floating.sh` (no hard-pinned `dreamd-mcp@` in user-facing copy).

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

Fails when a dependency matches an advisory in the [RustSec database](https://rustsec.org/). Check `deny.toml` for intentionally ignored advisories.

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

Windows is in the matrix for visibility but is non-gating until Windows daemon support lands (DR-121). The server/MCP modules are Unix-only today.

### Reproduce a specific OS locally

You cannot run macOS CI on Linux. For Linux:

```bash
cargo test --all-features --workspace
```

---

## Binary size gate

**Runs on:** `ubuntu-latest`  
**Blocks merge:** Yes  
**Limit:** Stripped `target/release/dreamd` < **20 MB** (NFR-2)

```bash
cargo build --release --workspace
strip target/release/dreamd
stat -c%s target/release/dreamd   # must be < 20971520
```

CI emits a soft warning at **16 MB**. Check the job summary for the measured size.

---

## Idle-RSS gate

**Runs on:** `ubuntu-latest`  
**Blocks merge:** Yes  
**Limit:** Daemon idle VmRSS < **30 MB** (NFR-1, Linux only)

```bash
cargo build --release -p dreamd
scripts/idle-rss.sh
```

The script spawns `dreamd watch` in a throwaway workspace, waits for the socket, samples `/proc/<pid>/status`, and prints MB to stdout. Override with `LIMIT_MB=30` or `SETTLE_SECS=2`.

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
dreamd --version    # must contain "unknown", must NOT contain "VERGEN_"
mv .git.disabled .git
```

Catches regressions where vergen git metadata leaks into release binaries.

---

## DCO sign-off

**Runs on:** `ubuntu-latest`  
**Blocks merge:** Yes (pull requests only)

Every commit in the PR range must include:

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

Version ↔ manifest ↔ `server.json` consistency, arg routing, and redirect/tar safety for the `npx` shim. No Rust required.

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

**macOS and Windows jobs** — informational only, do not block merge.

Windows uses `continue-on-error: true` because the daemon does not yet compile on Windows.

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

Triggered by `v*.*.*` tags. Builds cross-platform release binaries, packages tarballs for the `dreamd-mcp` npm shim, and publishes to GitHub Releases. Not a PR gate.

See [`.github/workflows/release.yml`](../.github/workflows/release.yml).

---

## Quick local pre-push checklist

Run these before opening a PR to catch most CI failures in one pass:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features --workspace
cargo build --release -p dreamd && strip target/release/dreamd
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
- [../deny.toml](../deny.toml) — license and advisory policy
- [README.md](./README.md) — documentation index
