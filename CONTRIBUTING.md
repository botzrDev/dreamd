# Contributing to dreamd

Thanks for your interest. `dreamd` is pre-release; the spec ([`SPEC.md`](./SPEC.md)) is v0.1 (on-disk contract frozen), the implementation is not. Expect churn.

## Code of Conduct

Participation in this project is governed by the [Code of Conduct](./CODE_OF_CONDUCT.md). By contributing you agree to uphold it.

## Reporting bugs and proposing features

- **Bug:** open an issue using the *Bug report* template. Include the version, OS, and a minimal reproduction.
- **Feature:** open a regular issue describing the use case and the gap.
- **Spec change:** open an issue using the *RFC* template, with the title prefixed `[RFC]`. Spec changes are decided by discussion on the issue.
- **Security vulnerability:** **do not** open a public issue. See [SECURITY.md](./SECURITY.md).

## Development setup

Requirements:

- Rust stable. An MSRV will be pinned ahead of v0.1.

```bash
git clone https://github.com/botzrDev/dreamd.git
cd dreamd

cargo build
cargo test
cargo clippy -- -D warnings
cargo fmt --check
```

### Task runner (`just`)

Common loops are wrapped in a root [`Justfile`](./Justfile) for convenience. Install [`just`](https://github.com/casey/just) (`cargo install just`, or your package manager), then run `just <recipe>`:

| Recipe | Runs | Purpose |
|---|---|---|
| `just dev` | `cargo build --workspace` | Debug build of the whole workspace. |
| `just test` | `cargo test --all-features --workspace` | Full test suite (mirrors the CI merge gate). |
| `just lint` | `cargo fmt --all -- --check` then `cargo clippy --workspace --all-targets --all-features -- -D warnings` | Formatting + clippy, exactly as CI runs them. |
| `just bench` | `cargo bench -p dreamd-core` | Recall-latency Criterion benchmarks. |
| `just release` | `cargo build --release -p dreamd`, `strip`, print size | Stripped `dreamd` CLI; prints its size (NFR-2 hard limit `< 15 MB` is enforced in CI, not here). |

`just` is optional and never a CI dependency — CI calls cargo directly. Without `just`, run the underlying cargo commands from the table above.

Install the git hooks once per clone. The pre-commit hook runs `cargo fmt --all -- --check` on every commit; the pre-push hook runs clippy before code reaches the remote. This catches formatting and lint drift locally instead of turning CI red:

```bash
# Install the pre-commit hook (one-time, per clone)
git config core.hooksPath .githooks
```

CI runs the same checks across Linux, macOS, and Windows. PRs must be green before merge.

## Pull requests

1. Fork the repo and create a topic branch from `main`.
2. Reference the relevant story ID in the branch and commits when one exists. Story IDs follow `DR-XXX` and are assigned by the dreamd team — external contributors don't need to assign one.
3. Keep PRs focused. One concern per PR. If you find yourself bundling, split it.
4. Update [`CHANGELOG.md`](./CHANGELOG.md) under `## [Unreleased]` for any user-visible change.
5. Update tests. New behavior gets new tests; bug fixes get regression tests.
6. Run `cargo fmt --check && cargo clippy --all-targets --all-features -- -D warnings && cargo test` locally before pushing.

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
- The local API binds to a Unix socket with `SO_PEERCRED`-based auth on Unix/macOS at v0.1. Windows bearer-token auth is deferred to v0.1.1.

If your PR touches any of these areas, please call it out in the description and link the relevant section of [`ARCHITECTURE.md`](./ARCHITECTURE.md).

## Documentation

When your change affects behavior users or contributors see, update the matching doc:

| You changed… | Update… |
|---|---|
| HTTP API, headers, status codes | [`docs/http-api.md`](./docs/http-api.md) |
| Config keys, env vars | [`docs/configuration.md`](./docs/configuration.md) |
| CLI flags or subcommands | Run `scripts/generate-man.sh`; update [`GUIDE.md`](./GUIDE.md) if workflow changes |
| MCP tools or adapter setup | [`SKILL.md`](./SKILL.md), relevant `adapters/*/README.md` |
| On-disk schema or dream cycle | [`SPEC.md`](./SPEC.md) (via RFC for breaking changes) |
| Engineering invariants | [`ARCHITECTURE.md`](./ARCHITECTURE.md) |
| CI jobs or local repro | [`docs/ci.md`](./docs/ci.md) |
| User-visible release notes | [`CHANGELOG.md`](./CHANGELOG.md) under `## [Unreleased]` |

Add new top-level docs to [`docs/README.md`](./docs/README.md). Story ID legend: [`STORY_IDS.md`](./STORY_IDS.md).

## Snapshot tests (insta)

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

Snapshot files live in `crates/dreamd-core/tests/snapshots/`. Any change to `run_deterministic_dream_cycle`, `run_cluster_engine`, `write_lessons_file`, or `wal::commit_cycle` may produce a snapshot diff — review it via `cargo insta review` before accepting. The fixture corpus at `tests/fixtures/dream-cycle-snapshot/` is frozen; do not modify it without updating the spec and snapshots together.

## License

By contributing, you agree that your contributions will be licensed under the [Apache License 2.0](./LICENSE).
