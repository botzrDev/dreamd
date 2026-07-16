#!/usr/bin/env bash
# ANTH-20 gate runner — sources keys from env file, runs bake-off.
set -euo pipefail
ENV_FILE="${1:-/tmp/anth20_bakeoff.env}"
if [[ ! -f "$ENV_FILE" ]]; then
  echo "missing env file: $ENV_FILE" >&2
  exit 1
fi
set -a
# shellcheck source=/dev/null
source "$ENV_FILE"
set +a
cd "$(dirname "$0")/../.."
exec python3 scripts/benchmark/state_drift_bench.py --bakeoff
