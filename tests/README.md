# Workspace integration test fixtures

Shared fixtures and golden files used across `dreamd-cli` and `dreamd-core` tests. Crate-specific integration tests live beside their crates.

## Layout

```
tests/
  fixtures/
    init*.golden.txt          # byte-exact stdout for `dreamd init` (cli tests)
    dream-cycle-snapshot/     # frozen JSONL corpus for insta snapshot tests
    demo-corpus/              # recall demo data + EXPECTED.md
```

## Fixture directories

| Path | Used by | Purpose |
|---|---|---|
| `fixtures/init.golden.txt` | `dreamd-cli/tests/init_golden.rs` | First `dreamd init` stdout |
| `fixtures/init.rerun.golden.txt` | same | Idempotent re-run |
| `fixtures/init.quiet.golden.txt` | same | `--quiet` mode |
| `fixtures/dream-cycle-snapshot/` | `dreamd-core/tests/dream_cycle_snapshot.rs` | Frozen episodic input — **do not edit** without updating snapshots + SPEC |
| `fixtures/demo-corpus/` | Manual / demo | Hand-authored recall corpus; see `demo-corpus/README.md` |

## Crate test READMEs

- [`crates/dreamd-cli/tests/README.md`](../crates/dreamd-cli/tests/README.md) — CLI snapshots, insta workflow
- [`crates/dreamd-core/tests/`](../crates/dreamd-core/tests/) — integration tests (see module comments)
- [`packages/dreamd-mcp/test/README.md`](../packages/dreamd-mcp/test/README.md) — npm shim routing tests

## Running

```bash
cargo test --workspace
cargo test -p dreamd --test init_golden
cargo test -p dreamd-core --test dream_cycle_snapshot
```

After intentional output changes: `cargo insta review`.
