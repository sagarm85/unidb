# unidb

A single embedded storage/transaction engine in Rust that unifies relational CRUD, vector search (durable on-disk IVF-Flat), graph edges, and a WAL-derived event queue over one page store, one WAL, one buffer pool, and one transaction manager.

The competitive edge is eliminating the multi-system dual-write tax. "Save row + embedding + graph edge + event" is one WAL append and one commit here, versus 3–4 network round-trips with no shared transaction across Postgres + a vector store + a graph DB + Kafka. **Measured (item 17, `benches/decompose.rs` Table 4, `MM_REPLACED_STACK=1`):** the same four-model write against a real **replaced stack** (Postgres row + pgvector + a graph table + an outbox queue, four independent commits) runs **3.61× faster in one atomic commit under real flush-to-platter durability** (unidb `F_FULLFSYNC` vs Postgres `fsync_writethrough` — 250 vs 69 txns/s; 1 sync vs 4). The throughput edge is *durability-cost-dependent* — it narrows to ~parity under a cheap/buffered VM `fsync` — but the **crash-consistency win is unconditional**: unidb recovers **0 orphans** where the four-system stack recovers a **torn record** (`tests/crash` `item16_*` proofs). Honest boundary per §1: we still expect to *lose* single-model CRUD vs a specialized incumbent — the win is the cross-domain atomic commit, not per-op speed.

**Status: M0–M11 shipped; hardening/ops phases 1–6 complete; **subscription CDC** (item 29) — `before`/`after` row images, canonical envelope, Debezium/Supabase format adapters on `GET /events/subscribe?format=`, and `unidb_catalog.subscription_lag` per-consumer lag observability (virtual relation + `/stats` + Prometheus gauges); a SQL-queryable system catalog (Milestone 18) — `information_schema.{tables,columns,table_constraints,key_column_usage,referential_constraints}` + `unidb_catalog.indexes` as synthesized virtual relations you `SELECT` over the normal query surface (no app-shaped REST endpoints), plus the Application Builder's Guide (`docs/engine_access_guide.md`); production-grade **observability metrics** (item 21) — lock-free per-chokepoint capture (per-statement-kind latency histograms, WAL-fsync cost, buffer-pool hit/miss/evict, lock-wait + deadlock counters, the alertable vacuum-horizon-age gauge, per-table page counts, worker-governance utilization) surfaced via `stats()`/`GET /stats` + Prometheus `/metrics`, with a widget-traceability table in the access guide; a **logs surface** (item 22) — JSON-lines server logs, a per-request `request_id`/`txn_id` correlation id joining the app log, slow-query log, and `audit.log`, and a superuser-gated, bounded, cursor-paged `GET /logs` reverse-read tail (cap + scan budget, never OOMs a multi-GB log dir); an **object storage service** (item 23) — the `unidb-storage` app-layer crate keeping bucket/object metadata in unidb tables and tiering bytes between engine LOBs (ACID-inline) and an S3-wire store (MinIO/S3) with presigned PUT/GET + an outbox/reconciler for crash-consistent uploads, adding no engine surface; **event queue at scale** (item 26) — a durable `DiskBTree` seq-index on `__events__` making `poll_events`/`poll_events_after` O(log n + returned) with flat latency proven 10k→300k, plus an `EventWake` condvar so `commit()` wakes subscribers and idle SSE streams use zero CPU; **time-based PITR + logical replication** (item 28) — `restore_to_time(target_ts_micros)` resolves wall-clock → LSN via a side `timeline.bin` mark file (per-commit resolution, no WAL format change; crash-safe); `unidb-logical` crate for table-subset logical replication (INSERT/UPDATE/DELETE from the item-26 event stream, at-least-once, offset-durable, survives primary restart); commit-time WAL fsync, autovacuum, a durable on-disk free-space map, and CRUD perf (Phase A write path — coalesced UPDATE index WAL, 8868 → 619 B/row, UPDATE-bulk 0.11× → 0.34× vs Postgres; Phase B read path — projection/qual decode pushdown, and `SELECT COUNT(*)` now **2.81× faster than Postgres** via a count-visible-slots fast path; Milestone P — **parallel scan workers** (`std::thread`, not tokio): unfiltered `SELECT COUNT(*)` **3.82×**, filtered `COUNT(*) WHERE …` **6.6×**, and filtered `SELECT … WHERE k …` **6.41×** faster in parallel, cutting Postgres's scan lead from +540% to +82%; **parallel scan is now default-on** with a global worker cap and timeout/cancellation-aware workers — item 15 governance; and **REST API enrichment (item 12)** — multi-request **transaction sessions** over HTTP (`X-Txn-Id`, per-session isolation, busy/principal/idle-reaper protection), one-shot isolation selection, RLS + events-vacuum + flush admin routes, atomic batch insert, and large-result cursors) landed.** Single-file storage core, MVCC transactions + SQL subset, vector/full-text search, a graph layer with a Cypher subset, a WAL-derived event queue, an optional REST/JWT/SSE/metrics server, a B-Tree secondary index, a CSR graph index, a Rust attach client (`unidb-attach`), group-commit + concurrent reads, semantic search (cosine metric + embedding CLI), SQL constraints, and heap vacuum/GC with a background **autovacuum** launcher are all implemented, tested, and benchmarked. The follow-on roadmap (`docs/backlog/roadmap.md`) is **complete**: Phase 1 (ACID hardening), Phase 2/4 (SQL types + query power), Phase 3 (durable multi-model storage), Phase 5 (concurrency — writers that scale with cores), and **Phase 6 (operations & HA — segmented WAL, replication slots + read replicas + failover, backups + PITR, users/roles/GRANT, TLS + audit, observability)**. See `PROGRESS.md` for milestone-by-milestone benchmark tables and `MEMORY.md` for current implementation state and known tech debt.

**Durability: group-committed force-log-at-commit (default).** Statement mini-transactions inside a user transaction append their WAL records without a per-statement fsync; `Engine::commit` forces the transaction's commit record durable via a group-coalesced `sync_up_to` — **one fsync per transaction** (ARIES force-log-at-commit; fulfills D1, D2/D5 unchanged). A commit is never acknowledged until its commit LSN is synced, so ACID durability is exact. Eviction that finds only not-yet-durable dirty pages forces a WAL sync rather than failing (safe under memory pressure), and WAL shipping is capped at the durable frontier so a replica can never get ahead of the primary on failover. Measured on the decomposition ladder (`benches/decompose.rs`): the full multi-model commit (row + B-tree + vector + edge + event) drops from ~33.1 ms/commit (old per-statement default) to **~4.4 ms/commit — ~7.5×** — with a plain-row commit at SQLite parity (~3.6 ms). Crash harness grew 21 → **25**; the valid-prefix recovery property test runs under both durability policies. See `docs/backlog/commit_time_fsync.md`.

**Operations & HA (Phase 6): deployable single primary + read replicas.** The WAL is a directory of fixed-size 16 MiB **segments** (seal + rotate; truncation deletes whole consumed segments), which is what makes **replication slots** and WAL shipping possible: a replica seeds from a **base backup** and applies the streamed WAL incrementally (`replication::Replica`), can be **promoted** on failover, and an optional **synchronous slot** avoids losing acknowledged commits. **Online base backups + WAL archiving** give point-in-time recovery (`backup::restore(..., target_lsn)`, PITR by LSN). Access control adds **users/roles/GRANT** (`authz`, per-table privileges, transitive role membership) with per-user JWT identity, plus a security **audit log** and native **TLS** (rustls). Observability adds a `pg_stat_*`-style `GET /stats`, a slow-query log, and an ops runbook (`docs/ops_runbook.md`). The crash harness grew 19 → **21** (P18 segmented WAL, P19 backup+PITR restore); the sync invariant still holds (no async runtime / TLS in the default embedded build). Encryption-at-rest is a documented, D9-sign-off-gated follow-up. See `docs/backlog/phase6_ops_ha.md` and `PROGRESS.md`'s Phase 6 entry.

**Concurrency (Phase 5): writers scale with cores.** `Engine` is now `Send + Sync`, so the server shares one `Arc<Engine>` across a pool of worker threads (a tokio blocking pool) instead of funneling every write through one dedicated writer thread. Every heap read-modify-write holds the page's exclusive latch (no lost updates), the transaction/lock manager runs real blocking wait queues with wait-for-graph deadlock detection, and durability uses **group commit**: the leader runs its `fsync` with the WAL append lock *released*, so concurrent committers coalesce behind a single fsync. Measured (`benches/concurrent_writers.rs`, 8 logical cores): **1→325, 2→330, 4→647 (1.99×), 8→1197 commits/s (3.68×)** — write throughput scales with concurrent writers instead of the flat single-writer ceiling. *Raw CRUD* scales; graph/large-object writes still serialize on a coarse write lock. **SQL writes now scale under concurrent writers too** (index-write-concurrency milestone): the `UNIDB_CONCURRENT_SQL_WRITES` toggle (**default-ON since the item-11 flip, 2026-07-13**; set it to `0`/`false`/`off` for the serialized fallback) lets catalog-non-mutating DML take a *shared* catalog lock, with `DiskBTree` index maintenance made race-safe by **latch-coupled ("crabbing") descent with safe-node early release** (structural-validator + `loom`-verified). Measured indexed 8-writer INSERT recovers from ~811 to ~1016 commits/s toward the ~1275 unindexed floor (re-measured on the flipped default; original ship 768 → 1058); toggle off reproduces the serialized path exactly. The 28-cell concurrency correctness matrix passes 28/28 at `CONC_REPEATS=10` with contention spinners, toggle on **and** off. A full Lehman-Yao B-link tree (overlapping same-subtree descents) is the remaining future work. Per-query **timeouts, cancellation, and `work_mem`** are available via `Engine::execute_sql_with_limits` (P5.f). Reads stay on the concurrent `ReadHandle` path (point reads + read-only `SELECT` run in parallel with writers via MVCC snapshots). The crash-injection harness stays green (19/19) under the concurrent model, and the sync invariant (no async runtime in the default embedded engine) holds. See `docs/backlog/phase5_concurrency.md`.

**Earlier performance (post-M8): group commit + read-only fsync skip.** Read-only transactions no longer pay a commit fsync (embedded point `SELECT` ~3.05 ms → ~1.09 µs). The buffer pool forces the WAL before stealing a not-yet-durable dirty page (ARIES no-force), so deferred (group-commit) mode is safe for working sets larger than the pool, removing the older `BufferPoolFull`-at-scale limitation. *(Update 2026-07-09: group-committed force-log-at-commit is now the durability default on every path — see "Durability" above — so the embedded path no longer pays a per-statement fsync either.)* See `docs/backlog/group_commit_and_read_concurrency.md`.

**M10: heap vacuum / MVCC garbage collection.** The engine physically reclaims space held by dead tuple versions via `Engine::vacuum() -> VacuumReport`. Vacuum computes a conservative visibility horizon over every live transaction **and** every live concurrent `ReadHandle` reader, marks reclaimable versions' line pointers DEAD via a crash-safe redo-only `WAL_VACUUM` record, scrubs those `RowId`s from every secondary index **before** any slot becomes reusable (the aliasing gate that keeps a reused slot from resolving a stale index entry to a live, wrong row), then compacts each page and frees the slots. Long-lived `REPEATABLE READ` transactions or readers hold the horizon back and are surfaced in `VacuumReport.horizon_blocked` rather than silently ignored.

**Autovacuum (A1–A4): auto-triggered background MVCC vacuum.** A background `std::thread` launcher (deliberately **not** tokio — the engine core stays synchronous) auto-triggers that same M10 vacuum so bloat is bounded without a human. It sleeps `naptime`, wakes, and when the Postgres-shape policy fires — `dead > threshold + scale_factor · live` over cheap global dead/live-tuple estimates (`AutoVacuumConfig`, env-configurable, default-on) — runs `Engine::vacuum`. It needs no new locking: `Engine` is `Send + Sync` (Phase 5), vacuum already takes `write_serial` + per-page latches (M10), and the horizon stays reader/replication-slot-correct — a background pass respects it unchanged, so a live RR reader still blocks reclamation. The worker holds a `Weak<Engine>` (no refcount cycle) and shuts down cleanly when the engine drops. Default-on for the served instance and via `Engine::open_arc`; a bare `Engine::open` handle stays thread-free (deterministic for tests; manual `vacuum()` always available). Stats via `Engine::stats()` / `GET /stats` / `/metrics`. Under sustained 200×30 churn, autovacuum keeps the heap at ~35 logical pages vs ~82 un-vacuumed (bounded vs unbounded). See `docs/backlog/autovacuum.md` and `PROGRESS.md`'s Autovacuum entry.

**Phase 1 (complete): ACID & storage foundation — the feature-freeze gate.** Before further scale/feature work, the engine closed its silent correctness holes (`docs/backlog/roadmap.md` §4, `docs/backlog/phase1_acid_hardening.md`). **P1.a — full-page-writes — is shipped:** an 8 KiB page write is not atomic, so a crash mid-write used to leave a torn page that CRC detects but cannot repair (the #1 data-loss hole). Now, on the first modification of a page after each checkpoint, the buffer pool logs the whole clean page image to the WAL (a redo-only `WAL_FPI` record); recovery replays it as the clean base before re-applying the interval's incremental redo on top, so a torn on-disk page is fully reconstructed. New crash point **P11** manufactures a real torn page and asserts recovery (`FORMAT_VERSION` 3→4). **P1.b — fsync-failure handling — is also shipped:** a failed `fsync`/`msync` may leave the OS having dropped the dirty data while clearing its dirty bit, so the WAL and buffer pool now treat a durability failure as fatal — they latch a poisoned state and never falsely report success (new crash point **P12**). **P1.c — the scaling foundation — is also shipped:** the page file now grows in 4 MiB chunks (was a whole-file remap on every `alloc_page` — O(N²), fatal at 100s of GB), the buffer pool is configurable (`UNIDB_BUFFER_POOL_PAGES`, default raised 256→4096 frames), and a real free-space map replaces the linear per-insert page scan — so insert/read throughput stays flat as a table grows (`benches/scale.rs`). **P1.d — isolation correctness — is also shipped:** write-write conflicts under `REPEATABLE READ`/`SERIALIZABLE` now surface as `SerializationFailure` (not a raw conflict), `READ COMMITTED` re-reads the latest committed version via its fresh per-statement snapshot (no spurious abort), and a new `SERIALIZABLE` level uses SSI rw-antidependency (pivot) detection to prevent write-skew. **P1.e — auto-checkpoint — is shipped, completing the phase:** the WAL used to grow unbounded (checkpoint was manual-only), so a checkpoint now fires automatically on a time or WAL-size trigger (at a quiescent point), keeping the WAL bounded (benchmarked ~8–23× smaller) at unchanged throughput. The crash-injection harness grew from 11 to **14** points and `FORMAT_VERSION` went 3→4; no locked decision was reversed. See `PROGRESS.md`'s Phase 1 entries.

**Phase 3 (complete): multi-model durable storage — the moat.** This phase kills the "rebuild every secondary index on open" tax (O(all data) startup, RAM-bound) by making the indexes durable on disk. **P3.a — the durable B-Tree — is shipped:** the B-Tree secondary index is now an on-disk B+tree (`DiskBTree`) whose nodes are pages in the shared page store, buffer-pool-managed, WAL-logged as full node-page images (a new redo-only `WAL_INDEX` record) and crash-recovered — so `Engine::open` reads it straight from a stable meta page instead of rescanning the heap to rebuild it. It moved off the async index worker onto the synchronous writer/read path, and vacuum scrubs it directly. New crash point **P13** wipes the entire data file and proves the whole tree is reconstructed from the WAL alone; the harness grew 14 → **15** and `FORMAT_VERSION` went 4→5. The open-cost benchmark (`benches/durable_index.rs`) shows B-Tree reopen time is flat as the table grows while a still-rebuilt HNSW index's reopen time rises. **P3.b is also shipped:** the **full-text (inverted)** and **edge-adjacency** indexes are now durable `DiskBTree`s too (reusing the same `WAL_INDEX` machinery, no new format version) — full-text keyed on tokens, the edge index keyed on `__edges__.from_id` — so neither is rebuilt on open; a new Rust-API `Engine::search_fulltext` gives the durable full-text index a real read path. The M7 **CSR index was retired** (it was consulted by no read path after the M7 traversal revert; adjacency is now served durably by the edge index), leaving the async worker serving only the vector index. Crash points **P14/P15** cover the two new durable indexes (harness 15 → **17**). **P3.c (on-disk vector) is shipped (spike + production):** a spike (`docs/design/p3c_vector_spike.md`) chose **on-disk IVF-Flat** — its cell posting lists are the same durable `DiskBTree`, centroids live in a WAL-logged meta page (bounded RAM) — and the production wiring makes it the live vector index: `CREATE INDEX ... USING HNSW` (and a new `USING IVF` alias) builds a durable `DiskIvfIndex`, `NEAR` routes through it (probe cells → exact re-rank from the heap → MVCC/RLS re-check), and **the async index worker is retired** (its last user was the in-RAM HNSW), so `Engine::open` now does **zero index rebuilding for every index type**. Recall@10 = **1.000** matches the HNSW baseline at bounded RAM (`benches/vector_recall.rs`, extended with a 20k-vector sweep + a no-rebuild reopen check); crash point **P17** proves the durable vector index survives a crash with recall intact. **P3.d (large objects) is shipped:** values too large for a tuple are stored **out-of-line, chunked (~7 KiB), and streamed** — a large object is a sequence of chunk rows in a `__lobs__` system table indexed by a durable `DiskBTree` on `lob_id`, so `Engine::put_large_object`/`read_large_object`/`delete_large_object` are atomic with the transaction, crash-recovered (crash point **P16**), vacuum-reclaimable, and stream one chunk at a time (multi-GB without OOM). **With that, Phase 3 is complete** — every secondary index is durable and crash-recovered, `Engine::open` is O(1) regardless of data size, and the crash harness has grown 14 → **19** (P13–P17). See `PROGRESS.md`'s P3.a–P3.d entries.

**Phase 4 (complete): query power — real SQL + a query brain.** Before Phase 4 the engine was single-table filter/project only. This phase makes it a real query engine, entirely additively (a trivial single-table `SELECT` still takes the original fast path; anything richer routes through a new physical-plan tree). **P4.a — joins:** `INNER`/`LEFT`/`RIGHT`/`CROSS` joins via hash join (with Grace spill-to-disk past a memory budget), sort-merge join, and index-nested-loop over the Phase-3 durable B-Tree. **P4.b — aggregation & sort:** `COUNT`/`SUM`/`AVG`/`MIN`/`MAX`, `GROUP BY`/`HAVING`, `DISTINCT`, `ORDER BY` (external merge-sort spill for large inputs), `LIMIT`/`OFFSET`. **P4.c — subqueries & CTEs:** scalar / `IN` / `EXISTS` subqueries (correlated and uncorrelated) and non-recursive `WITH` CTEs. **P4.d — statistics & cost-based optimizer:** `ANALYZE` gathers per-table/column statistics (row counts, distinct counts, equi-depth histograms) persisted durably on the catalog (never recomputed on open); a cost-based optimizer then chooses join order (Selinger left-deep DP ≤ 10 relations, greedy beyond) and index-vs-scan for base access. **P4.e — `EXPLAIN` / `EXPLAIN ANALYZE`** expose the chosen plan tree with estimated rows, and (with `ANALYZE`) actual rows + execution time. Correctness is checked **differentially against SQLite** on shared data for joins, aggregates, and subqueries. See `docs/backlog/phase4_query_power.md` and `PROGRESS.md`'s Phase 4 entry.

---

## Prerequisites

- Rust stable toolchain (`rustup update stable`)
- Cargo (comes with Rust)

Verify:

```bash
rustc --version
cargo --version
```

---

## Build

```bash
# Debug build (fast compile, slower runtime)
cargo build

# Release build (use for benchmarks)
cargo build --release
```

---

## Run the test suite

```bash
# All unit tests + integration tests (embedded crate, default features)
cargo test

# Crash-injection harness only (D7 — P1–P12 injection points + property test)
cargo test --test crash

# Server tests (REST/JWT/SSE/metrics) — requires the `server` feature
cargo test --features server

# Run a specific test by name
cargo test insert_and_get

# With structured log output visible
RUST_LOG=debug cargo test -- --nocapture
```

---

## Run benchmarks

Benchmarks require a release build. Results are written to `target/criterion/`.

```bash
cargo bench                          # load/vector/graph/queue benches
cargo bench --features server        # + server-overhead benches
```

| Bench file | Workload |
|---|---|
| `benches/load.rs` | Single-table INSERT/SELECT/UPDATE + transactional contention |
| `benches/vector.rs` | Vector INSERT/NEAR vs. Postgres+pgvector |
| `benches/graph.rs` | Edge CRUD + adjacency-scan batch-latching |
| `benches/queue.rs` | Event capture/poll/ack vs. Postgres `FOR UPDATE SKIP LOCKED` |
| `benches/server.rs` | HTTP+writer-thread overhead, JWT verification, SSE polling, concurrent throughput ceiling |
| `benches/decompose.rs` | W0–W4 per-commit write decomposition ladder (row → +B-tree → +vector → +edge → +event) vs. durability-matched SQLite; proves the commit-time-fsync default. Also the **`PG_URL`-gated Postgres baseline comparison** (B1–B4: durable insert, CRUD, concurrency, size sweep) reporting both durability lenses — `open_datasync` vs `fsync_writethrough` — side by side; driven by `scripts/pg_compare.sh`. Unaffected by plain `cargo bench` (skips when `PG_URL` unset). |

Full metrics tables for every milestone are in `PROGRESS.md`. Open
`target/criterion/report/index.html` for the HTML report.

---

## Lint and format

PRs must be clean on both gates before merge:

```bash
# Lint (zero warnings allowed)
cargo clippy -- -D warnings

# Format
cargo fmt --all

# Check formatting without modifying files
cargo fmt --all -- --check
```

---

## Use the engine as a library

```rust
use unidb::Engine;

// Open (or create) a database directory.
let mut engine = Engine::open(std::path::Path::new("./mydb"), 0)?;
//                                                              ^ 0 = use default 8 KiB page size

// Everything runs under an explicit MVCC transaction (M1+).
let xid = engine.begin()?; // default isolation: READ COMMITTED (D10)
// engine.begin_with_isolation(IsolationLevel::RepeatableRead)? for SI,
// or IsolationLevel::Serializable for SSI (P1.d) — write-skew is aborted

// SQL: relational, vector, and graph-adjacent DDL/DML all go through one
// execute_sql call; a `;`-separated body runs atomically under one xid.
engine.execute_sql(
    xid,
    "CREATE TABLE docs (id INT, body TEXT, embedding VECTOR(3))",
)?;
engine.execute_sql(
    xid,
    "INSERT INTO docs (id, body, embedding) VALUES (1, 'hello', [0.1, 0.2, 0.3])",
)?;

// Graph edges and Cypher share the same transaction/WAL.
let row_id = engine.create_edge(xid, 1, 2, "LINKS_TO", "{}")?;
engine.execute_cypher(xid, "MATCH (a)-[:LINKS_TO]->(b) WHERE a.id = 1 RETURN b.id")?;

engine.commit(xid)?;

// Raw row CRUD (bypasses SQL) is still available directly on Engine.
let xid = engine.begin()?;
let raw_id = engine.insert(xid, b"raw bytes")?;
let data = engine.get(xid, raw_id)?;
assert_eq!(data, b"raw bytes");
engine.commit(xid)?;

// Checkpoint: flush dirty pages + write checkpoint WAL record + truncate WAL.
engine.checkpoint()?;
```

Add to `Cargo.toml`:

```toml
[dependencies]
unidb = { path = "../unidb" }
```

### Run the REST server (optional, `server` feature)

```bash
UNIDB_JWT_SECRET=dev-secret \
UNIDB_DATA_DIR=/tmp/unidb \
UNIDB_BIND_ADDR=127.0.0.1:8080 \
cargo run --bin unidb-server --features server
```

**Config (env vars, no config file in v1):**

| Var | Default | Purpose |
|---|---|---|
| `UNIDB_JWT_SECRET` | — (**required**) | HMAC secret for verify-only JWT auth. No default — the server refuses to start without one. |
| `UNIDB_DATA_DIR` | `/tmp/unidb` | Storage directory — holds `control`/`data.db`/`db.wal`, nothing else. Defaults under `/tmp` so local/dev runs never write DB files into the repo; `/tmp` is ephemeral across reboots, so set this to a real volume for anything persistent. |
| `UNIDB_LOG_DIR` | `<UNIDB_DATA_DIR>/logs` | Rolling daily log files (`unidb.log.YYYY-MM-DD`). Independently overridable so logs can live on a different volume than data. |
| `UNIDB_BIND_ADDR` | `127.0.0.1:8080` | Listen address. |
| `UNIDB_PAGE_SIZE` | `0` (engine default) | Page size, fixed at first open (D8). |
| `UNIDB_TXN_IDLE_TIMEOUT_SECS` | `60` | Idle deadline for HTTP transaction sessions (R1) — an abandoned open session is auto-aborted by the reaper (it holds locks + pins the vacuum horizon). |
| `UNIDB_CURSOR_IDLE_TIMEOUT_SECS` | `60` | Idle deadline for `POST /sql` result cursors (R4). |

For a real deployment, set `UNIDB_DATA_DIR`/`UNIDB_LOG_DIR` to explicit
absolute paths rather than relying on the relative defaults, which resolve
against whatever directory the process happens to be started from.
Logging goes to **both** stdout (so `docker logs`/systemd journal capture
still works) and the rolling file under `UNIDB_LOG_DIR`.

Full route reference (payloads, responses, error codes, auth model) is in
[`docs/REST_API.md`](docs/REST_API.md). Quick smoke test — token generation
uses [`scripts/gen_jwt.sh`](scripts/gen_jwt.sh) (pure bash + `openssl`, no
Python/PyJWT dependency to install):

```bash
TOKEN=$(UNIDB_JWT_SECRET=dev-secret ./scripts/gen_jwt.sh)
curl -H "Authorization: Bearer $TOKEN" -X POST http://127.0.0.1:8080/sql \
  -d '{"sql":"CREATE TABLE t (id INT)"}'
curl http://127.0.0.1:8080/metrics   # no auth required
```

### Checking server performance

Two ways, depending on whether you have the Rust toolchain and want
rigorous, statistically-sampled numbers, or just a quick check against a
running instance:

```bash
# Rigorous: criterion benchmarks (HTTP+writer-thread overhead vs. direct
# Engine calls, JWT verification cost, SSE polling cost, concurrent
# throughput ceiling). Results in target/criterion/; also recorded in
# PROGRESS.md's M5 entry.
cargo bench --bench server --features server

# Quick: a plain-shell smoke test against any running server (local or
# deployed) — no Rust toolchain needed, just curl/openssl/awk. Reports
# sequential p50/p99 latency, concurrent throughput, and a /metrics
# snapshot. See scripts/bench_server.sh for env vars (BASE_URL, REQUESTS,
# CONCURRENCY).
UNIDB_JWT_SECRET=dev-secret ./scripts/bench_server.sh
```

### Rust attach client (`unidb-attach`, M8)

A third deployment mode, alongside embedding `unidb::Engine` directly in
your process or running the REST server standalone: attach to an
already-running server from a separate Rust process, with one call per
operation (no explicit `begin`/`commit` — every REST route already wraps
its own transaction server-side; use `;`-separated SQL in `execute_sql`
for multi-statement atomicity).

```toml
[dependencies]
unidb-attach = { path = "unidb-attach" }
```

```rust
use unidb_attach::AttachClient;

let client = AttachClient::new("http://127.0.0.1:8080", &token)?;
client.execute_sql("CREATE TABLE t (id INT, name TEXT)")?;
let rows = client.execute_sql("SELECT * FROM t")?;
```

It is Rust-only in v1 (other languages tracked in `docs/backlog/`), uses a
blocking `reqwest` client (no tokio runtime, no background thread), and
covers CRUD, SQL, Cypher, graph edges, indexing, and events — the full
pre-enrichment REST surface. The REST-enrichment routes (item 12 —
transaction sessions via `X-Txn-Id`, `/events/vacuum`,
`/tables/{table}/rls`, `/admin/flush`, `/rows/batch`, `/sql` cursors) now
exist on the server but are not yet wrapped by the client (an optional
follow-up; sessions are just a header on the existing calls). See
[`unidb-attach/src/lib.rs`](unidb-attach/src/lib.rs) for the full method
list and [`docs/REST_API.md`](docs/REST_API.md) for the underlying wire
contract.

```bash
# Attach-client tests spin up a real unidb-server test instance
cargo test -p unidb-attach

# Attach-client call overhead vs. direct embedded Engine calls
cargo bench -p unidb-attach
```

### Tracing / structured logging

Call once at startup to activate `RUST_LOG`-controlled output:

```rust
unidb::init_tracing();
```

```bash
RUST_LOG=info  ./myapp   # checkpoint events, WAL opens
RUST_LOG=debug ./myapp   # page flushes, allocations
RUST_LOG=trace ./myapp   # every WAL record written
```

**Server logs (item 22).** The `unidb-server` binary logs **JSON lines** to both
stdout and rolling daily files under `UNIDB_LOG_DIR` (set `UNIDB_LOG_FORMAT=text`
for a human-readable console during local dev). Each request gets a
`request_id` (returned as `x-request-id`) that — with `txn_id` — joins its lines
across the app log, the slow-query log, and `audit.log`. `GET /logs`
(superuser-gated) is a bounded, cursor-paged reverse read of those JSON files
for local triage; a real deployment ships the same files to CloudWatch/Datadog
(see `docs/ops_runbook.md` §8). This is all in the `server` feature only — the
default embedded build stays sync with no new dependency.

---

## Project layout

The repo root is a Cargo workspace: `unidb` (the embedded engine — everything below stays at the repo root, unaffected by the workspace split) plus `unidb-attach` (the REST attach client, M8), `unidb-embed` (client-side embedding CLI), `unidb-dispatch` (the downstream event dispatcher, Milestone 20 — embeds the engine, fans the event stream out to webhooks/rooms; keeps `tokio`/`reqwest` out of the engine's default sync build), and `unidb-storage` (the object storage service, item 23 — bucket/object metadata in unidb tables, bytes tiered between engine LOBs and an S3-wire store (MinIO/S3) with presigned URLs + an outbox/reconciler; keeps `tokio`/the AWS SDK out of the engine's default sync build).

```
src/
  format.rs        — magic number, version, constants, little-endian helpers
  error.rs         — DbError enum + Result alias
  control.rs       — control file (single source of recovery truth; holds next_xid too)
  mmap.rs          — sole unsafe module: memory-mapped file wrapper
  page.rs          — slotted-page layout, tuple header (xmin/xmax), CRC32 on every read
  bufferpool.rs    — fixed frames, clock eviction, WAL-before-page invariant (D5)
  wal.rs           — append-only log, redo+undo payloads, mini-transaction bracketing
  heap.rs          — MVCC-versioned heap: insert / get / update / delete
  mvcc.rs          — snapshot visibility rules (RC / RR)
  txn.rs           — transaction manager: begin/commit/abort, xid allocation
  lockmgr.rs       — lock manager keyed by (record_kind, record_id), SI abort-on-conflict; lock-wait/deadlock counters (item 21)
  metrics.rs       — lock-free AtomicHistogram + counter snapshots for stats()/­/metrics (item 21)
  concurrency_hooks.rs — on_read/on_write seam (no-op today, SSI landing point later)
  catalog.rs       — table/column/index definitions, RLS policies, serde_json-persisted
  sql/             — parser.rs, logical.rs (plan + RLS rewrite), executor.rs, datetime.rs (timestamp parse/format), query/plan/query_exec/join/aggregate/sort/optimizer/statistics/explain.rs (Phase-4 query power), information_schema.rs (Milestone 18: information_schema.*/unidb_catalog.* introspection as synthesized virtual relations)
  vector.rs        — in-RAM HNSW wrapper (instant-distance); retired from the runtime in P3.c, kept only as the recall/latency baseline in benches/vector_recall.rs
  fulltext.rs      — full-text tokenizer; the inverted index itself is a durable DiskBTree keyed on tokens (P3.b), read via Engine::search_fulltext
  btree_index.rs   — durable on-disk B+tree (DiskBTree) for equality/range WHERE predicates; paged, WAL-logged, crash-recovered, no rebuild on open (P3.a); also backs full-text + edge indexes (P3.b)
  csr_index.rs     — Compressed Sparse Row adjacency structure (retired from the runtime in P3.b; module kept for its benchmark)
  disk_vector.rs   — P3.c: durable on-disk IVF-Flat vector index (DiskIvfIndex); posting lists = durable DiskBTree, centroids in a WAL-logged meta page; backs CREATE INDEX ... USING HNSW|IVF and NEAR, no rebuild on open
  large_object.rs  — P3.d: out-of-line chunked + streamed large objects (__lobs__ rows + durable lob_id index); Engine::{put,read,delete}_large_object
  graph/           — edges.rs, index.rs, logical.rs, parser.rs, executor.rs (Cypher subset)
  queue/           — mod.rs (event capture, poll/ack/vacuum + poll_events_after live-tail cursor, M20 E1), payload.rs
  replication/     — P6.b/c: SlotRegistry (slots.json) + Replica (base snapshot + incremental WAL apply, promote/failover)
  backup/          — P6.d: base backup, WAL archiving, and restore/PITR (by target LSN); item 28: timeline.rs (16-byte (ts,lsn) marks), restore_to_time (wall-clock PITR)
  authz/           — P6.e: RoleStore (roles.json) — users/roles/GRANT, per-table privileges, auth-DDL parser
  audit/           — P6.f: append-only security audit trail (audit.log); item 22: +txn_id/request_id correlation fields + app-log mirror
  observability.rs — item 22 (L2): default-build thread-local request_id (server sets it on each blocking engine call; slow-query + audit read it)
  autovacuum.rs    — A1–A4: background std::thread launcher (Weak<Engine>, naptime, Postgres-shape policy) that auto-triggers Engine::vacuum; clean shutdown on Engine drop
  checkpoint.rs    — flush dirty pages → checkpoint record (+ next_xid) → truncate WAL (segment-aware, slot-floored — P6.a/b)
  recovery.rs      — ARIES-style redo + undo on open (scans all WAL segments in LSN order)
  wal.rs           — segmented append-only log (16 MiB segments, P6.a); redo+undo payloads; mini-txn bracketing; ship/decode-stream
  server/          — optional REST/JWT/SSE/metrics server (feature = "server"); tls.rs (rustls termination, P6.f); txn_session.rs (multi-request transaction sessions, R1) + cursor.rs (large-result cursors, R4) — REST enrichment, item 12; correlation.rs (request_id middleware/task-local, item 22) + logs.rs (bounded reverse-seek GET /logs, item 22)
  bin/unidb-server.rs — the server binary (required-features = ["server"]); HTTPS when UNIDB_TLS_CERT/KEY are set
  lib.rs           — Engine public API, init_tracing(); Engine::stats() + slow-query log (P6.g)
tests/
  crash/           — crash-injection harness (P1–P19 injection points + property test; 21 tests)
  server_*.rs      — REST server integration tests (feature = "server"); server_{replication,authz,tls,stats}.rs (Phase 6)
  authz.rs, observability.rs, replication.rs — Phase 6 engine-level integration tests
  graph_*.rs, vector_mvcc.rs, queue_*.rs, index_rebuild.rs, btree_mvcc.rs — per-milestone integration tests
benches/
  load.rs, vector.rs, graph.rs, queue.rs, server.rs, btree.rs, phase6_ops.rs — benchmarks per milestone/phase
scripts/
  bench_server.sh  — plain-shell perf smoke test against a running server (no Rust toolchain)
  gen_jwt.sh       — generate a verify-only HS256 JWT (bash + openssl, no Python/PyJWT)
  pg_compare.sh    — bring up Postgres (native-preferred; --docker mode), run the unidb-vs-Postgres baseline comparison (both durability lenses), report peak RSS, tear down
docs/
  engine_access_guide.md — Application Builder's Guide (Milestone 18): connect → query → introspect (information_schema/unidb_catalog catalog) → types → page → errors, + a schema-explorer recipe
  REST_API.md      — full HTTP route reference (payloads, responses, error codes)
  backlog/         — saved plans for not-yet-started future work (e.g. Phase 2 SQL expansion)
unidb-attach/
  src/lib.rs       — AttachClient: blocking reqwest client over the REST API (M8), Rust-only v1
  tests/           — attach_crud.rs, attach_sql.rs, attach_graph.rs, attach_extras.rs
  benches/attach.rs — attach-client call overhead vs. direct embedded Engine calls
unidb-dispatch/    — Milestone 20 (E2): downstream event dispatcher (own workspace crate, embeds Arc<Engine>)
  src/lib.rs       — Dispatcher: durable-offset poll→fan-out→ack loop, at-least-once, lag/vacuum-horizon warning
  src/sink.rs      — Sink trait + WebhookSink (retry→dead-letter), RoomSink (broadcast rooms), CollectingSink
  src/filter.rs    — per-subscription table/op filter + column projection (consumer-side; engine transforms nothing)
  src/dlq.rs       — dead-letter table dogfooded back into unidb (create + $n-bound insert)
  tests/           — dispatch_delivery.rs (at-least-once + resume + crash/replay zero-loss), dispatch_webhook_dlq.rs
unidb-storage/     — item 23: object storage service (own workspace crate, embeds Arc<Engine>; adds no engine surface)
  src/service.rs   — StorageService: put/get/delete, LOB-inline vs S3 tiering, presigned begin/finish upload
  src/store/       — ObjectStore trait + S3ObjectStore (MinIO+S3, aws-sdk-s3) + MemoryObjectStore (Docker-free tests)
  src/reconcile.rs — Reconciler: confirm/compensate the outbox + orphan-byte sweep
  src/metadata.rs  — buckets/objects/object_dlq tables as ordinary unidb SQL; src/outbox.rs — ConfirmSink (item-20 Dispatcher fast path)
  tests/           — round_trip.rs, crash_consistency.rs (both directions), outbox_dispatcher.rs, presign_and_config.rs, scale.rs
```

---

## Milestone roadmap

Milestones M0–M11 are **shipped, tested, and benchmarked**, and **Phase 2
(real data model) is complete** — the project is executing the phased scaling
plan in `docs/backlog/roadmap.md` (Phase 1 ACID hardening runs in parallel;
Phase 4 query power is next for the SQL lane). Metrics tables are in
`PROGRESS.md`; current implementation state and known tech debt are in
`MEMORY.md`.

| Milestone | Status | Summary |
|-----------|--------|---------|
| M0 — Storage core | done | Single-file page store, buffer pool, WAL, control file, crash recovery, single-table CRUD |
| M1 — MVCC + CRUD | done | Transactions, READ COMMITTED / REPEATABLE READ, SQL subset, JSON columns, RLS |
| M2 — Vector & Text search | done | `VECTOR(n)` type, async HNSW index, `NEAR` operator, full-text inverted index |
| M3 — Graph | done | Edge records, edge-list index, Cypher subset |
| M4 — Event queue | done | WAL-derived stream, durable consumer offsets, `vacuum_events` |
| M5 — API / server | done | Optional REST server (`docs/REST_API.md`) + verify-only JWT auth + SSE subscribe (durable-consumer *and* ephemeral live-tail with `Last-Event-ID`/`from_seq` resume + `table` filter, M20 E1) + `/metrics` |
| M20 — Events dispatcher | done | E1 SSE framing/resume; E2 `unidb-dispatch` crate — durable-offset fan-out to webhooks (retry→dead-letter table, dogfood) + rooms, at-least-once, crash/replay zero-loss; E3 event-schema contract in `docs/engine_access_guide.md` |
| item 26 — Event queue at scale | done | Q1: durable seq-index on `__events__` (`DiskBTree`) — `poll_events` / `poll_events_after` now O(log n + returned), flat latency proven 10k→300k; Q2: `EventWake` condvar — commit notifies, SSE wakes on push, idle cost zero; Q3: `vacuum_events` removes seq-index entries on reclaim (no retention pin) |
| item 29 — Subscription CDC | done | C1: `before`/`after`/`ts_ms` row images per op (back-compat flat `payload` preserved); C2: canonical native envelope + Debezium (`?format=debezium`) + Supabase (`?format=supabase`) format adapters on SSE subscribe; C3: `unidb_catalog.subscription_lag` virtual relation + `/stats` JSON + Prometheus `unidb_subscription_lag_events{consumer}` / `unidb_subscription_lag_seconds{consumer}`; C4: guide §8 updated with contract, three format examples, and lag detection guidance |
| item 23 — Object storage | done | `unidb-storage` crate — metadata in unidb tables, bytes tiered LOB-inline (ACID) / S3-wire (MinIO+S3, presigned PUT/GET); outbox + reconciler (confirm / compensate→DLQ / orphan-sweep); no engine surface added |
| M6 — B-Tree index | done | General-purpose secondary index for equality/range `WHERE` predicates, index-assisted `SELECT` |
| M7 — CSR graph index | retired (P3.b) | Compressed Sparse Row adjacency; consulted by no read path after the M7 traversal revert, superseded by the durable edge index — retired from the runtime in P3.b |
| M8 — Attach client | done | `unidb-attach`: Rust blocking-`reqwest` client over the REST API, no new protocol |
| M10 — Heap vacuum / GC | done | `Engine::vacuum()`: reader-aware horizon, crash-safe `WAL_VACUUM`, secondary-index vacuum gate, page compaction + slot reuse |
| Autovacuum (A1–A4 + item 27) | done | Background `std::thread` launcher auto-triggers vacuum via Postgres-shape policy; **per-table** dead/live estimates (`Engine::per_table_dead_estimate`, `tables_needing_vacuum`), `Engine::vacuum_table` scoped pass, `VacuumCostConfig` cost throttle (page-hit/dirty accounting + nap), `/stats` + `/metrics`, clean shutdown, crash points P26 + P30 |
| M11 — SQL constraints | done | `PRIMARY KEY` / `FOREIGN KEY` / `UNIQUE` / `NOT NULL` / `CHECK` / `DEFAULT` on `CREATE TABLE`, enforced on INSERT/UPDATE |
| P2.a — DECIMAL + TIMESTAMP | done | Exact fixed-point `DECIMAL(p, s)` (money) and UTC `TIMESTAMP` (time) column types, with round-trip + ordering + constraint support (Phase 2, SQL lane) |
| P2.b — FLOAT/UUID/BYTEA/DATE/TIME | done | Five more scalar types on the same encoding/coercion/comparison machinery |
| P2.c — ALTER/DROP/TRUNCATE | done | Schema evolution — `ADD COLUMN` (with `DEFAULT`), tombstone `DROP COLUMN`, `DROP TABLE`, `TRUNCATE`, and request-level DDL rollback |
| P2.d — sequences / SERIAL | done | Durable, monotonic, crash-safe auto-increment (`SERIAL` / `GENERATED AS IDENTITY`) |
| P2.e — prepared statements | done | `$n` bind parameters (`execute_sql_params`, `prepare`/`execute_prepared`, `POST /sql` `params`) — closes the SQL-injection surface |
| P3.a — durable B-Tree | done | The M6 B-Tree is now an on-disk B+tree (`DiskBTree`): paged, buffer-pool-managed, WAL-logged (`WAL_INDEX`), crash-recovered, **not rebuilt on open** (Phase 3, Core lane) |
| P3.b — durable full-text + edge index | done | Full-text (inverted) and edge-adjacency indexes are durable `DiskBTree`s too (no rebuild on open); CSR retired; new `Engine::search_fulltext` read path |
| P3.c — on-disk vector index | done | Durable on-disk IVF-Flat (`DiskIvfIndex`): posting lists = durable `DiskBTree`, centroids in a WAL-logged meta page; `CREATE INDEX ... USING HNSW\|IVF` builds it, `NEAR` reads it, async worker retired, **no rebuild on open**; recall@10=1.0 matches HNSW (crash point P17) |
| P3.d — large-object storage | done | Out-of-line chunked + streamed big files (`__lobs__` rows + durable `DiskBTree` index); atomic with the txn, crash-recovered, vacuum-reclaimable; `Engine::{put,read,delete}_large_object` — multi-GB without OOM |
| P6.a — segmented WAL | done | `db.wal/` is a directory of fixed-size 16 MiB segments; seal + rotate; truncation deletes whole consumed segments (no rewrite). Enables concurrent WAL readers (crash point P18) |
| P6.b — replication slots + WAL shipping | done | Persisted `SlotRegistry` (`slots.json`) holds the WAL truncation floor; `ship_wal`/`decode_stream`; REST `/replication/{slots,stream}` |
| P6.c — read replicas + failover | done | `replication::Replica`: base snapshot + incremental WAL apply, `promote()` failover, `wait_for_sync_replicas` synchronous option |
| P6.d — backups + PITR | done | `Engine::base_backup`/`archive_wal`, `backup::restore(base, archive, dest, target_lsn)` — point-in-time recovery by LSN (crash point P19); **item 28 extends this: `restore_to_time(target_ts_micros)` maps wall-clock → LSN via a side `timeline.bin` mark file (no WAL format change; one 16-byte mark per commit)** |
| item 28 — Logical replication | done | `unidb-logical` workspace crate: `LogicalReplicator` wraps the item-20 `Dispatcher` + `LogicalApplySink`; applies INSERT/UPDATE/DELETE from the item-26 event stream to a target engine; at-least-once, offset-durable, survives primary restart; tables outside scope skipped; key-column-update gap documented as item-26 follow-up |
| P6.e — users/roles/GRANT | done | `authz::RoleStore` (`roles.json`): users/roles/privileges, transitive membership, `execute_sql_as` enforcement, per-user JWT `sub` (open/bootstrap mode) |
| P6.f — security | done | Native TLS (rustls/`axum-server`) + audit log (`audit.log`). Encryption-at-rest deferred (D9 sign-off-gated) |
| P6.g — observability | done | `Engine::stats()` + `GET /stats` (`pg_stat_*`-style), slow-query log, ops runbook (`docs/ops_runbook.md`); EXPLAIN from P4.e |
| Observability metrics (item 21) | done | Lock-free per-chokepoint metrics in `stats()`/`GET /stats` + `/metrics`: per-statement-kind latency histograms, WAL-fsync cost, buffer-pool hit/miss/evict, lock-wait + deadlock counters, the alertable **vacuum-horizon-age gauge** (item-16 lesson), per-table page counts, and worker-governance utilization. Widget-traceability table in `docs/engine_access_guide.md` §9 |
| Logs surface (item 22) | done | JSON-lines server logs; per-request `request_id`+`txn_id` correlation across app/slow-query/`audit.log`; superuser-gated `GET /logs` — bounded, cursor-paged reverse read (cap + scan budget, no OOM/stall); CW/Datadog shipping guidance |
| Multi-page catalog (item 25) | done | `Catalog::persist`/`load` now chain across N 8 KiB pages (in-band magic detection, write-new-chain-then-flip atomicity, no `FORMAT_VERSION` bump, old blobs open unchanged); removes the ~8 KiB `HeapFull` ceiling item 23 had to work around — unlimited schema size, ANALYZE/SERIAL no longer overflow at runtime; crash point P33 |

---

## Design decisions (locked — do not re-open without sign-off)

| ID | Decision |
|----|----------|
| D1 | Buffer policy: steal + no-force, ARIES-style (both redo and undo logging) |
| D2 | Atomic unit is a mini-transaction (WAL-bracketed group of page writes) |
| D3 | Control file holds magic, version, page_size, checkpoint LSN, WAL tail, next_xid |
| D4 | Tuple header reserves xmin/xmax now; in-place UPDATE in M0, MVCC in M1 |
| D5 | WAL-before-page invariant: no dirty page flushed while page.LSN > durable WAL LSN |
| D6 | Single-file *data* storage; the WAL may be separate. **Evolved (P6.a, signed off 2026-07-09):** the WAL is now a directory of 16 MiB segment files. The data store stays a single file |
| D7 | Crash-injection harness: kill at defined points, reopen, assert recovered state (grows with each new durability mechanism — P1–P19 today, 21 tests) |
| D8 | Page size 8 KiB default, config-overridable at init, fixed after creation |
| D9 | On-disk format is fixed little-endian; every page carries CRC32 + LSN |
| D10 | Default isolation: READ COMMITTED; REPEATABLE READ + SERIALIZABLE (SSI, P1.d) available |
| D11 | `on_read`/`on_write` seam built now (no-op) for future SSI without executor rewrite |
| D12 | SI abort-on-conflict, then RC re-evaluation + SSI (all shipped P1.d) |
| D13 | Structured logging (tracing) from day one for WAL writes, checkpoints, recovery |
