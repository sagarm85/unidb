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
#   • No Docker       → native run on this host. A throwaway Postgres cluster is
#                       automatically started via initdb/pg_ctl (Homebrew Postgres)
#                       if PG_URL is not already set — so the Postgres comparison
#                       columns appear in every native report by default, no manual
#                       setup needed. The cluster is torn down on exit.
#                       On macOS, unidb commits via F_FULLFSYNC while Postgres uses
#                       fsync; the report notes this durability asymmetry.
#
# EVERY report additionally gets a **concurrency correctness matrix** appended —
# a pass/fail table of production-shaped concurrent read/write border cases
# (cross-row UPDATE churn = the item-16 anomaly shape, readers-during-writes at
# RC/RR/SERIALIZABLE, same-row contention, mixed CRUD, transfer sum invariance,
# vacuum interleaving, delete+reinsert slot reuse), swept across the
# UNIDB_CONCURRENT_SQL_WRITES toggle and indexed/unindexed tables, under CPU
# contention with repeats. It runs natively on the host in BOTH modes: it checks
# correctness invariants, not fsync-fair timing, so the docker asymmetry caveat
# does not apply to it.
#
# Usage:
#   scripts/report.sh                            # auto (Docker if available; native with auto-PG otherwise)
#   MM_SIZES=100000,1000000 scripts/report.sh    # sweep to millions
#   scripts/report.sh --docker                   # force Docker (fail if absent)
#   scripts/report.sh --native                   # force native with auto-PG
#   PG_URL=<conn> scripts/report.sh --native     # reuse an existing Postgres server
#   scripts/report.sh --conc                     # concurrency matrix ONLY (fast)
#   CONC_REPEATS=6 scripts/report.sh --conc      # tighten the intermittency net
#   CONC_SKIP=1 scripts/report.sh                # skip the matrix (perf-only run)
#
# Concurrency-matrix knobs (all optional): CONC_REPEATS (default 3), CONC_SPIN
# (contention spinner threads, default = cores), CONC_ROUNDS (workload
# multiplier), CONC_ONLY (scenario substring filter), CONC_STRICT=1 (nonzero
# exit if any cell fails — for CI).
#
# Output:
#   • Docker mode → docker/out/multi_model_report_<timestamp>.md
#   • Native mode → docs/performance/multi_model_report_<timestamp>.md
#   • --conc      → docs/performance/conc_matrix_<timestamp>.md
#
# Everything else (docker/compose, multi_model_report.sh, the decompose bench,
# the conc_matrix bench) is machinery this script drives — you only ever run
# THIS one.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

# Wall-clock start of this whole run (perf report + concurrency matrix) — see
# fmt_duration below; surfaced both on stderr as it runs and appended to the
# generated report itself, so "is this hung or just slow" always has a real
# answer (item 50, docs/backlog/50_patch_many_infinite_loop.md, is exactly a
# case where it used to be neither — a genuine hang with zero signal either
# way).
START_EPOCH="$(date +%s)"
fmt_duration() {
  local s="$1" m r
  m=$(( s / 60 )); r=$(( s % 60 ))
  if [[ "$m" -gt 0 ]]; then printf '%dm %ds' "$m" "$r"; else printf '%ds' "$r"; fi
}

MODE="auto"
case "${1:-}" in
  --native) MODE="native"; shift ;;
  --docker) MODE="docker"; shift ;;
  --conc)   MODE="conc";   shift ;;
  -h|--help) sed -n '2,47p' "$0"; exit 0 ;;
esac

docker_ok() { command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1; }

# ---------------------------------------------------------------------------
# Auto-managed throwaway Postgres for the native path.
# If PG_URL is unset when we enter native mode we spin up a local cluster with
# initdb (Homebrew Postgres), export PG_URL for the bench, and tear it down on
# exit.  This makes Postgres columns appear in every native report by default —
# the user never has to set PG_URL manually.
# ---------------------------------------------------------------------------
_PG_NATIVE_DATADIR=""
_PG_NATIVE_SOCKDIR=""

_teardown_pg() {
  [[ -z "$_PG_NATIVE_DATADIR" ]] && return
  echo "[report] stopping throwaway Postgres…" >&2
  pg_ctl -D "$_PG_NATIVE_DATADIR" stop -m fast >/dev/null 2>&1 || true
  rm -rf "$_PG_NATIVE_DATADIR" "$_PG_NATIVE_SOCKDIR"
  _PG_NATIVE_DATADIR=""
}

bringup_pg_native() {
  if ! command -v initdb >/dev/null 2>&1 || ! command -v pg_ctl >/dev/null 2>&1; then
    echo "[report] NOTE: initdb/pg_ctl not found — Postgres column will be skipped." >&2
    echo "[report]       Install Postgres (e.g. brew install postgresql) to enable it." >&2
    return
  fi
  _PG_NATIVE_DATADIR="$(mktemp -d /tmp/unidb_pgdata.XXXXXX)"
  _PG_NATIVE_SOCKDIR="$(mktemp -d /tmp/unidb_pgsock.XXXXXX)"
  local port=5439
  echo "[report] auto-starting throwaway native Postgres (port $port, Unix socket)…" >&2
  initdb -D "$_PG_NATIVE_DATADIR" -U postgres --auth=trust >/dev/null 2>&1
  pg_ctl -D "$_PG_NATIVE_DATADIR" -w \
    -o "-k $_PG_NATIVE_SOCKDIR -p $port -c listen_addresses=''" \
    start >/dev/null 2>&1
  createdb -h "$_PG_NATIVE_SOCKDIR" -p "$port" -U postgres unidb_bench >/dev/null 2>&1
  export PG_URL="host=$_PG_NATIVE_SOCKDIR port=$port user=postgres dbname=unidb_bench"
  echo "[report] throwaway Postgres ready — PG_URL set automatically." >&2
}

# Build + run the concurrency correctness matrix, appending its markdown
# section to the report file given as $1 (creating it with a small header if it
# does not exist yet — the --conc-only path).
run_conc_matrix() {
  local out="$1"
  local conc_start_epoch
  conc_start_epoch="$(date +%s)"
  echo "[report] building concurrency matrix (release)…" >&2
  cargo build --release --bench conc_matrix >/dev/null 2>&1
  local bin
  bin="$(ls -t target/release/deps/conc_matrix-* 2>/dev/null | grep -v '\.d$' | head -1)"
  if [[ -z "${bin:-}" || ! -x "$bin" ]]; then
    echo "[report] ERROR: conc_matrix bench binary not found" >&2
    return 1
  fi
  if [[ ! -f "$out" ]]; then
    mkdir -p "$(dirname "$out")"
    {
      echo "# Concurrency correctness report"
      echo
      echo "_Generated by \`scripts/report.sh --conc\` — border-case concurrent"
      echo "read/write correctness only (no throughput tables)._"
      echo
      echo "| | |"
      echo "|---|---|"
      echo "| Date | $(date '+%Y-%m-%d %H:%M:%S %Z') |"
      echo "| Commit | \`$(git rev-parse --short HEAD 2>/dev/null || echo '?')\` (branch \`$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo '?')\`) |"
      echo "| Machine | $(sysctl -n machdep.cpu.brand_string 2>/dev/null || uname -m) · $(getconf _NPROCESSORS_ONLN 2>/dev/null || echo '?') cores · $(uname -sr) |"
      echo "| Build | \`--release\`, group-commit |"
      echo
    } >"$out"
  fi
  echo "[report] running concurrency correctness matrix (repeats=${CONC_REPEATS:-3},"    >&2
  echo "[report] spinners=${CONC_SPIN:-auto}) — every cell is a pass/fail invariant…" >&2
  local status=0
  { echo; echo "---"; echo; } >>"$out"
  # CONC_STRICT=1 propagates a failing exit; otherwise never abort the report.
  "$bin" >>"$out" 2>/dev/null || status=$?
  if [[ "$status" -ne 0 ]]; then
    if [[ "${CONC_STRICT:-}" == "1" ]]; then
      echo "[report] concurrency matrix FAILED (CONC_STRICT=1) — see $out" >&2
      return "$status"
    fi
    echo "[report] NOTE: concurrency matrix recorded failures (see table in report)." >&2
  fi
  local conc_elapsed conc_taken
  conc_elapsed="$(( $(date +%s) - conc_start_epoch ))"
  conc_taken="$(fmt_duration "$conc_elapsed")"
  {
    echo
    echo "_Concurrency matrix generation time: $conc_taken (build + repeats=${CONC_REPEATS:-3} ×"
    echo "the scenario cells listed above, deliberately CPU-saturating by design)._"
  } >>"$out"
  echo "[report] concurrency matrix appended to $out (took $conc_taken)" >&2
}

# ── concurrency-matrix-only fast path ────────────────────────────────────────
if [[ "$MODE" == "conc" ]]; then
  OUT="docs/performance/conc_matrix_$(date +%Y%m%d_%H%M%S).md"
  run_conc_matrix "$OUT"
  echo "$OUT"
  exit 0
fi

use_docker=false
if [[ "$MODE" == "docker" ]]; then
  docker_ok || { echo "FATAL: --docker requested but the Docker daemon isn't running." >&2; exit 2; }
  use_docker=true
elif [[ "$MODE" == "auto" ]] && docker_ok; then
  use_docker=true
fi

# Register the PG teardown so it always fires on exit, even on error.
trap '_teardown_pg' EXIT

# Generate the base perf report; capture its path (both wrapped scripts print
# the report path as their final stdout line).
if $use_docker; then
  echo "[report] mode: DOCKER — fair-fsync comparison on Linux (recommended)." >&2
  REPORT="$("$REPO_ROOT/scripts/docker_report.sh" "$@" | tail -1)"
else
  echo "[report] mode: NATIVE ($(uname -sr)). Tip: start Docker for the fair-fsync comparison." >&2
  if [[ -z "${PG_URL:-}" ]]; then
    # Auto-spin up a throwaway native Postgres so every native report includes
    # the Postgres comparison columns without the user needing to set PG_URL.
    bringup_pg_native
  fi
  OUT="docs/performance/report_$(date +%Y%m%d_%H%M%S).md"
  REPORT="$("$REPO_ROOT/scripts/multi_model_report.sh" "$OUT" | tail -1)"
fi

if [[ -z "${REPORT:-}" || ! -f "$REPORT" ]]; then
  echo "[report] FATAL: no report file was produced." >&2
  exit 1
fi

# ── append the concurrency correctness matrix (native, both modes) ──────────
if [[ "${CONC_SKIP:-}" == "1" ]]; then
  echo "[report] CONC_SKIP=1 — concurrency matrix skipped." >&2
else
  run_conc_matrix "$REPORT"
fi

TOTAL_ELAPSED="$(( $(date +%s) - START_EPOCH ))"
TOTAL_TAKEN="$(fmt_duration "$TOTAL_ELAPSED")"
{
  echo
  echo "---"
  echo
  echo "**Total report generation time (this run): $TOTAL_TAKEN** — from"
  echo "\`scripts/report.sh\` invocation to this line, including every build step,"
  echo "the perf tables above, and the concurrency matrix."
} >>"$REPORT"

echo "[report] report: $REPORT (total: $TOTAL_TAKEN)" >&2

# Compare against latest benchmark and print delta (informational only).
python3 "$REPO_ROOT/scripts/compare_bench.py" "$REPORT" || true

echo "$REPORT"
