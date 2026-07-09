#!/usr/bin/env bash
#
# pg_compare.sh — bring up Postgres, run the unidb-vs-Postgres baseline
# comparison (benches/decompose.rs, PG_URL-gated), tear it down, and report
# peak RSS. Implements the bring-up half of checkpoint B1 in
# docs/backlog/pg_baseline_comparison.md.
#
# The comparison itself reports TWO durability lenses side by side (never one
# alone): lens 1 = wal_sync_method=open_datasync (macOS Postgres default, not
# flush-to-platter), lens 2 = fsync_writethrough (F_FULLFSYNC, matching unidb's
# durable default). The bench flips the lens server-wide via ALTER SYSTEM +
# pg_reload_conf(), so this script only needs to hand it a superuser PG_URL.
#
# Modes:
#   (default)   NATIVE Postgres on macOS — the honest lens-2 environment
#               (initdb a throwaway cluster on a private Unix socket).
#   --docker    Docker Postgres — prints the VM-durability caveat (Docker on
#               macOS runs a Linux VM whose fsync-to-host-platter semantics are
#               unquantifiable and flattering to Postgres).
#   $PG_URL set — reuse that server as-is; no bring-up, no teardown.
#
# Env knobs:
#   PG_URL     reuse an existing (superuser) server; skips bring-up/teardown.
#   N          largest size-sweep row count (default 1000000).
#   PG_IMAGE   image for --docker mode (default postgres:18).
#   KEEP       KEEP=1 leaves the native datadir / Docker container in place.
#
# Usage:
#   scripts/pg_compare.sh
#   scripts/pg_compare.sh --docker
#   PG_URL='postgres://postgres:pw@localhost:5432/unidb_bench' scripts/pg_compare.sh

set -euo pipefail

MODE="native"
[[ "${1:-}" == "--docker" ]] && MODE="docker"

N="${N:-1000000}"
PG_IMAGE="${PG_IMAGE:-postgres:18}"
KEEP="${KEEP:-}"
FILTER='pg_'                       # criterion filter: only the pg_* groups
export PG_SWEEP_SIZES="${PG_SWEEP_SIZES:-10000,100000,${N}}"

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

NATIVE_DATADIR=""
NATIVE_SOCKDIR=""
DOCKER_NAME="unidb_pgbench"
BROUGHT_UP=""                      # "native" | "docker" | "" (reused $PG_URL)

teardown() {
  [[ -n "$KEEP" ]] && { echo "[pg_compare] KEEP set — leaving Postgres up"; return; }
  case "$BROUGHT_UP" in
    native)
      echo "[pg_compare] stopping native Postgres, removing datadir"
      pg_ctl -D "$NATIVE_DATADIR" stop -m fast >/dev/null 2>&1 || true
      rm -rf "$NATIVE_DATADIR" "$NATIVE_SOCKDIR" ;;
    docker)
      echo "[pg_compare] removing Docker container $DOCKER_NAME"
      docker rm -f "$DOCKER_NAME" >/dev/null 2>&1 || true ;;
  esac
}
trap teardown EXIT

bringup_native() {
  command -v initdb >/dev/null || { echo "ERROR: initdb not found (install Postgres, e.g. brew install postgresql)"; exit 1; }
  NATIVE_DATADIR="$(mktemp -d /tmp/unidb_pgdata.XXXXXX)"
  NATIVE_SOCKDIR="$(mktemp -d /tmp/unidb_pgsock.XXXXXX)"
  local port=5439
  echo "[pg_compare] NATIVE bring-up (initdb → $NATIVE_DATADIR, socket $NATIVE_SOCKDIR:$port)"
  initdb -D "$NATIVE_DATADIR" -U postgres --auth=trust >/dev/null
  # Unix-socket only (no TCP): the local-socket path the spec asks for, and it
  # avoids colliding with any Postgres already on a TCP port.
  pg_ctl -D "$NATIVE_DATADIR" -w -o "-k $NATIVE_SOCKDIR -p $port -c listen_addresses=''" start >/dev/null
  createdb -h "$NATIVE_SOCKDIR" -p "$port" -U postgres unidb_bench
  export PG_URL="host=$NATIVE_SOCKDIR port=$port user=postgres dbname=unidb_bench"
  BROUGHT_UP="native"
  echo "[pg_compare] native Postgres up (superuser, local Unix socket)"
}

bringup_docker() {
  command -v docker >/dev/null || { echo "ERROR: docker not found"; exit 1; }
  cat <<'EOF'
[pg_compare] ============================ CAVEAT ============================
[pg_compare] Docker on macOS runs Postgres inside a Linux VM. fsync against the
[pg_compare] VM's virtual disk does NOT guarantee a flush to the host's physical
[pg_compare] platter, so lens 2 (fsync_writethrough) durability is UNQUANTIFIABLE
[pg_compare] here and FLATTERING to Postgres. For honest lens-2 numbers use the
[pg_compare] native mode (default). A native Linux host gives both engines
[pg_compare] uniform fsync semantics and yields the publishable numbers.
[pg_compare] ================================================================
EOF
  echo "[pg_compare] DOCKER bring-up ($PG_IMAGE)"
  docker rm -f "$DOCKER_NAME" >/dev/null 2>&1 || true
  docker run -d --name "$DOCKER_NAME" \
    -e POSTGRES_PASSWORD=bench -e POSTGRES_DB=unidb_bench \
    -p 5544:5432 "$PG_IMAGE" >/dev/null
  echo -n "[pg_compare] waiting for Postgres to accept connections"
  until docker exec "$DOCKER_NAME" pg_isready -U postgres >/dev/null 2>&1; do
    echo -n "."; sleep 1
  done
  echo " ready"
  export PG_URL="postgres://postgres:bench@localhost:5544/unidb_bench"
  BROUGHT_UP="docker"
}

if [[ -n "${PG_URL:-}" ]]; then
  echo "[pg_compare] PG_URL preset — reusing existing server (no bring-up/teardown)"
elif [[ "$MODE" == "docker" ]]; then
  bringup_docker
else
  bringup_native
fi

echo "[pg_compare] PG_URL=$PG_URL"
echo "[pg_compare] PG_SWEEP_SIZES=$PG_SWEEP_SIZES"

# Build the bench binary, then run it directly under /usr/bin/time -l so the
# reported "maximum resident set size" is the bench process's peak RSS (engine +
# pg client), not cargo's.
cargo bench --bench decompose --no-run >/dev/null 2>&1
BIN="$(find target/release/deps -maxdepth 1 -name 'decompose-*' -type f -perm -111 2>/dev/null | xargs -r ls -t | head -1)"
if [[ -z "$BIN" ]]; then
  echo "ERROR: could not locate compiled decompose bench binary"; exit 1
fi
echo "[pg_compare] running: $BIN --bench $FILTER"
echo "[pg_compare] ================= comparison output ================="
if command -v /usr/bin/time >/dev/null; then
  /usr/bin/time -l "$BIN" --bench "$FILTER" 2>&1 | tee /tmp/unidb_pgbench_out.txt
  echo "[pg_compare] ---- peak RSS ----"
  grep -E "maximum resident set size" /tmp/unidb_pgbench_out.txt || echo "(peak RSS line not found)"
else
  "$BIN" --bench "$FILTER"
fi
echo "[pg_compare] done."
