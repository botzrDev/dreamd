# Contributing to dreamd

Thanks for your interest. `dreamd` is pre-release; the spec ([`SPEC.md`](./SPEC.md)) is stable-ish, the implementation is not. Expect churn.

## Code of Conduct

Participation in this project is governed by the [Code of Conduct](./CODE_OF_CONDUCT.md). By contributing you agree to uphold it.

## Reporting bugs and proposing features

- **Bug:** open an issue using the *Bug report* template. Include the version, OS, and a minimal reproduction.
- **Feature:** open an issue using the *Feature request* template.
- **Spec change:** open an issue using the *RFC* template, with the title prefixed `[RFC]`. Spec changes are decided by discussion on the issue.
- **Security vulnerability:** **do not** open a public issue. See [SECURITY.md](./SECURITY.md).

## Development setup

Requirements:

- Rust stable matching the `rust-version` field in [`Cargo.toml`](./Cargo.toml) (current MSRV).
- `just` (optional but recommended) — see [`Justfile`](./Justfile).

```bash
git clone https://github.com/botzrDev/dreamd.git
cd dreamd

# common tasks (with just):
just lint        # cargo fmt --check && cargo clippy -- -D warnings
just test        # cargo test
just build       # cargo build --release

# or directly with cargo:
cargo build
cargo test
cargo clippy -- -D warnings
cargo fmt --check
```

CI runs the same checks across Linux, macOS, and Windows. PRs must be green before merge.

## Pull requests

1. Fork the repo and create a topic branch from `main`.
2. Reference the relevant story ID in the branch and commits when one exists. Story IDs follow `DR-XXX` (see the agile plan; the backlog is currently kept locally and will be migrated to GitHub Issues before v0.1).
3. Keep PRs focused. One concern per PR. If you find yourself bundling, split it.
4. Update [`CHANGELOG.md`](./CHANGELOG.md) under `## [Unreleased]` for any user-visible change.
5. Update tests. New behavior gets new tests; bug fixes get regression tests.
6. Run `just lint && just test` locally before pushing.
7. Fill out the PR template.

### Commit messages

We follow a loose conventional-commits style. Prefixes we use: `feat:`, `fix:`, `docs:`, `chore:`, `refactor:`, `test:`, `perf:`, `build:`, `ci:`. Reference the story ID when applicable:

```
feat(api): add POST /api/v1/dream (DR-104)
fix(io): fdatasync before returning 201 from /learn (DR-211)
docs: clarify salience formula derivation
```

### Developer Certificate of Origin (DCO)

This project uses the [DCO](https://developercertificate.org/) instead of a CLA. Every commit must be signed off:

```bash
git commit -s -m "feat: ..."
```

The `Signed-off-by:` trailer certifies that you wrote the code or have the right to contribute it under the project's license (Apache-2.0). PRs without sign-off will be asked to amend before merge.

## Load-bearing engineering decisions

Some decisions in the implementation are not negotiable without re-reading the PRD and the threat model — for example:

- All JSONL appends go through a single coordinator with `sync_data` before returning 201.
- Salience is computed at query time, never indexed.
- The dream cycle uses a write-ahead log; destructive ops are restartable.
- The local API binds to a Unix socket with `SO_PEERCRED`-based auth on Unix, and to `127.0.0.1` with a bearer token on Windows.

If your PR touches any of these areas, please call it out in the description and link the relevant section of [`CLAUDE.md`](./CLAUDE.md) (or, post-v0.1, the architecture doc that supersedes it).

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
