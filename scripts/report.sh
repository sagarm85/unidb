#!/usr/bin/env bash
#
# report.sh — THE one command to generate the full unidb multi-model benchmark
# report. Run this every time. The markdown it writes is self-contained (machine
# env, all measurement tables, durability caveats, and how to read them) — you do
# not need to open any other document to understand it.
#
# It auto-selects the environment so you don't have to think about it:
#   • Docker running  → fair-fsync comparison on Linux, where unidb AND Postgres
#                       both use plain fsync() (the honest apples-to-apples ratio).
#                       Recommended, and the default when Docker is available.
#   • No Docker       → native run on this host. Still valid, but on macOS unidb
#                       commits via F_FULLFSYNC while Postgres-default does not;
#                       the report states this so the numbers aren't misread.
#
# Usage:
#   scripts/report.sh                            # auto (Docker if available)
#   MM_SIZES=100000,1000000 scripts/report.sh    # sweep to millions
#   scripts/report.sh --docker                   # force Docker (fail if absent)
#   scripts/report.sh --native                   # force native (skip Docker)
#
# Output:
#   • Docker mode → docker/out/multi_model_report_<timestamp>.md
#   • Native mode → docs/performance/multi_model_report_<timestamp>.md
#
# Everything else (docker/compose, multi_model_report.sh, the decompose bench) is
# machinery this script drives — you only ever run THIS one.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

MODE="auto"
case "${1:-}" in
  --native) MODE="native"; shift ;;
  --docker) MODE="docker"; shift ;;
  -h|--help) sed -n '2,30p' "$0"; exit 0 ;;
esac

docker_ok() { command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1; }

use_docker=false
if [[ "$MODE" == "docker" ]]; then
  docker_ok || { echo "FATAL: --docker requested but the Docker daemon isn't running." >&2; exit 2; }
  use_docker=true
elif [[ "$MODE" == "auto" ]] && docker_ok; then
  use_docker=true
fi

if $use_docker; then
  echo "[report] mode: DOCKER — fair-fsync comparison on Linux (recommended)." >&2
  exec "$REPO_ROOT/scripts/docker_report.sh" "$@"
fi

echo "[report] mode: NATIVE ($(uname -sr)). Tip: start Docker for the fair-fsync comparison." >&2
if [[ -z "${PG_URL:-}" ]]; then
  echo "[report] NOTE: PG_URL unset — the Postgres column will be skipped. Set PG_URL" >&2
  echo "[report]       (superuser conn) or use Docker mode to include Postgres." >&2
fi
OUT="docs/performance/multi_model_report_$(date +%Y%m%d_%H%M%S).md"
"$REPO_ROOT/scripts/multi_model_report.sh" "$OUT"
