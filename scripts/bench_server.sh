#!/usr/bin/env bash
# unidb-server performance smoke test — a plain-shell alternative to
# `cargo bench --bench server --features server` (benches/server.rs) for
# checking a *running* server (local or deployed) without a Rust toolchain.
# Uses only curl/openssl/awk — no extra dependencies, no PyJWT.
#
# This is a smoke test, not a substitute for benches/server.rs: it reports
# a rough p50/p99 and a concurrent-throughput number from real HTTP calls
# against a live server, useful for a quick "is this deployment healthy /
# in the right ballpark" check (e.g. in a deploy script or CI job), not for
# the rigorous, statistically-sampled numbers criterion produces.
#
# Usage:
#   UNIDB_JWT_SECRET=dev-secret ./scripts/bench_server.sh
#   BASE_URL=http://127.0.0.1:8080 REQUESTS=200 CONCURRENCY=10 \
#     UNIDB_JWT_SECRET=dev-secret ./scripts/bench_server.sh
#
# Env vars:
#   UNIDB_JWT_SECRET  required — must match the server's own secret
#   BASE_URL          default http://127.0.0.1:8080
#   REQUESTS          default 200 — total requests for each phase
#   CONCURRENCY       default 10 — parallel clients for the throughput phase

set -euo pipefail

BASE_URL="${BASE_URL:-http://127.0.0.1:8080}"
SECRET="${UNIDB_JWT_SECRET:?set UNIDB_JWT_SECRET to match the server configuration}"
REQUESTS="${REQUESTS:-200}"
CONCURRENCY="${CONCURRENCY:-10}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Verify-only JWT (HS256) matching src/server/auth.rs — signed locally with
# the shared secret, exactly like any real client of this API would.
# Shared with README's quick-start snippet via scripts/gen_jwt.sh (kept as
# one implementation, not duplicated).
TOKEN=$(UNIDB_JWT_SECRET="${SECRET}" "${SCRIPT_DIR}/gen_jwt.sh" bench 3600)
AUTH_HEADER="Authorization: Bearer ${TOKEN}"
TMP_LAT="$(mktemp)"
trap 'rm -f "$TMP_LAT"' EXIT

echo "== unidb-server performance smoke test =="
echo "target=${BASE_URL} requests=${REQUESTS} concurrency=${CONCURRENCY}"
echo

echo "-- setup: scratch table --"
curl -sf -o /dev/null -X POST "${BASE_URL}/sql" -H "${AUTH_HEADER}" \
    -H 'Content-Type: application/json' \
    -d '{"sql":"CREATE TABLE bench_scratch (id INT)"}' || true

echo "-- sequential /sql INSERT latency, n=${REQUESTS} --"
: >"${TMP_LAT}"
for i in $(seq 1 "${REQUESTS}"); do
    start=$(date +%s%N)
    curl -sf -o /dev/null -X POST "${BASE_URL}/sql" -H "${AUTH_HEADER}" \
        -H 'Content-Type: application/json' \
        -d "{\"sql\":\"INSERT INTO bench_scratch (id) VALUES (${i})\"}"
    end=$(date +%s%N)
    echo $(((end - start) / 1000000)) >>"${TMP_LAT}"
done
sort -n "${TMP_LAT}" -o "${TMP_LAT}"
n=$(wc -l <"${TMP_LAT}" | tr -d ' ')
p50_line=$((n * 50 / 100 + 1))
p99_line=$((n * 99 / 100 + 1))
[ "$p50_line" -gt "$n" ] && p50_line="$n"
[ "$p99_line" -gt "$n" ] && p99_line="$n"
p50=$(sed -n "${p50_line}p" "${TMP_LAT}")
p99=$(sed -n "${p99_line}p" "${TMP_LAT}")
avg=$(awk '{s+=$1} END {printf "%.1f", s/NR}' "${TMP_LAT}")
echo "p50=${p50}ms p99=${p99}ms avg=${avg}ms"
echo

echo "-- concurrent /sql throughput, ${CONCURRENCY} parallel clients, ${REQUESTS} total --"
start=$(date +%s%N)
seq 1 "${REQUESTS}" | xargs -P "${CONCURRENCY}" -I{} curl -sf -o /dev/null -X POST \
    "${BASE_URL}/sql" -H "${AUTH_HEADER}" -H 'Content-Type: application/json' \
    -d '{"sql":"INSERT INTO bench_scratch (id) VALUES (1)"}'
end=$(date +%s%N)
elapsed_ms=$(((end - start) / 1000000))
ops_per_sec=$(awk -v n="${REQUESTS}" -v ms="${elapsed_ms}" 'BEGIN { printf "%.1f", n * 1000 / ms }')
echo "elapsed=${elapsed_ms}ms throughput=${ops_per_sec} ops/s"
echo

echo "-- /metrics snapshot (no auth required) --"
curl -sf "${BASE_URL}/metrics" | grep -E 'axum_http_requests_total|unidb_' || echo "(no matching metric lines yet)"
