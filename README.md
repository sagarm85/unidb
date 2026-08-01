# unidb

**One embedded database. Four data models. One atomic commit.**

unidb is a Rust library that stores relational rows, vector embeddings, graph edges, and event streams in a single file — sharing one WAL, one transaction manager, and one crash-recovery path. Writing a user record, its embedding, a relationship edge, and an audit event is a single `commit()` call, not four network round-trips across four systems.

---

## Why unidb

Modern AI applications routinely need to:
- Save a row to Postgres
- Store an embedding in Qdrant or Pinecone
- Write a graph edge to Neo4j
- Publish an audit event to Kafka

Each step is a separate network call with no shared transaction. A crash between step 2 and step 3 leaves a partially-written record. Recovery requires a distributed saga, an outbox pattern, or idempotency logic in every service.

unidb replaces that entire stack with a single embedded engine:

| Before | After |
|--------|-------|
| Postgres + Qdrant + Neo4j + Kafka | unidb |
| 4 network round-trips per write | 1 local commit |
| No shared transaction | Full ACID across all four models |
| Distributed saga for crash recovery | 0 orphans — WAL is the event stream |
| 4 operational surfaces to run | 1 file |

---

## Four models, one commit

```rust
use unidb::Engine;

let engine = Engine::open("mydb.unidb")?;

// All four writes in one atomic transaction
let txn = engine.begin()?;

// Relational row
engine.execute_sql(txn,
    "INSERT INTO users (id, name) VALUES (1, 'Alice')"
)?;

// Vector embedding (HNSW-indexed, searchable via NEAR)
engine.execute_sql(txn,
    "UPDATE users SET embedding = [0.1, 0.2, 0.9, ...] WHERE id = 1"
)?;

// Graph edge
engine.execute_sql(txn,
    "INSERT INTO edges (from_id, to_id, edge_type) VALUES (1, 2, 'follows')"
)?;

// Event automatically captured and published to the WAL-derived queue on commit
engine.commit(txn)?;  // one fsync — all four models — atomic
```

```rust
// Vector similarity search
let results = engine.execute_sql(txn,
    "SELECT id, name FROM users NEAR embedding TO [0.1, 0.2, 0.9, ...] LIMIT 10"
)?;

// Graph traversal
engine.execute_cypher(txn,
    "MATCH (a)-[:follows]->(b) WHERE a.id = 1 RETURN b.id"
)?;
```

---

## Measured performance

Benchmarks run on Docker Linux aarch64 (ARM), release build, `fsync` durability matched between unidb and Postgres.

### Single-model CRUD vs Postgres (Docker Linux, 2026-07-21 consolidated run)

> Note: unidb is an embedded general-purpose engine competing against a specialized server. Losses on single-model CRUD are expected and documented honestly here. Source: [`docs/performance/report_20260721_035629.md`](docs/performance/report_20260721_035629.md) (cross-run ratios are environment-sensitive — see that folder's README).

| Operation | unidb | Postgres | Ratio |
|-----------|-------|----------|-------|
| SELECT COUNT(*) | 2.0B rec/s | 48.6M rec/s | **unidb 41.3×** (O(1) statistics fast path) |
| DELETE all | 22.5M rec/s | 5.2M rec/s | **unidb 4.29×** |
| DELETE selected | 6.9M rec/s | 3.4M rec/s | **unidb 2.01×** |
| SELECT GROUP BY | 26.2M rec/s | 20.4M rec/s | **unidb 1.29×** |
| UPDATE HOT-eligible | 942k rec/s | 889k rec/s | **unidb 1.06×** |
| UPDATE non-HOT | 633k rec/s | 978k rec/s | postgres 1.5× |
| INSERT per-row | 4,128 rec/s | 8,783 rec/s | postgres 2.1× (fsync floor) |
| SELECT filtered 5% | 2.7M rec/s | 6.0M rec/s | postgres 2.2× (parallel index scan) |

The INSERT and filtered-SELECT gaps are structural: per-row INSERT hits the fsync floor (one `fsync` per commit is durability, not a bug), and Postgres's parallel index scan infrastructure outpaces unidb's current B-tree path on selective queries. These are known and tracked (a warm-path page cache landed 2026-07-22 — item 109 — measuring 3.0× faster warm filtered SELECTs; the one-shot cold number above is the honest official record).

### Multi-model commit cost (Docker Linux, 2026-07-21 run)

| rows | Plain INSERT (W0) | Full four-model commit (W4) |
|-----:|------------------:|----------------------------:|
| 10k | 0.44 ms | 7.66 ms |
| 100k | 0.23 ms | 21.79 ms |

Synchronous HNSW insert dominated the multi-model path in this run — the cost of maintaining a navigable small-world graph on each write. As of 2026-07-22 (item 107) HNSW maintenance runs **asynchronously in a background worker** on served engines (bounded lag, queue-depth gauge); the first official benchmark of the collapsed W4/W0 ladder is queued.

### Bulk insert throughput

| Dataset size | Throughput |
|--------------|------------|
| 10k rows | 30k rows/sec |
| 1M rows | 18k rows/sec |

### Crash consistency

| Scenario | unidb | 4-system stack |
|----------|-------|----------------|
| Crash mid multi-model write | 0 orphans | Torn record |

Proven by the crash-injection test suite (`tests/crash/`).

### Concurrency correctness matrix

32/32 pass under Read Committed, Repeatable Read, Serializable, concurrent writers, vacuum churn, FK races, and unique-constraint races.

---

## Use cases

unidb is the right choice when:

- **You build AI applications** that save both structured metadata and vector embeddings and need them to stay in sync. One transaction, no dual-write bugs.
- **You build recommendation systems** where user preferences, similarity vectors, and social graph edges must be atomically consistent.
- **You need an audit/event pipeline without Kafka.** The WAL is the event stream. Subscribe over SSE, replay from any offset, or replicate to a downstream system — all from the same commit.
- **You want SQLite-class simplicity** (one file, no server required) but also need vector search, graph traversal, and a durable event queue.
- **You want to ship fast** without standing up and operating four separate systems.

unidb is not the right choice when:
- You only need single-model CRUD at scale — use Postgres or SQLite.
- You need distributed consensus across multiple nodes — unidb is single-primary.
- You need the widest SQL compatibility — unidb covers a practical subset, not full ANSI SQL.

---

## Quick start

```toml
# Cargo.toml
[dependencies]
unidb = { path = "." }  # or version once published on crates.io
```

```rust
use unidb::Engine;

// Open or create a database file
let engine = Engine::open("mydb.unidb")?;

// Create a table
let txn = engine.begin()?;
engine.execute_sql(txn,
    "CREATE TABLE users (id INT PRIMARY KEY, name TEXT, embedding VECTOR(128))"
)?;
engine.commit(txn)?;

// Insert a row
let txn = engine.begin()?;
engine.execute_sql(txn,
    "INSERT INTO users (id, name) VALUES (1, 'Alice')"
)?;
engine.commit(txn)?;

// Query
let txn = engine.begin()?;
let rows = engine.execute_sql(txn, "SELECT id, name FROM users WHERE id = 1")?;
engine.commit(txn)?;

// Structured logging
unidb::init_tracing();
```

Isolation levels available: `READ COMMITTED` (default), `REPEATABLE READ`, `SERIALIZABLE` (SSI, write-skew prevention).

---

## REST server

unidb ships an optional HTTP server (`server` feature) — useful when you want to share one database across multiple processes or languages.

```bash
UNIDB_JWT_SECRET=dev-secret \
UNIDB_DATA_DIR=/var/lib/unidb \
UNIDB_BIND_ADDR=127.0.0.1:8080 \
cargo run --bin unidb-server --features server
```

```bash
# Generate a JWT and run a query
TOKEN=$(UNIDB_JWT_SECRET=dev-secret ./scripts/gen_jwt.sh)
curl -H "Authorization: Bearer $TOKEN" -X POST http://127.0.0.1:8080/sql \
  -d '{"sql":"SELECT COUNT(*) FROM users"}'

# Prometheus metrics (no auth)
curl http://127.0.0.1:8080/metrics

# Subscribe to live events over SSE
curl -H "Authorization: Bearer $TOKEN" \
  "http://127.0.0.1:8080/events/subscribe?table=users"

# Register a database webhook (item 141): POST the CDC envelope to an
# operator endpoint on every insert to `orders`, superuser-only
curl -H "Authorization: Bearer $TOKEN" -X POST http://127.0.0.1:8080/webhooks \
  -d '{"id":"orders-hook","target_url":"https://example.com/hooks/orders","table_pattern":"orders","events":["insert"],"signing_secret":"shh"}'

# Auth admin API (item 142): ban a user (superuser-only) — subsequent
# logins/refreshes get 403 USER_BANNED and every session is revoked
curl -H "Authorization: Bearer $TOKEN" -X PATCH http://127.0.0.1:8080/auth/admin/users/alice \
  -d '{"banned":true}'

# Scheduled jobs / cron (item 144): run SQL nightly at 3am server-local
# time, superuser-only — pg_cron parity, no storage-engine change
curl -H "Authorization: Bearer $TOKEN" -X POST http://127.0.0.1:8080/cron/jobs \
  -d '{"name":"nightly-cleanup","schedule":"0 3 * * *","sql":"DELETE FROM sessions WHERE expires_at < now()"}'
```

Key environment variables:

| Variable | Default | Purpose |
|----------|---------|---------|
| `UNIDB_JWT_SECRET` | required | HMAC secret for HS256 JWT verification |
| `UNIDB_JWT_SIGNING_KEY` | unset | Item 121 A5: enables token issuance (`/auth/login`/`signup`/`refresh`) as a first-class production capability, independent of `UNIDB_DEV_LOGIN` |
| `UNIDB_JWT_PUBLIC_KEY` | unset | Item 121 A6: PEM public key (RSA or EC/P-256, auto-detected) — switches verification to asymmetric RS256/ES256 and disables local issuance; published at `GET /.well-known/jwks.json` |
| `UNIDB_DATA_DIR` | `/tmp/unidb` | Storage directory |
| `UNIDB_BIND_ADDR` | `127.0.0.1:8080` | Listen address |
| `UNIDB_TXN_IDLE_TIMEOUT_SECS` | `60` | Timeout for idle HTTP transaction sessions |
| `UNIDB_SLOW_QUERY_MS` | unset | Log queries slower than this threshold |

Full route reference: [`docs/REST_API.md`](docs/REST_API.md)

**Auto-generated data APIs**, both derived from the live catalog and enforcing
the same RLS/grant engine as `/sql` (no separate authorization surface):
- `GET/POST/PATCH/DELETE /rest/v1/<table>` — a PostgREST-style resource API
  (filter operators, `select=`/embedded FK expansion, `order=`/`limit`/`offset`)
  plus a generated `GET /rest/v1` OpenAPI 3 document. `Prefer` header response
  controls (item 139): `count=exact` -> `Content-Range` (RLS-scoped, exact,
  zero extra cost when unused); `return=representation|minimal` on
  `POST`/`PATCH`/`DELETE`.
- `POST /graphql` — a schema-derived GraphQL API (`pg_graphql` analog) that,
  unlike a relational-only equivalent, also exposes **graph edge traversal**
  and **vector similarity** as first-class fields alongside FK relationships.
  Read **and write**: a `Query` root plus a `Mutation` root
  (`insert_<t>`/`update_<t>`/`delete_<t>` per table, item 133) — every
  mutation resolves through the same enforced SQL path as `/rest/v1`/`/sql`,
  so RLS/`WITH CHECK`/column grants apply identically.

---

## Schema migrations

unidb ships a Supabase-style, forward-only SQL migration tool: plain `.sql` files applied in ascending version order, tracked in a `schema_migrations` table the tool creates and maintains itself.

```bash
mkdir -p migrations
cat > migrations/0001_init.sql <<'EOF'
CREATE TABLE users (id INT PRIMARY KEY, email TEXT NOT NULL);
EOF

UNIDB_DATA_DIR=/var/lib/unidb cargo run --bin unidb-migrate -- migrations
# unidb-migrate: applying migrations from 'migrations'...
# unidb-migrate: applied 1, skipped 0 (0001)
```

- **File naming:** `<version>_<name>.sql`, sorted **lexicographically** — zero-padded sequence numbers (`0001_init.sql`) or a sortable timestamp prefix both work.
- **Idempotent:** re-running skips already-applied versions (checksum-verified).
- **Drift-checked:** editing an already-applied migration's file is rejected (checksum mismatch) rather than silently re-applied.
- **Stops on first error**, leaving the failing migration unrecorded; earlier migrations in the run stay applied.
- Also available as a Rust API: `Engine::apply_migrations(dir: &Path) -> Result<MigrationReport>`.
- **Read [`docs/SCHEMA_MIGRATIONS.md`](docs/SCHEMA_MIGRATIONS.md) before writing a migration** — it documents exactly which failures are (and are not) rolled back, since unidb's catalog DDL is not fully transactional across separate SQL calls.

---

## What's included

**Storage and transactions**
- Single-file page store with ARIES WAL (steal + no-force, full redo+undo recovery)
- MVCC: Read Committed, Repeatable Read, Serializable (SSI)
- Group commit — one `fsync` per transaction
- Auto-checkpoint and autovacuum (background, Postgres-style policy)
- Full-page writes + CRC32 checksums on every page
- Crash-injection harness (54 crash/recovery tests as of 2026-07-22)

**SQL and relational**
- SQL subset: SELECT (with joins, aggregates, GROUP BY, HAVING, ORDER BY, LIMIT), INSERT, UPDATE, DELETE, CREATE/ALTER/DROP TABLE, TRUNCATE, RETURNING
- Window functions (ROW_NUMBER/RANK/DENSE_RANK/LAG/LEAD/SUM/AVG/COUNT/MIN/MAX OVER) and set operations (UNION/INTERSECT/EXCEPT)
- Column types: INT, BIGINT, FLOAT, TEXT, BOOL, DECIMAL, TIMESTAMP, DATE, UUID, BYTEA, JSON, VECTOR(n)
- Constraints: PRIMARY KEY, FOREIGN KEY, UNIQUE, NOT NULL, CHECK, DEFAULT, SERIAL
- B-tree secondary indexes (durable, crash-recovered, no rebuild on open); covering indexes via `CREATE INDEX … INCLUDE (cols)`; index-only scans
- Cost-based optimizer with ANALYZE statistics and EXPLAIN / EXPLAIN ANALYZE; LRU plan cache
- Joins: hash join (with grace spill-to-disk), sort-merge, index-nested-loop; INNER/LEFT/RIGHT/FULL OUTER/NATURAL, `ON` and `USING`
- Subqueries, CTEs, derived tables, prepared statements with `$n` bind parameters
- Text matching: `LIKE`/`ILIKE`, and full-text `MATCH` over an inverted index
- Row-level security (RLS) with per-op policies and `current_user`
- `information_schema` introspection (rows filtered by the caller's table grants)

**Vector search**
- `VECTOR(n)` column type, up to arbitrary dimensions
- Durable on-disk HNSW index (M=16, ef_construction=200)
- `NEAR ... TO ... LIMIT` operator — recall@10 ≥ 0.95 at 1k–10k vectors
- No index rebuild on open

**Graph**
- Edge records: `(from_id, to_id, edge_type, props JSON)`
- Durable edge-adjacency B-tree index
- Cypher subset for traversal queries
- Full ACID — edges are rows, committed with everything else

**Event queue and CDC**
- WAL-derived durable event stream per table
- Consumer offsets, replay from any point
- `SUBSCRIBE` SSE push with `Last-Event-ID` resume
- Before/after row images on every change
- Debezium and Supabase format adapters
- Per-consumer lag observability via `/stats` and Prometheus gauges
- Realtime Broadcast + Presence (item 132) — ephemeral, in-memory SSE
  pub/sub (`/realtime/broadcast/*`) and per-topic presence tracking
  (`/realtime/presence/*`), Supabase-Realtime-parity on top of the same DB
  change stream above; never touches the WAL/heap (server restart drops it,
  by design)
- Realtime channel authorization (item 140) — opt-in, role-based, topic-glob
  allow/deny policies in front of all four routes above (superuser-managed
  via `/realtime/policies`), with an audited `service_role`/superuser
  bypass and a fail-closed `UNIDB_REALTIME_REQUIRE_AUTHZ` enforce mode
- Database webhooks (item 141) — superuser-registered outbound HTTP POST on
  `INSERT`/`UPDATE`/`DELETE` (`/webhooks`), delivering the CDC envelope with
  an `X-Unidb-Signature: sha256=<HMAC>` header; bounded exponential-backoff
  retry (a dead endpoint never wedges the stream or blocks another webhook),
  at-least-once, background delivery worker downstream of commit only
- Scheduled jobs / cron (item 144) — Supabase/`pg_cron` parity: superuser-
  registered SQL (`/cron/jobs`) run on a standard 5-field cron schedule,
  server-local time, optionally `run_as` a scoped role so RLS/grants apply;
  control-plane only, the scheduler is strictly a caller of the same
  `execute_sql` path every statement uses, no overlap/no backfill, per-job
  status + `unidb_cron_runs_total`/`unidb_cron_failures_total` metrics

**Operations and HA**
- Segmented WAL (16 MiB segments) enabling replication slots
- Read replicas with WAL streaming and `promote()` failover
- Online base backup + WAL archiving + point-in-time recovery (by timestamp or LSN)
- Users, roles, GRANT — per-table privileges with transitive role membership
- Password auth (argon2id), refresh-token sessions, TOTP MFA, OAuth 2.0
  social login (Google, GitHub, Apple, Microsoft/Azure AD, GitLab, Discord,
  Facebook — item 143 extended the original Google/GitHub preset table),
  and self-service password-reset + magic-link sign-in over a pluggable
  email transport (SMTP or a no-network dev-inbox log transport, item 138)
- Leaked-password protection (item 143) — opt-in (`UNIDB_PASSWORD_HIBP_CHECK=1`)
  HaveIBeenPwned "Pwned Passwords" k-anonymity check at every password-set
  point (signup, admin create/patch, password reset); fails open with a
  warning on an HIBP outage
- Auth admin API (item 142) — consolidated, superuser-only user management
  (`/auth/admin/users`: paginated list/get/create/update/delete) plus
  per-user **ban** (rejected at login/refresh/email-verify with `403
  USER_BANNED`, revokes sessions) and split `app_metadata`/`user_metadata`
  — Supabase's `auth.admin`, built entirely on existing user/credential/
  session machinery
- Native TLS (rustls), audit log
- Prometheus `/metrics` endpoint, slow-query log, per-chokepoint latency histograms
- Object storage service (`unidb-storage`) — metadata in unidb tables, bytes tiered to S3/MinIO

**Client options**
- Embed directly as a Rust library (no `tokio` in the engine core)
- Optional REST server (tokio-based, JWT auth, SSE)
- `unidb-attach` — Rust blocking client over the REST API

---

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│  Your application (embedded Rust)  ·  REST server (optional) │
└────────────────────────────┬────────────────────────────────┘
                             │ Engine::execute_sql / execute_cypher / NEAR
┌────────────────────────────▼────────────────────────────────┐
│  Query layer   parser → logical plan → physical plan         │
│                cost-based optimizer · EXPLAIN                │
├─────────────────────────────────────────────────────────────┤
│  Four record kinds (one record-kind tag, one heap)           │
│  Relational rows · VECTOR(n) + HNSW · Graph edges · Events   │
├─────────────────────────────────────────────────────────────┤
│  Transaction manager — MVCC snapshots · lock manager         │
│  RC / RR / Serializable (SSI)                                │
├─────────────────────────────────────────────────────────────┤
│  Storage layer                                               │
│  Buffer pool (steal+no-force) · WAL (ARIES) · Page store     │
│  Control file · Crash recovery · Auto-checkpoint             │
└──────────────────────────────┬──────────────────────────────┘
                               │
                         mydb.unidb  (one file)
                         db.wal/     (WAL segments)
```

The engine core uses only `std::thread` — no `tokio` dependency for embedded use. The optional REST server and replication layer add `tokio`.

---

## Benchmarks

Detailed benchmark tables, workload descriptions, and historical comparisons are in [`docs/performance/`](docs/performance/) (and linked from the commit history for each release).

To run locally:

```bash
# Release build required
cargo build --release

# Full benchmark suite
cargo bench

# Postgres comparison (requires PG_URL env var pointing at a running instance)
PG_URL=postgres://localhost/unidb_bench ./scripts/pg_compare.sh

# Quick HTTP smoke test against a running server
UNIDB_JWT_SECRET=dev-secret ./scripts/bench_server.sh
```

---

## Building and testing

```bash
# Prerequisites: Rust stable toolchain
rustup update stable

# Debug build
cargo build

# Release build (use for benchmarks)
cargo build --release

# Full test suite
cargo test

# Crash-injection harness
cargo test --test crash

# Server tests (requires --features server)
cargo test --features server

# Lint (zero warnings required)
cargo clippy -- -D warnings

# Format
cargo fmt --all
```

For structured log output during tests:
```bash
RUST_LOG=debug cargo test -- --nocapture
```

---

## Project layout

```
src/
  lib.rs              Engine public API, init_tracing()
  heap.rs             MVCC-versioned row storage
  btree_index.rs      Durable on-disk B+tree (WAL-logged, crash-recovered)
  hnsw_index.rs       Durable on-disk HNSW vector index
  wal.rs              Segmented append-only WAL (ARIES redo+undo)
  bufferpool.rs       Buffer pool with WAL-before-page enforcement
  mvcc.rs             Snapshot visibility (RC / RR)
  txn.rs              Transaction manager
  lockmgr.rs          Lock manager with deadlock detection
  sql/                Parser, planner, optimizer, executor
  graph/              Edge records, Cypher subset
  queue/              Event capture, poll, consumer offsets
  replication/        WAL shipping, read replicas, failover
  backup/             Base backup, WAL archiving, PITR
  authz/              Users, roles, GRANT
  migrations.rs       SQL schema-migrations tooling (Engine::apply_migrations)
  server/             Optional REST/JWT/SSE/metrics (feature = "server")
  bin/
    unidb-migrate.rs  Schema-migrations CLI
tests/
  crash/              Crash-injection harness (54 crash/recovery tests)
benches/              Throughput and latency benchmarks
scripts/
  pg_compare.sh       Postgres baseline comparison
  gen_jwt.sh          JWT generator (bash + openssl, no Python needed)
  bench_server.sh     HTTP performance smoke test
docs/
  REST_API.md         Full HTTP route reference
  SCHEMA_MIGRATIONS.md  Migrations tool: naming, algorithm, non-transactional-DDL caveat
  engine_access_guide.md  Application builder's guide
  ops_runbook.md      Production operations guide
unidb-attach/         Rust blocking REST client
unidb-dispatch/       Downstream event dispatcher (webhooks, rooms)
unidb-storage/        Object storage service (S3/MinIO tiering)
```

---

## License

MIT — see [LICENSE](LICENSE)
