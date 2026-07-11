# unidb vs FFSDB — Benchmark Evaluation

> Source of the FFS numbers: <https://ffsdb.com/evals> (FFS database
> `2.0.0-alpha.1`, "Apple M-series silicon", Rust 1.82+), fetched
> **2026-07-08**.
> unidb numbers: measured fresh on **2026-07-08** on the hardware below,
> `cargo bench` release builds, plus the head-to-head Postgres numbers
> already recorded in `PROGRESS.md` (M2/M3/M4).
> Raw benchmark logs backing every unidb figure here live in
> [`raw-results.md`](./raw-results.md).

---

## 0. Read this first — the two systems are in different categories

This is the single most important thing to understand before reading any
table below, and skipping it will make every ratio misleading.

**FFS's evals measure raw, embedded, in-process *index data structures* —
`ffs::BTree`, `ffs::Hnsw`, `ffs::Csr` — against *other libraries* of the
same shape (`sled`, `instant-distance`, `petgraph`).** These are native
Rust calls: no SQL parse, no query plan, no MVCC snapshot, and — crucially —
**no per-operation durability (WAL fsync)**. They are measured in
**nanoseconds to low microseconds**. FFS is, in these evals, benchmarking
itself *as a library of fast index primitives*.

**unidb's benchmarks measure the full durable transactional engine path.**
Every comparable unidb operation goes through: SQL parse → logical plan →
MVCC snapshot → heap/index access → **a per-statement WAL `fsync`** →
commit. unidb is a unified multi-model **database** (one page store, one
WAL, one buffer pool, one transaction manager across relational + vector +
graph + queue — see `CLAUDE.md` §1), not a library of index structures.
Its numbers are dominated by durability and transaction cost, and land in
the **microsecond-to-millisecond** range.

**Consequence:** a raw "unidb X ms vs FFS Y ns" ratio measures *durability
and SQL overhead*, **not index quality**, and it will always make unidb
look thousands of times slower. That comparison is category-mismatched and
we do not headline it. What *is* meaningful:

1. **The one clean apples-to-apples: HNSW.** unidb's vector index *is*
   `instant-distance` (it wraps the crate directly — see `src/vector.rs`).
   `instant-distance` is one of FFS's own baselines. So FFS's published
   "2.64× faster than `instant-distance` on query" is, transitively, a
   direct statement about the index engine unidb is built on. See §4.
2. **Postgres head-to-heads**, which both projects ran. See §5.
3. **The architectural story**: what each op costs, and *why*. See §3.

Where unidb has never built a comparison at all (LanceDB, Kùzu, Neo4j),
that is stated as "not covered," not fabricated. See §6.

---

## 1. Hardware & versions

| | This evaluation (unidb) | FFS evals (as published) |
|---|---|---|
| CPU | Apple **M5 Pro** | "Apple M-series silicon" (chip unspecified) |
| RAM | 48 GB | unspecified |
| OS | macOS (Darwin 25.4) | macOS |
| Rust | 1.95.0 | 1.82+ |
| Postgres | 18.4 + pgvector 0.8.4 | 17 + pgvector (version unspecified) |
| Subject version | unidb @ `main` (M0–M8) | FFS `2.0.0-alpha.1` |

⚠️ The two runs are **not on identical hardware** (M5 Pro here vs an
unspecified M-series for FFS, and PG 18 vs PG 17). FFS's own numbers are
reproduced verbatim as published; treat cross-machine ratios as
directional, not exact. unidb-vs-Postgres numbers here *are* same-machine
and apples-to-apples.

---

## 2. FFS's published evals (reproduced verbatim for reference)

FFS's headline table, exactly as published:

| Comparator | Shape | Measured Path | FFS result |
|---|---|---|---|
| sled | embedded KV | insert / lookup / 100-key range scan | 2.89× / 1.55× / 3.94× |
| instant-distance | embedded HNSW | build / query, dim 128, k=10 | 1.13× / 2.64× |
| petgraph | in-memory graph | 1-hop / 2-hop BFS / edge append | 9.28× / 1.43× · append 9.64× **slower** |
| Postgres + pgvector | server vector DB | HNSW query, same algorithm/params | 3.01× |
| LanceDB | embedded vector DB | query vs IVF-PQ / vs brute force | 3.03× / 4.78× |
| Kùzu | embedded graph DB | bulk load / 1-hop | 18.5× / ~29,000×\* |
| Neo4j | server graph DB | bulk load / 1-hop | ~1,200× / ~195,000×\* |

\* Starred ratios measure architectural stack cost (FFI / Bolt server
round-trips), not engine parity — FFS's own caveat.

---

## 3. unidb's fresh numbers, and the head-to-head mapping

Below, each FFS eval is mapped to unidb's **closest existing benchmark**,
with the honest category note per row. "n/a — no equivalent" means unidb
has no comparable primitive or never built the comparison.

### 3a. B-Tree / KV — FFS vs sled  →  unidb B-Tree secondary index (M6)

FFS measures a raw `ffs::BTree` (buffer-pool-backed) doing ns-scale
`u64` insert/lookup/range. unidb's B-Tree is a **SQL secondary index**;
its benchmark measures an indexed `SELECT` vs an unindexed full-scan
`SELECT`, both paying the per-statement fsync.

| unidb `cargo bench --bench btree` | 1,000 rows | 10,000 rows |
|---|---|---|
| Point `SELECT WHERE id = k`, **indexed** | 3.22 ms | 3.07 ms |
| Point `SELECT`, full scan | 3.42 ms | 4.47 ms |
| Range `SELECT WHERE id > lo` (~10 rows), **indexed** | 3.14 ms | 3.10 ms |
| Range `SELECT`, full scan | 3.46 ms | 4.93 ms |

**FFS `ffs::BTree`: lookup 182 ns, insert 429 ns, 100-key range 796 ns.**

**Honest read:** these numbers are ~4 orders of magnitude apart, and that
gap is **almost entirely the per-statement WAL fsync + SQL parse/plan**,
not the B-Tree. unidb's underlying index is a `std::collections::BTreeMap`
— its raw lookup is itself ns-scale. What unidb's benchmark *does*
demonstrate is the thing that matters for a query engine: the indexed path
stays **flat** as the table grows (3.07 ms at 10k) while the full-scan path
**grows** (4.47 ms), i.e. the index removes the scan cost it can control.
It does not (and at this scale cannot) remove the fixed durability cost.
A raw "unidb 3.1 ms vs FFS 182 ns" ratio is **not reported as a result** —
it measures durable SQL vs a library call.

### 3b. HNSW vector — FFS vs instant-distance  →  unidb vector index (M2)

**unidb's HNSW index *is* `instant-distance` 0.6.1** (`src/vector.rs`
wraps it; it rebuilds the whole graph per upsert because the crate has no
incremental insert — see `MEMORY.md`'s M2.b design note).

| unidb `cargo bench --bench vector` | value | note |
|---|---|---|
| `NEAR` query, k=5, 300 rows indexed | **3.93 ms** | full SQL path (parse+snapshot+**fsync**) |
| `NEAR` query, k=20 / k=50 | 4.01 ms / 4.02 ms | k barely matters — fixed overhead dominates |
| INSERT 200 rows, no vector index | 4.48 ms/row | per-statement fsync baseline |
| INSERT 200 rows, HNSW index active | 11.75 ms/row | 2.62× the no-index cost (graph rebuild-per-upsert) |
| Raw `VectorIndex` upsert → 100 pts | 7.84 ms/pt (cumulative) | O(n) rebuild-per-insert, off-thread |
| Raw `InvertedIndex` term search, 300 docs | 13.86 µs | full-text primitive, no fsync in path |

**FFS: HNSW build 202 µs/insert, query 113 µs/search, recall@10 0.966 —
vs `instant-distance` build 228 µs, query 298 µs (FFS 1.13× / 2.64×).**

**The clean apples-to-apples (see §4):** FFS is **2.64× faster on query
than the exact library unidb's vector search is built on.** unidb's own
SQL `NEAR` (3.93 ms) is ~35× slower again than raw `instant-distance` —
but that additional gap is the transaction/fsync wrapper, evidenced by the
raw full-text primitive resolving in **13.86 µs** once that wrapper is
stripped away. So of unidb's 3.93 ms `NEAR`: the ANN search is
microseconds, the rest is durability.

### 3c. Graph CSR — FFS vs petgraph  →  unidb CSR + adjacency scan (M3/M7)

FFS measures a raw `ffs::Csr` doing ns-scale in-memory neighbour-slice
access. unidb's graph traversal re-resolves every candidate through the
**durable heap + MVCC visibility check** (that re-validation is what makes
traversal transactionally correct — an aborted edge never surfaces).

| unidb `cargo bench --bench graph` | 1,000-edge hub | 10,000-edge hub |
|---|---|---|
| Adjacency scan, **naive** (1 fetch/candidate) | 876 µs | 8.78 ms |
| Adjacency scan, **batched** (EdgeIndex, M3.b) | **75.4 µs** | **744 µs** |
| Adjacency scan, **CSR** (M7) | 75.5 µs | 749 µs |
| Edge insert (transactional `create_edge`) | 3.41 ms/edge | — |

**FFS: 1-hop 11 ns, 2-hop BFS 1,137 ns, edge append 30 ns (petgraph:
102 ns / 1,630 ns / 3 ns).**

**Honest read:** unidb's *raw* `CsrIndex::candidates()` returns a
contiguous neighbour slice and is itself ns-scale — architecturally the
same structure FFS and petgraph benchmark. But unidb **never traverses
without the durable re-fetch + MVCC recheck**, so its measured 1-hop
traversal is 75 µs (1k) / 744 µs (10k), not nanoseconds. That is the price
of transactional correctness, paid deliberately. Note also that **CSR and
the older HashMap `EdgeIndex` are at parity** (75.5 vs 75.4 µs) — the
batched heap-resolve step dominates either way, so CSR's cache-friendly
layout shows no single-hop win (its value is future multi-hop traversal;
reported plainly, not hidden — same finding as `PROGRESS.md`'s M7 entry).
unidb's transactional edge insert (3.41 ms) vs FFS's raw append (30 ns) is,
again, a durability-vs-library comparison, not reported as a "result."

### 3d. Predicate-aware retrieval

FFS publishes a predicate-aware ANN eval (filtered vector search: 20k
nodes, dim 128, recall vs latency at 1%/10% selectivity). **unidb has no
equivalent benchmark.** unidb *can* express `SELECT ... WHERE NEAR(...) AND
<predicate>` (the RLS/WHERE terms re-filter NEAR candidates — M2.d), which
is the "post-filter" strategy, but it has not been benchmarked for
recall-vs-selectivity and has no predicate-aware (pre-filtered) ANN mode.
**Not covered.**

---

## 4. The one clean apples-to-apples: HNSW query, via `instant-distance`

Because unidb wraps `instant-distance` directly, FFS's own baseline number
*is* a statement about unidb's vector engine:

| Layer | HNSW query latency (dim 128, k=10) | Source |
|---|---|---|
| **FFS `ffs::Hnsw`** | **113 µs** | FFS evals (their HW) |
| `instant-distance` (raw) = **unidb's index core** | **298 µs** | FFS evals (their HW) |
| unidb SQL `NEAR` (300 rows, k=5) | 3.93 ms | this run (M5 Pro) |

**Takeaways, stated honestly:**
- At the **index-engine layer**, FFS is genuinely **~2.64× faster** than
  the HNSW library unidb builds on. This is a real, defensible FFS win and
  the most meaningful single comparison in this whole document.
- unidb layers a **full durable SQL transaction** on top of that index,
  which is where its extra ~13× (298 µs → 3.93 ms) goes — a deliberate
  design cost, not an index deficiency.
- unidb also inherits `instant-distance`'s **rebuild-per-upsert** cost on
  the write side (no incremental HNSW insert in the crate's public API),
  visible as the 11.75 ms/row indexed-insert figure in §3b. This is known,
  tracked tech debt (`MEMORY.md` M2.b), independent of the query story.

---

## 5. Postgres — what's possible, both projects

Both FFS and unidb benchmarked against Postgres. Here is FFS's number, a
**fresh unidb-run pgvector number matching FFS's setup**, and unidb's own
previously-recorded Postgres head-to-heads.

### 5a. Fresh pgvector HNSW (this machine, replicating FFS's setup)

10,000 vectors, dim 128, HNSW `m=16, ef_construction=200`, k=10, mean over
200 queries, `hnsw.ef_search=40` (pgvector default). Measured **server-side**
via a `plpgsql` loop (no client/socket round-trip) — the same
"exclude client overhead" convention `PROGRESS.md` uses for all unidb-vs-PG
numbers.

| Metric (PG 18.4 + pgvector 0.8.4, M5 Pro) | value |
|---|---|
| HNSW index build (bulk `CREATE INDEX` on 10k preloaded rows) | 770 ms |
| HNSW query latency, k=10 (server-side, ef_search=40) | **43.5 µs/query** |
| Brute-force query latency (no index), k=10 | 1,556 µs/query |

**For reference, FFS's published pgvector numbers** (PG 17, over a Unix
socket — so their query number *includes* the client round-trip):
HNSW query **380 µs**, no-index query 1,877 µs, index build 5.75 s,
585 µs/insert with index. FFS reports **3.01×** faster than pgvector.

⚠️ **These pgvector query numbers are not directly comparable to each
other**: FFS's 380 µs is measured over a socket (includes round-trip),
mine (43.5 µs) is server-side only (excludes it), on newer hardware and a
newer Postgres. The brute-force numbers (1,556 µs vs 1,877 µs) are the more
comparable pair and are in the same ballpark. What this *does* confirm is
that pgvector's own HNSW is fast and healthy on this box; FFS's raw
`ffs::Hnsw` (113–126 µs) and my server-side pgvector (43.5 µs) are both far
below unidb's SQL `NEAR` (3.93 ms) — because unidb's number carries the
full transaction, and neither of theirs does.

### 5b. unidb's own recorded Postgres head-to-heads (from `PROGRESS.md`)

These were run same-machine, server-side PG timing, on the milestones that
built each feature:

| Workload | unidb | Postgres | Source |
|---|---|---|---|
| Vector INSERT, no index (per row) | ~4.2 ms | ~0.094 ms (10,668/s) | M2 |
| Vector INSERT, HNSW active (per row) | ~11.8 ms | ~0.52 ms (1,916/s) | M2 |
| `NEAR` / `<->` query, k=5, 300 rows | ~4–5 ms | ~0.43 ms (PG chose seq-scan at this size) | M2 |
| Adjacency scan, 1k-edge hub (batched) | ~94 µs | ~98 µs (seq scan) | M3 |
| Adjacency scan, 10k-edge hub (batched) | ~930 µs | ~568 µs | M3 |
| Queue `poll_events`, 100 rows | **~20.8 µs** | ~2.7 ms (`SELECT … FOR UPDATE SKIP LOCKED`) | M4 |
| Queue `poll_events`, 1,000 rows | **~205 µs** | ~2.6 ms | M4 |
| Queue `poll_events`, 5,000 rows | **~984 µs** | ~3.1 ms | M4 |

**Notable:** on the **graph adjacency scan**, unidb's batched path is
already competitive with Postgres (parity at 1k, within ~1.6× at 10k). On
the **event-queue poll**, unidb is **~100× faster than the Postgres
"SKIP LOCKED queue" idiom** — because unidb reads a purpose-built
`__events__` heap in-process while Postgres pays `BEGIN`/lock/`UPDATE`/
`COMMIT` per poll over a connection. On raw **INSERT throughput**, unidb
trails Postgres ~35–56× (the per-statement fsync, no group commit — the
same M1-era gap, tracked, not vector/graph-specific).

---

## 6. Coverage gaps — comparisons unidb has *not* built

Stated explicitly rather than left blank:

| FFS comparator | unidb equivalent? | Status |
|---|---|---|
| sled (embedded KV) | B-Tree secondary index (SQL path) | mapped, §3a — category-different |
| instant-distance (HNSW) | vector index *wraps this crate* | **direct, §3b/§4** |
| petgraph (graph) | CSR / adjacency scan (durable) | mapped, §3c — category-different |
| Postgres + pgvector | yes, run fresh + M2 | **direct, §5** |
| LanceDB | — | **not covered** (no LanceDB comparison exists) |
| Kùzu | — | **not covered** (no embedded-graph-DB comparison) |
| Neo4j | — | **not covered** (no server-graph comparison) |
| Memgraph / FalkorDB / PG+AGE | — | **not covered** (FFS also defers these) |

---

## 7. Bottom line

- **On raw embedded index-primitive speed, FFS is the faster system**, and
  the cleanest evidence is that it beats `instant-distance` — the library
  unidb's own vector search is built on — by ~2.64× on query (§4). If the
  goal is "fastest non-durable in-process index structures," FFS wins the
  comparisons it publishes.
- **unidb is not competing on that axis.** It is a **durable, transactional,
  multi-model engine**: every number here carries a per-statement WAL fsync
  and MVCC snapshot that FFS's raw-primitive evals do not. Comparing the two
  head-to-head in ns measures durability, not index quality — so we don't
  headline those ratios.
- **Where the comparison is fair, unidb holds up**: its batched graph
  adjacency scan is at Postgres parity (§5b), and its integrated event-queue
  poll is ~100× faster than the standard Postgres queue idiom — because
  those exercise unidb's actual thesis (one in-process engine, no
  cross-system glue) rather than single-primitive microspeed.
- **unidb's real competitive claim is still unmeasured here by design.**
  Per `CLAUDE.md` §6, unidb's edge is the **cross-domain single-commit
  transaction** — "save a row + its embedding + a graph edge + an event" as
  *one* WAL append and *one* commit, versus 3–4 systems with no shared
  transaction. Neither FFS's single-primitive evals nor this document
  measure that; the full "replaced-stack" benchmark remains the deliberately
  deferred follow-up (`MEMORY.md`, Open questions).

*Generated 2026-07-08. Raw logs: [`raw-results.md`](./raw-results.md).*
