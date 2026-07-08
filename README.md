# unidb

A single embedded storage/transaction engine in Rust that unifies relational CRUD, vector search (HNSW), graph edges, and a WAL-derived event queue over one page store, one WAL, one buffer pool, and one transaction manager.

The competitive edge is eliminating the multi-system dual-write tax. "Save row + embedding + graph edge + event" is one WAL append and one commit here, versus 3–4 network round-trips with no shared transaction across Postgres + a vector store + a graph DB + Kafka.

**Status: M0–M11 all shipped.** Single-file storage core, MVCC transactions + SQL subset, vector/full-text search, a graph layer with a Cypher subset, a WAL-derived event queue, an optional REST/JWT/SSE/metrics server, a B-Tree secondary index, a CSR graph index, a Rust attach client (`unidb-attach`), group-commit + concurrent reads, semantic search (cosine metric + embedding CLI), SQL constraints, and heap vacuum/GC are all implemented, tested, and benchmarked. See `PROGRESS.md` for milestone-by-milestone benchmark tables and `MEMORY.md` for current implementation state and known tech debt.

**Performance (post-M8): group commit + read-only fsync skip.** Read-only transactions no longer pay a commit fsync (embedded point `SELECT` ~3.05 ms → ~1.09 µs), and the server writer thread now group-commits — batching all queued requests behind a single fsync per batch — so concurrent write throughput scales with load instead of hitting a flat single-writer ceiling (~131 → ~4,780 ops/s from 1 to 50 concurrent `POST /sql` INSERTs). The embedded (non-server) path keeps per-statement durability unchanged; group-commit deferral is enabled only by the single-owner server writer thread. The buffer pool now forces the WAL before stealing a not-yet-durable dirty page (ARIES no-force), so deferred mode is safe for working sets larger than the pool — which also removes the older `BufferPoolFull`-at-scale limitation. Reads run off the single writer thread too: a `Send + Sync` `ReadHandle` lets point reads (`get` / `GET /rows/:id`) and read-only SQL `SELECT` (`POST /sql`) run concurrently with each other and with the writer, coordinating only through MVCC snapshots (writes, DDL, and `NEAR` stay on the writer thread by design). See `docs/backlog/group_commit_and_read_concurrency.md`.

**M10: heap vacuum / MVCC garbage collection.** The engine now physically reclaims space held by dead tuple versions via an explicit `Engine::vacuum() -> VacuumReport` (there is no autovacuum in v1; it mirrors the existing `vacuum_events` explicit-call model). Vacuum computes a conservative visibility horizon over every live transaction **and** every live concurrent `ReadHandle` reader, marks reclaimable versions' line pointers DEAD via a crash-safe redo-only `WAL_VACUUM` record, scrubs those `RowId`s from every secondary index **before** any slot becomes reusable (the aliasing gate that keeps a reused slot from resolving a stale index entry to a live, wrong row), then compacts each page and frees the slots. Long-lived `REPEATABLE READ` transactions or readers hold the horizon back and are surfaced in `VacuumReport.horizon_blocked` rather than silently ignored. Embedded-API only in v1 (no REST route). See `docs/backlog/m10_heap_vacuum_gc.md` and `PROGRESS.md`'s M10 entry.

**Phase 1 (complete): ACID & storage foundation — the feature-freeze gate.** Before further scale/feature work, the engine closed its silent correctness holes (`docs/backlog/roadmap.md` §4, `docs/backlog/phase1_acid_hardening.md`). **P1.a — full-page-writes — is shipped:** an 8 KiB page write is not atomic, so a crash mid-write used to leave a torn page that CRC detects but cannot repair (the #1 data-loss hole). Now, on the first modification of a page after each checkpoint, the buffer pool logs the whole clean page image to the WAL (a redo-only `WAL_FPI` record); recovery replays it as the clean base before re-applying the interval's incremental redo on top, so a torn on-disk page is fully reconstructed. New crash point **P11** manufactures a real torn page and asserts recovery (`FORMAT_VERSION` 3→4). **P1.b — fsync-failure handling — is also shipped:** a failed `fsync`/`msync` may leave the OS having dropped the dirty data while clearing its dirty bit, so the WAL and buffer pool now treat a durability failure as fatal — they latch a poisoned state and never falsely report success (new crash point **P12**). **P1.c — the scaling foundation — is also shipped:** the page file now grows in 4 MiB chunks (was a whole-file remap on every `alloc_page` — O(N²), fatal at 100s of GB), the buffer pool is configurable (`UNIDB_BUFFER_POOL_PAGES`, default raised 256→4096 frames), and a real free-space map replaces the linear per-insert page scan — so insert/read throughput stays flat as a table grows (`benches/scale.rs`). **P1.d — isolation correctness — is also shipped:** write-write conflicts under `REPEATABLE READ`/`SERIALIZABLE` now surface as `SerializationFailure` (not a raw conflict), `READ COMMITTED` re-reads the latest committed version via its fresh per-statement snapshot (no spurious abort), and a new `SERIALIZABLE` level uses SSI rw-antidependency (pivot) detection to prevent write-skew. **P1.e — auto-checkpoint — is shipped, completing the phase:** the WAL used to grow unbounded (checkpoint was manual-only), so a checkpoint now fires automatically on a time or WAL-size trigger (at a quiescent point), keeping the WAL bounded (benchmarked ~8–23× smaller) at unchanged throughput. The crash-injection harness grew from 11 to **14** points and `FORMAT_VERSION` went 3→4; no locked decision was reversed. See `PROGRESS.md`'s Phase 1 entries.

**Phase 3 (in progress): multi-model durable storage — the moat.** This phase kills the "rebuild every secondary index on open" tax (O(all data) startup, RAM-bound) by making the indexes durable on disk. **P3.a — the durable B-Tree — is shipped:** the B-Tree secondary index is now an on-disk B+tree (`DiskBTree`) whose nodes are pages in the shared page store, buffer-pool-managed, WAL-logged as full node-page images (a new redo-only `WAL_INDEX` record) and crash-recovered — so `Engine::open` reads it straight from a stable meta page instead of rescanning the heap to rebuild it. It moved off the async index worker onto the synchronous writer/read path, and vacuum scrubs it directly. New crash point **P13** wipes the entire data file and proves the whole tree is reconstructed from the WAL alone; the harness grew 14 → **15** and `FORMAT_VERSION` went 4→5. The open-cost benchmark (`benches/durable_index.rs`) shows B-Tree reopen time is flat as the table grows while a still-rebuilt HNSW index's reopen time rises. **P3.b is also shipped:** the **full-text (inverted)** and **edge-adjacency** indexes are now durable `DiskBTree`s too (reusing the same `WAL_INDEX` machinery, no new format version) — full-text keyed on tokens, the edge index keyed on `__edges__.from_id` — so neither is rebuilt on open; a new Rust-API `Engine::search_fulltext` gives the durable full-text index a real read path. The M7 **CSR index was retired** (it was consulted by no read path after the M7 traversal revert; adjacency is now served durably by the edge index), leaving the async worker serving only the vector index. Crash points **P14/P15** cover the two new durable indexes (harness 15 → **17**). **P3.c (on-disk vector) is spike-complete:** a spike (`docs/design/p3c_vector_spike.md`) chose **on-disk IVF-Flat** — its cell posting lists are the same durable `DiskBTree`, centroids stay in bounded RAM — and validated **recall@10 = 1.000 at nprobe=4** against brute-force ground truth (`benches/vector_recall.rs`), while the in-RAM HNSW baseline took 30 s to build just 1,200 vectors; the spike also found and fixed a real `DiskBTree` duplicate-key-spanning-leaves bug. The production wiring is a follow-up PR. P3.d (large objects) is in progress. See `PROGRESS.md`'s P3.a–P3.c entries.

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
UNIDB_DATA_DIR=./unidb-data \
UNIDB_BIND_ADDR=127.0.0.1:8080 \
cargo run --bin unidb-server --features server
```

**Config (env vars, no config file in v1):**

| Var | Default | Purpose |
|---|---|---|
| `UNIDB_JWT_SECRET` | — (**required**) | HMAC secret for verify-only JWT auth. No default — the server refuses to start without one. |
| `UNIDB_DATA_DIR` | `./unidb-data` | Storage directory — holds `control`/`data.db`/`db.wal`, nothing else. |
| `UNIDB_LOG_DIR` | `<UNIDB_DATA_DIR>/logs` | Rolling daily log files (`unidb.log.YYYY-MM-DD`). Independently overridable so logs can live on a different volume than data. |
| `UNIDB_BIND_ADDR` | `127.0.0.1:8080` | Listen address. |
| `UNIDB_PAGE_SIZE` | `0` (engine default) | Page size, fixed at first open (D8). |

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
REST surface except `vacuum_events`/`set_rls_policy`/`flush`, which have
no REST route to call (also tracked in `docs/backlog/`). See
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

---

## Project layout

The repo root is a Cargo workspace with two members: `unidb` (the embedded engine — everything below stays at the repo root, unaffected by the workspace split) and `unidb-attach` (the REST attach client, M8).

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
  lockmgr.rs       — lock manager keyed by (record_kind, record_id), SI abort-on-conflict
  concurrency_hooks.rs — on_read/on_write seam (no-op today, SSI landing point later)
  catalog.rs       — table/column/index definitions, RLS policies, serde_json-persisted
  sql/             — parser.rs, logical.rs (plan + RLS rewrite), executor.rs, datetime.rs (timestamp parse/format)
  vector.rs        — HNSW wrapper (instant-distance) for VECTOR(n) columns
  fulltext.rs      — full-text tokenizer; the inverted index itself is a durable DiskBTree keyed on tokens (P3.b), read via Engine::search_fulltext
  btree_index.rs   — durable on-disk B+tree (DiskBTree) for equality/range WHERE predicates; paged, WAL-logged, crash-recovered, no rebuild on open (P3.a); also backs full-text + edge indexes (P3.b)
  csr_index.rs     — Compressed Sparse Row adjacency structure (retired from the runtime in P3.b; module kept for its benchmark)
  index_worker.rs  — background thread; since P3.b builds only the HNSW vector index (B-Tree/full-text/edge are durable/synchronous)
  graph/           — edges.rs, index.rs, logical.rs, parser.rs, executor.rs (Cypher subset)
  queue/           — mod.rs (event capture, poll/ack/vacuum), payload.rs
  checkpoint.rs    — flush dirty pages → checkpoint record (+ next_xid) → truncate WAL
  recovery.rs      — ARIES-style redo + undo on open
  server/          — optional REST/JWT/SSE/metrics server (feature = "server")
  bin/unidb-server.rs — the server binary (required-features = ["server"])
  lib.rs           — Engine public API, init_tracing()
tests/
  crash/           — crash-injection harness (P1–P12 injection points + property test)
  server_*.rs      — REST server integration tests (feature = "server")
  graph_*.rs, vector_mvcc.rs, queue_*.rs, index_rebuild.rs, btree_mvcc.rs — per-milestone integration tests
benches/
  load.rs, vector.rs, graph.rs, queue.rs, server.rs, btree.rs — criterion benchmarks per milestone
scripts/
  bench_server.sh  — plain-shell perf smoke test against a running server (no Rust toolchain)
  gen_jwt.sh       — generate a verify-only HS256 JWT (bash + openssl, no Python/PyJWT)
docs/
  REST_API.md      — full HTTP route reference (payloads, responses, error codes)
  backlog/         — saved plans for not-yet-started future work (e.g. Phase 2 SQL expansion)
unidb-attach/
  src/lib.rs       — AttachClient: blocking reqwest client over the REST API (M8), Rust-only v1
  tests/           — attach_crud.rs, attach_sql.rs, attach_graph.rs, attach_extras.rs
  benches/attach.rs — attach-client call overhead vs. direct embedded Engine calls
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
| M5 — API / server | done | Optional REST server (`docs/REST_API.md`) + verify-only JWT auth + SSE subscribe + `/metrics` |
| M6 — B-Tree index | done | General-purpose secondary index for equality/range `WHERE` predicates, index-assisted `SELECT` |
| M7 — CSR graph index | retired (P3.b) | Compressed Sparse Row adjacency; consulted by no read path after the M7 traversal revert, superseded by the durable edge index — retired from the runtime in P3.b |
| M8 — Attach client | done | `unidb-attach`: Rust blocking-`reqwest` client over the REST API, no new protocol |
| M10 — Heap vacuum / GC | done | `Engine::vacuum()`: reader-aware horizon, crash-safe `WAL_VACUUM`, secondary-index vacuum gate, page compaction + slot reuse |
| M11 — SQL constraints | done | `PRIMARY KEY` / `FOREIGN KEY` / `UNIQUE` / `NOT NULL` / `CHECK` / `DEFAULT` on `CREATE TABLE`, enforced on INSERT/UPDATE |
| P2.a — DECIMAL + TIMESTAMP | done | Exact fixed-point `DECIMAL(p, s)` (money) and UTC `TIMESTAMP` (time) column types, with round-trip + ordering + constraint support (Phase 2, SQL lane) |
| P2.b — FLOAT/UUID/BYTEA/DATE/TIME | done | Five more scalar types on the same encoding/coercion/comparison machinery |
| P2.c — ALTER/DROP/TRUNCATE | done | Schema evolution — `ADD COLUMN` (with `DEFAULT`), tombstone `DROP COLUMN`, `DROP TABLE`, `TRUNCATE`, and request-level DDL rollback |
| P2.d — sequences / SERIAL | done | Durable, monotonic, crash-safe auto-increment (`SERIAL` / `GENERATED AS IDENTITY`) |
| P2.e — prepared statements | done | `$n` bind parameters (`execute_sql_params`, `prepare`/`execute_prepared`, `POST /sql` `params`) — closes the SQL-injection surface |
| P3.a — durable B-Tree | done | The M6 B-Tree is now an on-disk B+tree (`DiskBTree`): paged, buffer-pool-managed, WAL-logged (`WAL_INDEX`), crash-recovered, **not rebuilt on open** (Phase 3, Core lane) |
| P3.b — durable full-text + edge index | done | Full-text (inverted) and edge-adjacency indexes are durable `DiskBTree`s too (no rebuild on open); CSR retired; new `Engine::search_fulltext` read path |
| P3.c — on-disk vector index | spike done | On-disk IVF-Flat chosen (posting lists = durable `DiskBTree`, bounded-RAM centroids); recall@10=1.0 at nprobe=4 validated; production wiring is a follow-up PR |

---

## Design decisions (locked — do not re-open without sign-off)

| ID | Decision |
|----|----------|
| D1 | Buffer policy: steal + no-force, ARIES-style (both redo and undo logging) |
| D2 | Atomic unit is a mini-transaction (WAL-bracketed group of page writes) |
| D3 | Control file holds magic, version, page_size, checkpoint LSN, WAL tail, next_xid |
| D4 | Tuple header reserves xmin/xmax now; in-place UPDATE in M0, MVCC in M1 |
| D5 | WAL-before-page invariant: no dirty page flushed while page.LSN > durable WAL LSN |
| D6 | Single-file storage for M0 (WAL may be a separate file) |
| D7 | Crash-injection harness: kill at defined points, reopen, assert recovered state (grows with each new durability mechanism — P1–P12 today) |
| D8 | Page size 8 KiB default, config-overridable at init, fixed after creation |
| D9 | On-disk format is fixed little-endian; every page carries CRC32 + LSN |
| D10 | Default isolation: READ COMMITTED; REPEATABLE READ + SERIALIZABLE (SSI, P1.d) available |
| D11 | `on_read`/`on_write` seam built now (no-op) for future SSI without executor rewrite |
| D12 | SI abort-on-conflict, then RC re-evaluation + SSI (all shipped P1.d) |
| D13 | Structured logging (tracing) from day one for WAL writes, checkpoints, recovery |
