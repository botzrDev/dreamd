# Contributing to dreamd

Thanks for your interest. `dreamd` is at v1.0.0. The spec ([`SPEC.md`](./SPEC.md)) is the frozen on-disk contract; the implementation still churns within that contract.

## Code of Conduct

Participation in this project is governed by the [Code of Conduct](./CODE_OF_CONDUCT.md). By contributing you agree to uphold it.

## Reporting bugs and proposing features

- **Bug:** open an issue using the *Bug report* template. Include the version, OS, and a minimal reproduction.
- **Feature or idea:** start a thread in [Discussions](https://github.com/botzrDev/dreamd/discussions) describing the use case and the gap. Blank issues are disabled; the issue chooser offers only the *Bug report* and *RFC* templates.
- **Spec change:** open an issue using the *RFC* template, with the title prefixed `[RFC]`. Spec changes are decided by discussion on the issue.
- **Security vulnerability:** **do not** open a public issue. See [SECURITY.md](./SECURITY.md).

### Good first issues

New to the codebase? Start here:
https://github.com/botzrDev/dreamd/issues?q=is%3Aissue+is%3Aopen+label%3A%22good+first+issue%22

## Development setup

Requirements:

- Rust stable. The toolchain is pinned in [`rust-toolchain.toml`](./rust-toolchain.toml) (`1.95.0`, with `rustfmt` and `clippy`) — the same version CI installs. With `rustup`, the first `cargo` command in the checkout fetches it.
- Network access on the first `--all-features` build: the optional `vectors` feature pulls a prebuilt ONNX Runtime at compile time. A plain `cargo build` does not.

```bash
git clone https://github.com/botzrDev/dreamd.git
cd dreamd

cargo build
cargo test --all-features --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
```

To run the CLI you just built, name the binary — the `dreamd` package ships two (`dreamd` and `generate_man`), so a bare `cargo run -p dreamd` errors out:

```bash
cargo run -p dreamd --bin dreamd -- --help
cargo run -p dreamd --bin dreamd -- doctor
```

CLI unit tests live in the library target (`cargo test -p dreamd --lib <filter>`); `cargo test -p dreamd --bin dreamd` runs zero tests and still exits 0.

Repository layout:

| Path | What |
|---|---|
| `crates/dreamd-core/` | Memory engine: Tantivy BM25 index, salience collector, episodic store, coordinator, dream cycle + WAL, HTTP server, MCP server |
| `crates/dreamd-cli/` | The `dreamd` binary (package name `dreamd`) and the `generate_man` helper |
| `crates/dreamd-protocol/` | Shared types: `AgentLearning`, `EventId`, HTTP schemas |
| `packages/dreamd-mcp/` | Node.js `npx` shim that downloads the prebuilt binary (`node --test` for its tests) |
| `adapters/` | Per-harness setup files and READMEs |
| `docs/`, `examples/`, `scripts/` | Reference docs ([index](./docs/README.md)), runnable walkthroughs, CI and release helpers |
| `tests/fixtures/` | Workspace-level golden files and frozen corpora |

### Task runner (`just`)

Common loops are wrapped in a root [`Justfile`](./Justfile) for convenience. Install [`just`](https://github.com/casey/just) (`cargo install just`, or your package manager), then run `just <recipe>`:

| Recipe | Runs | Purpose |
|---|---|---|
| `just dev` | `cargo build --workspace` | Debug build of the whole workspace. |
| `just test` | `cargo test --all-features --workspace` | Full test suite (mirrors the CI merge gate). |
| `just lint` | `cargo fmt --all -- --check` then `cargo clippy --workspace --all-targets --all-features -- -D warnings` | Formatting + clippy, exactly as CI runs them. |
| `just bench` | `cargo bench -p dreamd-core` | Recall-latency Criterion benchmarks. |
| `just release` | `cargo build --release -p dreamd`, `strip`, print size | Stripped `dreamd` CLI; prints its size. The NFR-2 hard limit (20 MB = 20,971,520 bytes) is enforced in CI, not here — the recipe's own output still labels the limit "15 MB", which is stale. |

`just` is optional and never a CI dependency — CI calls cargo directly. Without `just`, run the underlying cargo commands from the table above.

Install the git hooks once per clone. The pre-commit hook runs `cargo fmt --all -- --check` on every commit; the pre-push hook runs `cargo clippy --all-targets --all-features -- -D warnings` before code reaches the remote. This catches formatting and lint drift locally instead of turning CI red:

```bash
# Install the pre-commit and pre-push hooks (one-time, per clone)
git config core.hooksPath .githooks
```

CI runs lint and test on Linux and macOS as merge gates, plus Linux-only gates for binary size, idle RSS, the no-`.git` tarball build, the npm shim, and the install funnel. Windows jobs exist but are `continue-on-error`: the workspace compiles there and `watch` serves learn over loopback TCP, but the dream cycle and the index do not run on Windows. Linux and macOS gates must be green before merge. Every job is described in [`docs/ci.md`](./docs/ci.md).

## Your first contribution in ~15 minutes

A start-to-finish loop for your first PR. Each step links to the section with the details rather than repeating them.

1. **Clone the repo** (see [Development setup](#development-setup) for the requirements):

   ```bash
   git clone https://github.com/botzrDev/dreamd.git
   cd dreamd
   ```

2. **Prove your toolchain works by running the lint gate.** Install `just` first if you don't have it — the [Task runner (`just`)](#task-runner-just) section covers installation and the full recipe table:

   ```bash
   just lint
   ```

   This runs formatting + clippy exactly as CI does; a clean pass means your environment is ready. (No `just`? Run the underlying cargo commands from the recipe table instead.)

3. **Pick an issue** from the good-first-issue filter linked in [Good first issues](#good-first-issues).

4. **Make your change and open a PR** following the checklist in [Pull requests](#pull-requests) — topic branch from `main`, one concern per PR, tests, a `CHANGELOG.md` entry for user-visible changes, and DCO sign-off (`git commit -s`).

## Pull requests

1. Fork the repo and create a topic branch from `main`.
2. Reference the relevant story ID in the branch and commits when one exists. Story IDs (`DR-`, `WEG-`, `AILAB-`, `BZR-` — see [`STORY_IDS.md`](./STORY_IDS.md)) are assigned by the dreamd team — external contributors don't need to assign one.
3. Keep PRs focused. One concern per PR. If you find yourself bundling, split it.
4. Update [`CHANGELOG.md`](./CHANGELOG.md) under `## [Unreleased]` for any user-visible change.
5. Update tests. New behavior gets new tests; bug fixes get regression tests.
6. Run `cargo fmt --all -- --check && cargo clippy --workspace --all-targets --all-features -- -D warnings && cargo test --all-features --workspace` locally before pushing (or `just lint` then `just test`).

### Commit messages

We follow a loose conventional-commits style. Prefixes we use: `feat:`, `fix:`, `docs:`, `chore:`, `refactor:`, `test:`, `perf:`, `build:`, `ci:`. Reference the story ID when applicable:

```
feat(api): add POST /api/v1/dream (DR-104)
fix(io): fdatasync before returning 201 from /learn (DR-211)
docs: clarify scoring formula derivation
```

### Developer Certificate of Origin (DCO)

This project uses the [DCO](https://developercertificate.org/) instead of a CLA. Every commit must be signed off:

```bash
git commit -s -m "feat: ..."
```

The `Signed-off-by:` trailer certifies that you wrote the code or have the right to contribute it under the project's license (Apache-2.0). PRs without sign-off will be asked to amend before merge.

## Load-bearing engineering decisions

Some decisions in the implementation are not negotiable without re-reading SPEC.md and the threat model — for example:

- All JSONL appends go through a single coordinator with `sync_data` before returning 201.
- Relevance is computed at query time, never indexed.
- The dream cycle uses a write-ahead log; destructive ops are restartable.
- The local API binds to a Unix domain socket with a peer-credential UID match on every request on Linux and macOS. On Windows it is loopback TCP with a bearer token; a non-loopback bind is refused without `--insecure`.

If your PR touches any of these areas, please call it out in the description and link the relevant section of [`ARCHITECTURE.md`](./ARCHITECTURE.md).

## Comments and docs in code

Prefer short invariant / "do NOT" comments over narrating what the next line does:

- Lead module `//!` with the constraint and the anti-pattern when one exists.
- Keep an explicit `SAFETY:` contract next to any `unsafe` (do not widen the scoped `detach_double_fork` exception without an equal contract).
- Ticket IDs are fine as provenance; do not describe shipped work as "upcoming", and never point `dreamd migrate` at the Tantivy index (index self-heals; migrate is episodic `RECORD_SCHEMA_VERSION` only).
- Avoid decorative `// ---` / `// ===` section banners — blank lines or a one-line section title are enough.
- Do not conflate episodic `RECORD_SCHEMA_VERSION`, daemon `STATE_SCHEMA_VERSION`, and `index::SCHEMA_VERSION`.

## Documentation

When your change affects behavior users or contributors see, update the matching doc:

| You changed… | Update… |
|---|---|
| HTTP API, headers, status codes | [`docs/http-api.md`](./docs/http-api.md) |
| Config keys, env vars | [`docs/configuration.md`](./docs/configuration.md) |
| CLI flags or subcommands | Run `scripts/generate-man.sh` (regenerates `docs/dreamd.1`) and review the `cli_help` snapshots; update [`GUIDE.md`](./GUIDE.md) if workflow changes. A new top-level subcommand must also be added to `DREAMD_SUBCOMMANDS` in `packages/dreamd-mcp/bin/dreamd-mcp.js`, or `npx -y dreamd-mcp <subcommand>` silently starts the MCP server instead |
| MCP tools or adapter setup | [`SKILL.md`](./SKILL.md), relevant `adapters/*/README.md` |
| On-disk schema or dream cycle | [`SPEC.md`](./SPEC.md) (via RFC for breaking changes) |
| Engineering invariants | [`ARCHITECTURE.md`](./ARCHITECTURE.md) |
| CI jobs or local repro | [`docs/ci.md`](./docs/ci.md) |
| User-visible release notes | [`CHANGELOG.md`](./CHANGELOG.md) under `## [Unreleased]` |

Add new top-level docs to [`docs/README.md`](./docs/README.md). Story ID legend: [`STORY_IDS.md`](./STORY_IDS.md).

## Snapshot tests (insta)

Reviewing snapshot diffs requires the [`cargo-insta`](https://github.com/mitsuhiko/insta) CLI. Install it once:

```bash
cargo install cargo-insta
```

### CLI snapshots (help text)

```bash
cargo test -p dreamd --test cli_help
cargo insta review
```

Snapshot files live in `crates/dreamd-cli/tests/snapshots/`. Any change to `Command` enum variants, flags, or `clap` metadata will update these snapshots — review via `cargo insta review` before accepting.

### dreamd-core snapshots (dream-cycle output)

```bash
cargo test -p dreamd-core --test dream_cycle_snapshot
cargo insta review
```

Snapshot files live in `crates/dreamd-core/tests/snapshots/`. Any change to `dream_cycle::run_filesystem_phases` (what the test drives), `run_deterministic_dream_cycle`, `run_cluster_engine`, `write_lessons_file`, or `wal::commit_cycle` may produce a snapshot diff — review it via `cargo insta review` before accepting. The fixture corpus at `tests/fixtures/dream-cycle-snapshot/` is frozen; do not modify it without updating the spec and snapshots together.

## License

By contributing, you agree that your contributions will be licensed under the [Apache License 2.0](./LICENSE).
