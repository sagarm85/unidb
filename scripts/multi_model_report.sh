#!/usr/bin/env bash
#
# multi_model_report.sh — generate a self-contained "multi-model at scale" report.
#
# One command, no other docs needed. Builds the bench in release, runs the W0→W4
# decomposition ladder pre-grown to a sweep of table sizes (does the ~1.2× tax
# hold at scale?), optionally adds the single-model unidb-vs-Postgres comparison,
# captures peak RSS, and writes a dated markdown report you can read on its own.
#
# Usage:
#   scripts/multi_model_report.sh                 # default sizes (1k,10k,100k)
#   MM_SIZES=100000,1000000,5000000 scripts/multi_model_report.sh   # to millions
#   PG_URL="host=/tmp port=5432 user=$USER dbname=postgres" scripts/multi_model_report.sh
#   scripts/multi_model_report.sh path/to/out.md  # explicit output path
#
# Env knobs (all optional):
#   MM_SIZES    comma list of pre-grow row counts   (default 1000,10000,100000)
#   MM_SAMPLE   marginal-commit sample per point     (default 200)
#   PG_URL      superuser Postgres conn string       (default: Postgres table skipped)
#   MM_REPLACED_STACK=1  Table 4 adds the §6 replaced-stack column (row + pgvector
#               + graph + queue, four independent commits) + crash-consistency
#               verdict. Needs a pgvector-enabled Postgres (CREATE EXTENSION vector).
#   MM_FK_ORDERS  order count for Table 5's PK/FK relational-integrity stress
#               (default 20000) — a customers/orders schema with a real
#               REFERENCES constraint, row-level-enforced on both engines.
#   MM_REPLACED_STACK_REALISTIC=1  Table 4.1 (item 61): TRUE replaced-stack —
#               Postgres (row + pgvector + graph adjacency) as three separate
#               autocommit connections + Redpanda (separate process, real TCP
#               round-trip) as the event queue leg. Requires REDPANDA_ADDR and
#               a running Redpanda instance (see docker/docker-compose.yml).
#               Without this flag only the conservative PG-only proxy runs.
#   REDPANDA_ADDR  Redpanda/Kafka bootstrap address for Table 4.1
#               (default localhost:9092)
#
# ── Table-selection knobs (speed up per-item bench runs) ──────────────────────
#   MM_TABLES=1,2,3   allowlist — run ONLY these tables (empty = all tables).
#                     Tables 1+2 are one measurement (the W0→W4 ladder): listing
#                     either runs both. Table 3.1 is gated together with 3.
#                     Example: MM_TABLES=3 runs only Table 3 + 3.1 CRUD stress.
#   MM_SKIP_LADDER=1  skip Tables 1+2 (the W0→W4 ladder). Its W2–W4 pre-grows
#                     build the HNSW + graph indexes synchronously — the single
#                     biggest time sink of a full run (~2.5 h at default sizes).
#   MM_SKIP_TABLE4=1  skip Table 4 + 4.1 (HNSW at-scale, ~45 min at 100k rows).
#                     Use when the change does not touch vector/HNSW code.
#   MM_SKIP_TABLE5=1  skip Table 5 (FK relational-integrity stress, ~5–10 min).
#                     Use when the change does not touch FK enforcement code.
#
# Skipped tables print a `_Skipped:` marker under their heading. To fill them
# from a previous full run instead of leaving holes, pass MM_BASELINE to
# scripts/report.sh — it stitches those tables in from the baseline report with
# an explicit provenance stamp (never confusable with a fresh measurement).
#
# Per-item quick profiles (combine MM_SIZES=100000 for fastest single-table run):
#   WAL / commit / B-tree / CRUD items (~30–45 min; add MM_BASELINE to fill the
#   skipped tables from the last full report, provenance-stamped):
#       MM_SKIP_LADDER=1 MM_SKIP_TABLE4=1 MM_SKIP_TABLE5=1 scripts/report.sh --docker
#   Vector / HNSW items:
#       scripts/report.sh --docker                     (run everything — Tables 1/2/4 are the signal)
#   Group-commit item (101) — concurrent INSERT only:
#       MM_SKIP_LADDER=1 MM_SKIP_TABLE4=1 MM_SKIP_TABLE5=1 MM_SIZES=100000 scripts/report.sh --docker
#   Full historical baseline comparison (per major release / shared-layer change):
#       scripts/report.sh --docker                     (no flags — run everything)
#   UNIDB_BUFFER_POOL_PAGES  frames for every unidb engine THIS BENCH opens
#               (default 2,000,000 -- set internally by bench_engine_open() in
#               decompose.rs, not the library's own smaller default). Only
#               raise this further for sweeps well beyond the default sizes
#               below; the built-in default already covers them with
#               headroom (item 42 -- without it, large MM_SIZES/MM_BULK_SIZES
#               points silently hit BufferPoolFull and understate unidb's
#               real throughput, not a correctness issue but a misleading one).
#
# NOTE: W2–W4 build the vector (HNSW) and graph indexes synchronously, so large
# MM_SIZES are slow by design — that cost is the whole point of the measurement.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

# Wall-clock start, covers everything below (build + the bench run itself) —
# reported in the header table as "Time taken" so a reader waiting on this
# script has a real number instead of guessing whether it's hung (see item
# 50, docs/backlog/50_patch_many_infinite_loop.md, for why that guess used to
# matter: this exact phase could, before that fix, hang indefinitely instead
# of just taking a while).
START_EPOCH="$(date +%s)"

OUT="${1:-docs/performance/report_$(date +%Y%m%d).md}"
mkdir -p "$(dirname "$OUT")"

echo "[multi_model_report] building release bench…" >&2
cargo build --release --bench decompose >/dev/null 2>&1

BIN="$(ls -t target/release/deps/decompose-* 2>/dev/null | grep -v '\.d$' | head -1)"
if [[ -z "${BIN:-}" || ! -x "$BIN" ]]; then
  echo "[multi_model_report] ERROR: could not find the built decompose bench binary" >&2
  exit 1
fi

# Environment header facts (best-effort; blank if a tool is missing).
DATE="$(date '+%Y-%m-%d %H:%M:%S %Z')"
GIT_COMMIT="${GIT_COMMIT:-$(git rev-parse --short HEAD 2>/dev/null || echo '?')}"
GIT_BRANCH="${GIT_BRANCH:-$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo '?')}"
CPU="$(sysctl -n machdep.cpu.brand_string 2>/dev/null || uname -m)"
NCPU="$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo '?')"
OS="$(uname -sr)"
# unidb's commit sync primitive on THIS platform: macOS resolves File::sync_all
# to F_FULLFSYNC (flush-to-platter); Linux (incl. the Docker image) uses plain
# fsync. Detect it so the header matches where the bench actually ran.
case "$(uname -s)" in
  Darwin) SYNC_PRIM="F_FULLFSYNC" ;;
  *)      SYNC_PRIM="fsync" ;;
esac
SIZES_SHOWN="${MM_SIZES:-1000,10000,100000}"
SAMPLE_SHOWN="${MM_SAMPLE:-200}"
PG_SHOWN="$([[ -n "${PG_URL:-}" ]] && echo 'set (Postgres column measured)' || echo 'unset (Postgres column skipped)')"
REALISTIC_SHOWN="$([[ "${MM_REPLACED_STACK_REALISTIC:-}" == "1" ]] && echo 'set (Table 4.1 realistic stack measured)' || echo 'unset (Table 4.1 skipped)')"
LADDER_SHOWN="$([[ "${MM_SKIP_LADDER:-}" == "1" ]] && echo 'SKIPPED (MM_SKIP_LADDER=1)' || echo 'measured')"
TABLE4_SHOWN="$([[ "${MM_SKIP_TABLE4:-}" == "1" ]] && echo 'SKIPPED (MM_SKIP_TABLE4=1)' || ( [[ -n "${MM_TABLES:-}" ]] && echo "${MM_TABLES}" | grep -q '4' && echo "included (MM_TABLES=${MM_TABLES})" || echo 'measured' ))"
TABLE5_SHOWN="$([[ "${MM_SKIP_TABLE5:-}" == "1" ]] && echo 'SKIPPED (MM_SKIP_TABLE5=1)' || echo 'measured')"
TABLES_SHOWN="$([[ -n "${MM_TABLES:-}" ]] && echo "allowlist: ${MM_TABLES}" || echo 'all')"

BODY="$(mktemp)"; TIMEFILE="$(mktemp)"
trap 'rm -f "$BODY" "$TIMEFILE"' EXIT

echo "[multi_model_report] running ladder (sizes=$SIZES_SHOWN, sample=$SAMPLE_SHOWN)…" >&2
# Peak RSS is captured out-of-band. macOS BSD `time -l` reports it in BYTES;
# GNU `time -v` (Linux, incl. the Docker image) reports "Maximum resident set
# size (kbytes)". Try each; fall back to a plain run (RSS n/a) if neither works.
RSS_BYTES=""
if /usr/bin/time -l true >/dev/null 2>&1; then
  UNIDB_BENCH=mmreport /usr/bin/time -l "$BIN" >"$BODY" 2>"$TIMEFILE" || true
  RSS_BYTES="$(awk '/maximum resident set size/{print $1; exit}' "$TIMEFILE")"
elif /usr/bin/time -v true >/dev/null 2>&1; then
  UNIDB_BENCH=mmreport /usr/bin/time -v "$BIN" >"$BODY" 2>"$TIMEFILE" || true
  RSS_KB="$(awk -F': ' '/Maximum resident set size/{print $2; exit}' "$TIMEFILE")"
  [[ -n "${RSS_KB:-}" ]] && RSS_BYTES=$(( RSS_KB * 1024 ))
else
  UNIDB_BENCH=mmreport "$BIN" >"$BODY" 2>"$TIMEFILE" || true
fi
if [[ -n "${RSS_BYTES:-}" ]]; then
  RSS_MIB="$(awk -v b="$RSS_BYTES" 'BEGIN{printf "%.0f MiB", b/1048576}')"
else
  RSS_MIB="n/a"
fi

ELAPSED_SECS="$(( $(date +%s) - START_EPOCH ))"
TIME_TAKEN="$(awk -v s="$ELAPSED_SECS" 'BEGIN{
  m=int(s/60); r=s%60;
  if (m>0) printf "%dm %ds", m, r; else printf "%ds", r
}')"

{
  echo "# Multi-model at-scale report"
  echo
  echo "_Generated by \`scripts/multi_model_report.sh\` — self-contained; no other docs needed._"
  echo
  echo "| | |"
  echo "|---|---|"
  echo "| Date | $DATE |"
  echo "| Commit | \`$GIT_COMMIT\` (branch \`$GIT_BRANCH\`) |"
  echo "| Machine | $CPU · $NCPU cores · $OS |"
  echo "| Build | \`--release\`, group-commit (one \`$SYNC_PRIM\` per commit) |"
  echo "| Sizes swept | $SIZES_SHOWN |"
  echo "| Marginal sample | $SAMPLE_SHOWN commits/point |"
  echo "| Postgres | $PG_SHOWN |"
  echo "| Realistic stack (Table 4.1) | $REALISTIC_SHOWN |"
  echo "| Tables run | $TABLES_SHOWN |"
  echo "| Tables 1+2 (W0→W4 ladder) | $LADDER_SHOWN |"
  echo "| Table 4 + 4.1 (HNSW) | $TABLE4_SHOWN |"
  echo "| Table 5 (FK stress) | $TABLE5_SHOWN |"
  echo "| Peak RSS | $RSS_MIB |"
  echo "| Time taken | $TIME_TAKEN (build + selected tables; excludes the concurrency matrix appended below, timed separately) |"
  echo
  echo "---"
  echo
  cat "$BODY"
} >"$OUT"

echo "[multi_model_report] wrote $OUT (took $TIME_TAKEN)" >&2
echo "$OUT"
