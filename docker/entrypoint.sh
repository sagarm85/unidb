#!/usr/bin/env bash
#
# Container entrypoint for the fair-fsync bench (see docker/Dockerfile).
# Waits for Postgres, then runs the canonical multi-model report inside Linux so
# unidb and Postgres share identical fsync() semantics. Writes the dated report
# to /out (bind-mounted to docker/out on the host).
set -euo pipefail

PG_HOST="${PG_HOST:-postgres}"
PG_PORT="${PG_PORT:-5432}"
PG_USER="${PG_USER:-postgres}"

echo "[entrypoint] waiting for Postgres at ${PG_HOST}:${PG_PORT} …" >&2
for _ in $(seq 1 60); do
  if pg_isready -h "$PG_HOST" -p "$PG_PORT" -U "$PG_USER" >/dev/null 2>&1; then
    echo "[entrypoint] Postgres ready." >&2
    break
  fi
  sleep 1
done

mkdir -p /out
OUT="/out/multi_model_report_$(date +%Y%m%d_%H%M%S).md"

# multi_model_report.sh reads PG_URL / MM_SIZES / MM_SAMPLE / GIT_COMMIT from the
# environment (all set by docker-compose) and captures peak RSS via GNU time -v.
scripts/multi_model_report.sh "$OUT"

echo "[entrypoint] report written to $OUT (host: docker/out/)" >&2
