# dreamd contributor entry points (WEG-33 / DR-005).
#
# Requires `just` (https://github.com/casey/just). CI does NOT use this file —
# it invokes cargo directly (see .github/workflows/ci.yml and docs/ci.md). These
# recipes mirror the CI gates, so a green `just lint` / `just test` locally means
# the same checks will be green in CI. `just` is a convenience, never a CI dep.

# Debug build of the whole workspace.
dev:
    cargo build --workspace

# Full test suite — mirrors the CI merge gate.
test:
    cargo test --all-features --workspace

# Recall-latency benchmarks (Criterion). Only dreamd-core carries benches.
bench:
    cargo bench -p dreamd-core

# Stripped release build of the `dreamd` CLI; prints its size (NFR-2 gate is CI-side).
release:
    #!/usr/bin/env bash
    set -euo pipefail
    # NFR-2: CI enforces a hard < 15 MB limit (soft warn at 12 MB). This recipe only
    # *reports* the size — the enforcing gate lives in .github/workflows/ci.yml, so a
    # large binary does not fail `just release` here.
    cargo build --release -p dreamd
    BINARY=target/release/dreamd
    strip "$BINARY"
    # Portable byte size: Linux uses `stat -c%s`, macOS uses `stat -f%z`.
    case "$(uname -s)" in
        Darwin) SIZE_BYTES=$(stat -f%z "$BINARY") ;;
        *)      SIZE_BYTES=$(stat -c%s "$BINARY") ;;
    esac
    SIZE_MB=$(awk "BEGIN { printf \"%.2f\", ${SIZE_BYTES} / 1024 / 1024 }")
    echo "Stripped ${BINARY}: ${SIZE_MB} MB (${SIZE_BYTES} bytes) — NFR-2 limit 15 MB"

# Lint exactly as CI does: formatting check, then clippy with warnings denied.
lint:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets --all-features -- -D warnings
