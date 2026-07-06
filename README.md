# unidb

A single embedded storage engine in Rust that unifies relational CRUD, vector search (HNSW), graph edges, and a WAL-derived event queue over one page store, one WAL, one buffer pool, and one transaction manager.

The competitive edge is eliminating the multi-system dual-write tax. "Save row + embedding + graph edge + event" is one WAL append and one commit here, versus 3–4 network round-trips with no shared transaction across Postgres + a vector store + a graph DB + Kafka.

**Current milestone: M0 — Storage core** (single-file page store, buffer pool, WAL, control file, crash recovery, durable single-table CRUD).

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
# All unit tests + integration tests
cargo test

# Crash-injection harness only (D7 — 5 injection points)
cargo test --test crash

# Run a specific test by name
cargo test insert_and_get

# With structured log output visible
RUST_LOG=debug cargo test -- --nocapture
```

---

## Run benchmarks

Benchmarks require a release build. Results are written to `target/criterion/`.

```bash
cargo bench
```

This runs three workloads against the engine and reports throughput (ops/s) and latency (p50/p99):

| Workload | Description |
|----------|-------------|
| `insert` | Single-table INSERT at 100 / 1 000 / 10 000 rows |
| `select_point` | Point GET by RowId |
| `update_in_place` | In-place UPDATE by RowId |

Open `target/criterion/report/index.html` to view the HTML report.

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

// Insert a row — returns a stable RowId.
let row_id = engine.insert(b"hello world")?;

// Read it back.
let data = engine.get(row_id)?;
assert_eq!(data, b"hello world");

// Update in-place (new payload must fit in existing slot for M0).
engine.update(row_id, b"updated")?;

// Delete.
engine.delete(row_id)?;

// Checkpoint: flush dirty pages + write checkpoint WAL record + truncate WAL.
engine.checkpoint()?;
```

Add to `Cargo.toml`:

```toml
[dependencies]
unidb = { path = "../unidb" }
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

```
src/
  format.rs      — magic number, version, constants, little-endian helpers
  error.rs       — DbError enum + Result alias
  control.rs     — control file (single source of recovery truth)
  mmap.rs        — sole unsafe module: memory-mapped file wrapper
  page.rs        — slotted-page layout, tuple header, CRC32 on every read
  bufferpool.rs  — fixed frames, clock eviction, WAL-before-page invariant (D5)
  wal.rs         — append-only log, redo+undo payloads, mini-transaction bracketing
  heap.rs        — single-table heap: insert / get / update / delete
  checkpoint.rs  — flush dirty pages → checkpoint record → truncate WAL
  recovery.rs    — ARIES-style redo + undo on open
  lib.rs         — Engine public API, init_tracing()
tests/
  crash/         — crash-injection harness (5 injection points)
benches/
  load.rs        — throughput + latency benchmarks (criterion)
```

---

## Milestone roadmap

| Milestone | Status | Summary |
|-----------|--------|---------|
| **M0 — Storage core** | in progress | Single-file page store, buffer pool, WAL, control file, crash recovery, single-table CRUD |
| M1 — MVCC + CRUD | planned | Transactions, READ COMMITTED / REPEATABLE READ, SQL subset, JSON columns, RLS |
| M2 — Vector & Text search | planned | `VECTOR(n)` type, async HNSW index, `NEAR` operator, full-text inverted index |
| M3 — Graph | planned | Edge records, edge-list index, Cypher subset |
| M4 — Event queue | planned | WAL-derived stream, durable consumer offsets, replay |
| M5 — API / server | planned | Optional REST server + JWT auth + subscribe + `/metrics` |

---

## Design decisions (locked — do not re-open without sign-off)

| ID | Decision |
|----|----------|
| D1 | Buffer policy: steal + no-force, ARIES-style (both redo and undo logging) |
| D2 | Atomic unit is a mini-transaction (WAL-bracketed group of page writes) |
| D3 | Control file holds magic, version, page_size, checkpoint LSN, WAL tail |
| D4 | Tuple header reserves xmin/xmax now; in-place UPDATE in M0, MVCC in M1 |
| D5 | WAL-before-page invariant: no dirty page flushed while page.LSN > durable WAL LSN |
| D6 | Single-file storage for M0 (WAL may be a separate file) |
| D7 | Crash-injection harness: kill at 5 points, reopen, assert recovered state |
| D8 | Page size 8 KiB default, config-overridable at init, fixed after creation |
| D9 | On-disk format is fixed little-endian; every page carries CRC32 + LSN |
| D10 | Default isolation: READ COMMITTED; REPEATABLE READ available |
| D11 | `on_read`/`on_write` seam built now (no-op) for future SSI without executor rewrite |
| D12 | Implement SI abort-on-conflict before RC re-evaluation path |
| D13 | Structured logging (tracing) from day one for WAL writes, checkpoints, recovery |
