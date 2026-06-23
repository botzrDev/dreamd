#!/usr/bin/env bash
# Regenerate doc/dreamd.1 from clap definitions.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo run -p dreamd --bin generate_man --quiet
