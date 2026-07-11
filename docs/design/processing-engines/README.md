# unidb — Processing Engines: Detailed Design Documents

> A deep-dive design reference for every processing engine in unidb: architecture,
> data structures at each level, algorithms, border cases, performance work,
> parallelism, metrics, and the forward roadmap. Distilled from the code as shipped
> (see `MEMORY.md` for live state); when this collection and
> `CLAUDE.md`/`PROGRESS.md` disagree, those win.
>
> Diagrams are [Mermaid](https://mermaid.js.org/) — rendered natively by GitHub.

**Scope snapshot:** engine as of 2026-07-11 (`FORMAT_VERSION` 5, crash harness 29
points, Phases 1–6 + Milestone P shipped).

---

## Index

| # | Document | Engine / concern | What it covers |
|---|----------|------------------|----------------|
| 1 | [Architecture overview](01_architecture_overview.md) | Whole system | Layer stack, engine inventory, one-commit multi-model flow, data structures by level, trust & failure model |
| 2 | [Storage engine](02_storage_engine.md) | Pages · buffer pool · heap | 8 KiB slotted pages, CRC+LSN, mmap-as-storage, CLOCK eviction, D5 enforcement, per-page latches, durable FSM, atomic heap grow, large objects |
| 3 | [WAL & crash recovery](03_wal_and_recovery.md) | Durability | Record wire format, mini-transactions, segments, group commit, fsync poisoning, ARIES-style redo/undo, FPI torn-page repair, the 29-point crash matrix |
| 4 | [Transaction engine (MVCC)](04_transaction_engine.md) | Concurrency control | Snapshots & visibility, isolation levels (RC/RR/SSI), lock manager & deadlock detection, vacuum + autovacuum, the index-aliasing gate |
| 5 | [SQL query engine](05_sql_query_engine.md) | Query processing | Parser → logical → cost-based optimizer (Selinger DP) → executors; joins (hash/Grace, merge, index-NL), spills, decode pushdown, EXPLAIN |
| 6 | [Indexing engines](06_indexing_engines.md) | Secondary indexes | DiskBTree node format, latch-crabbing writes, latch-free reads, WAL coalescing, duplicate-key fix, full-text inverted index, FSM-as-BTree |
| 7 | [Vector engine](07_vector_engine.md) | Similarity search | HNSW history and why it was retired, durable IVF-Flat layout, k-means training, NEAR path, recall results |
| 8 | [Graph engine](08_graph_engine.md) | Edges & traversal | `__edges__` system table, durable adjacency index, Cypher subset, batch-latch resolution, the CSR self-visibility bug story |
| 9 | [Event queue engine](09_event_queue_engine.md) | Streaming | Synchronous transactional event capture, Kafka-style manual offsets, SSE subscribe, slow-consumer vs. vacuum contract |
| 10 | [Parallelism & performance](10_parallelism_and_performance.md) | Throughput | Group commit, parallel scan workers, partial aggregates, concurrent SQL writes, read-path wins; full benchmark & metrics analysis |
| 11 | [Server, replication & operations](11_server_replication_operations.md) | Ops surface | REST API, JWT/roles/audit, `/metrics` & `/stats`, WAL shipping, replicas & failover, backup/PITR, runbook pointers |
| 12 | [Future roadmap](12_future_roadmap.md) | Direction | Promising development goals, proposed milestones and phases, graduation criteria, risk register |

---

## How to read this collection

- **New to the codebase?** Read doc 1, then docs 2–4 in order — everything else
  sits on the storage/WAL/MVCC triad.
- **Reviewing durability claims?** Docs 2 + 3 and the crash matrix in doc 3.
- **Evaluating the multi-model thesis?** Docs 1, 7, 8, 9 and the benchmark
  analysis in doc 10.
- **Planning future work?** Doc 12, cross-referenced against
  `docs/backlog/README.md`.

## Conventions used throughout

- **Locked decisions** are cited as `D1`–`D13` (see `CLAUDE.md §3`); phases and
  milestones as `P1.a`, `M10`, `P5.e` etc. (see `PROGRESS.md`).
- Code references are `file.rs` paths relative to `src/` unless noted.
- All on-disk integers are **little-endian** (D9). "Page" means the 8 KiB (D8)
  unit unless a different size is explicit.
- **Border cases** sections list the failure/edge conditions each engine handles
  explicitly, with the mechanism that handles them.
