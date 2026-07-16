#!/usr/bin/env bash
# promote_bench.sh — mark a report as the new canonical benchmark.
#
# Usage:
#   scripts/promote_bench.sh <report_file>
#
# Copies <report_file> to benchmark_<timestamp>.md in the same directory,
# then prints the new path. Future compare_bench.py runs will use it as the
# baseline.
set -euo pipefail

if [[ $# -lt 1 ]]; then
  echo "usage: scripts/promote_bench.sh <report_file>" >&2
  exit 1
fi

SRC="$1"
if [[ ! -f "$SRC" ]]; then
  echo "ERROR: $SRC not found" >&2
  exit 1
fi

DIR="$(dirname "$SRC")"
TS="$(date +%Y%m%d_%H%M%S)"
DEST="$DIR/benchmark_${TS}.md"

cp "$SRC" "$DEST"
echo "[promote] benchmark set: $DEST"
