#!/usr/bin/env bash
# Local workspace test coverage via cargo-llvm-cov.
#
# One-time install:
#   cargo install cargo-llvm-cov
#   rustup component add llvm-tools-preview
#
# Usage:
#   scripts/coverage.sh            # html + lcov, no browser
#   scripts/coverage.sh --open     # also open the HTML report
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

if ! command -v cargo-llvm-cov >/dev/null 2>&1; then
    echo "cargo-llvm-cov not installed. Run: cargo install cargo-llvm-cov" >&2
    exit 1
fi

OUT_DIR="target/coverage"
mkdir -p "$OUT_DIR"

# Test-helper bins under tests/bin/ are subprocess fixtures, not product code —
# exclude from the report. See memory: test-helper-bin-pattern.
IGNORE_RE='tests/bin/'

cargo llvm-cov clean --workspace
cargo llvm-cov --no-report --workspace --ignore-filename-regex "$IGNORE_RE"
cargo llvm-cov report --html --output-dir "$OUT_DIR/html" --ignore-filename-regex "$IGNORE_RE"
cargo llvm-cov report --lcov --output-path "$OUT_DIR/lcov.info" --ignore-filename-regex "$IGNORE_RE"
cargo llvm-cov report --summary-only --ignore-filename-regex "$IGNORE_RE"

echo
echo "HTML report:  $OUT_DIR/html/index.html"
echo "Lcov:         $OUT_DIR/lcov.info"

if [[ "${1:-}" == "--open" ]]; then
    cargo llvm-cov report --html --output-dir "$OUT_DIR/html" --ignore-filename-regex "$IGNORE_RE" --open
fi
