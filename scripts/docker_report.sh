#!/usr/bin/env bash
#
# docker_report.sh — one command to run the unidb-vs-Postgres multi-model
# comparison in Docker (Linux), where both engines use the same fsync()
# durability primitive. Removes the macOS F_FULLFSYNC-vs-fsync asymmetry so the
# unidb÷PG ratio is apples-to-apples by construction.
#
# Usage:
#   scripts/docker_report.sh                              # default sizes
#   MM_SIZES=100000,1000000 scripts/docker_report.sh      # push to millions
#   MM_REPLACED_STACK=1 scripts/docker_report.sh          # §6 headline column
#
# Output: docker/out/multi_model_report_<timestamp>.md
#
# Caveat: on Docker Desktop for macOS the containers share one Linux VM disk, so
# the *ratio* is fair but absolute flush-to-platter durability is VM-bound. For
# publishable absolute numbers, run this on a native Linux host.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if ! command -v docker >/dev/null; then
  echo "FATAL: docker not found." >&2
  exit 2
fi

# Pass the host's real git commit + branch into the container (its context
# excludes .git, so it cannot derive these itself — the header would show '?').
export GIT_COMMIT="$(git -C "$REPO_ROOT" rev-parse --short HEAD 2>/dev/null || echo '?')"
export GIT_BRANCH="$(git -C "$REPO_ROOT" rev-parse --abbrev-ref HEAD 2>/dev/null || echo '?')"
export MM_SIZES="${MM_SIZES:-1000,10000,100000}"
export MM_SAMPLE="${MM_SAMPLE:-200}"
# MM_REPLACED_STACK=1 → Table 4 adds the §6 replaced-stack column (row + pgvector
# + graph + queue as four independent commits) + a crash-consistency verdict.
# The compose file uses the pgvector image so the vector role can run.
export MM_REPLACED_STACK="${MM_REPLACED_STACK:-}"
# MM_REPLACED_STACK_REALISTIC=1 → Table 4.1 (item 61): the TRUE replaced-stack:
# Postgres (row + pgvector + graph adjacency, three separate autocommit
# connections) + Redpanda (separate Docker container, real TCP inter-process
# latency) as the event queue leg. Demonstrates the overhead the conservative
# PG-only proxy (MM_REPLACED_STACK=1) understates. Requires the redpanda
# service (always started in compose regardless of this flag).
export MM_REPLACED_STACK_REALISTIC="${MM_REPLACED_STACK_REALISTIC:-}"

mkdir -p "$REPO_ROOT/docker/out"
cd "$REPO_ROOT/docker"

echo "[docker_report] building image + running fair-fsync comparison on Linux…" >&2
echo "[docker_report] sizes=$MM_SIZES sample=$MM_SAMPLE commit=$GIT_COMMIT" >&2

# Count reports with `find` (does NOT fail on no-match, unlike a `*.md` glob
# under `set -o pipefail`, which would abort the script before compose runs).
count_reports() { find "$REPO_ROOT/docker/out" -maxdepth 1 -name '*.md' 2>/dev/null | wc -l | tr -d ' '; }

# Snapshot existing reports so we can tell if THIS run produced a new one.
BEFORE="$(count_reports)"

# Fresh phase/stats files for THIS run (the bench appends to phases.csv).
PHASES="$REPO_ROOT/docker/out/phases.csv"
STATS="$REPO_ROOT/docker/out/stats.csv"
rm -f "$PHASES"; : > "$STATS"

# Background CPU/mem sampler: poll `docker stats` on both containers (~1 s),
# tagging each sample with a host unix-ms timestamp, until the bench container
# has come up and then gone. The bench writes phase windows to phases.csv; the
# post-processor correlates the two into a per-phase resource table.
sample_stats() {
  local seen=0 guard=0
  while [[ $guard -lt 7200 ]]; do
    guard=$((guard + 1))
    if docker ps --format '{{.Names}}' 2>/dev/null | grep -q 'unidb-fair-bench-bench'; then
      seen=1
      ts="$(python3 -c 'import time;print(int(time.time()*1000))' 2>/dev/null || echo 0)"
      docker stats --no-stream --format '{{.Name}},{{.CPUPerc}},{{.MemUsage}}' 2>/dev/null \
        | grep 'unidb-fair-bench' | sed "s/^/${ts},/" >> "$STATS" || true
    elif [[ $seen -eq 1 ]]; then
      break
    fi
    sleep 1
  done
}
sample_stats & SAMPLER_PID=$!

# --abort-on-container-exit stops Postgres when the bench finishes;
# --exit-code-from bench propagates the bench's exit status. `|| true` so a
# container-level failure (e.g. Postgres refusing to start) still reaches the
# report-existence check below, which is the real success criterion.
docker compose up --build --abort-on-container-exit --exit-code-from bench || true

kill "$SAMPLER_PID" 2>/dev/null || true
wait "$SAMPLER_PID" 2>/dev/null || true

# Tear down containers (leave the report on the host).
docker compose down -v >/dev/null 2>&1 || true

AFTER="$(count_reports)"
LATEST="$(find "$REPO_ROOT/docker/out" -maxdepth 1 -name '*.md' 2>/dev/null | xargs ls -1t 2>/dev/null | head -1 || true)"
if [[ -n "$LATEST" && "$AFTER" -gt "$BEFORE" ]]; then
  # Append the per-phase CPU/memory table correlated from phases + stats.
  python3 "$REPO_ROOT/scripts/mm_resource_report.py" "$LATEST" "$PHASES" "$STATS" >&2 2>&1 || \
    echo "[docker_report] WARNING: CPU/mem correlation step failed (report still valid)" >&2
  echo "[docker_report] report: $LATEST" >&2
  echo "$LATEST"
else
  echo "[docker_report] FATAL: bench produced no new report — see the compose output" >&2
  echo "[docker_report]        above (common cause: Postgres failed to start)." >&2
  exit 1
fi
