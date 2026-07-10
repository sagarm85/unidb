# Fair-fsync benchmark image.
#
# Purpose: run the unidb-vs-Postgres multi-model comparison on **Linux**, where
# both engines use the same `fsync()` durability primitive — eliminating the
# macOS asymmetry (unidb's Rust `sync_all` = F_FULLFSYNC vs Postgres default
# `fsync(2)` = cache-only). Built and run via docker/docker-compose.yml.
#
# Single-stage on the official Rust image on purpose: `scripts/multi_model_report.sh`
# shells out to `cargo` and `git`, so the runtime needs the toolchain + repo. The
# bench is pre-built at image-build time so `docker compose up` starts measuring
# immediately.
FROM rust:1-bookworm

# `time` = GNU /usr/bin/time (peak RSS via `-v`); postgresql-client = pg_isready
# for the readiness wait. buildpack-deps (rust image base) already has cc/gcc,
# which rusqlite's bundled SQLite needs to compile.
RUN apt-get update \
 && apt-get install -y --no-install-recommends time postgresql-client ca-certificates \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY . .

# Pre-compile the decompose bench for Linux (fresh; target/ is .dockerignore'd).
RUN cargo build --release --bench decompose \
 && chmod +x docker/entrypoint.sh scripts/*.sh

ENTRYPOINT ["docker/entrypoint.sh"]
