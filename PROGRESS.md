# PROGRESS.md

> Milestone completion ledger. One entry per milestone, written when the
> milestone's PR is raised. Each entry records the benchmark **and memory**
> metrics for that milestone. Append newest at the bottom.
>
> Rules & decisions: `CLAUDE.md`. Current working state: `MEMORY.md`.
> Stamp every entry with the **actual current system date**.

---

## How to fill an entry

Copy the template, fill every field, link the PR. The metrics table is
**required** — a milestone is not "done" without recorded throughput + peak
memory (see `CLAUDE.md` §6).

### Entry template

```
## Mx — <name>   [status]   <date>

**PR:** #<n> — <link>
**Summary:** <2–3 sentences on what shipped>

**Benchmarks** (release build, <machine/spec>):

| Workload                     | Throughput (ops/s) | p50 (µs) | p99 (µs) | Peak RSS | Baseline (<what>) |
|------------------------------|--------------------|----------|----------|----------|-------------------|
| <e.g. single-table INSERT>   |                    |          |          |          |                   |
| <e.g. point SELECT by key>   |                    |          |          |          |                   |
| <e.g. UPDATE by key>         |                    |          |          |          |                   |

**Crash harness:** <points covered> — all green / notes
**What changed:** <bullets>
**Known limitations / tech debt:** <bullets>
**Deferred to later milestones:** <bullets>
**Locked-decision changes (if any):** <decision id + human sign-off, or "none">
```

---

## Milestones

## M0 — Storage core   [DONE]   2026-07-06

**PR:** _pending (not yet opened; benchmarks recorded ahead of PR per session workflow)_
**Summary:** Single-file page store, buffer pool, WAL, control file, crash recovery, durable single-table CRUD. No MVCC. Crash-injection harness (D7) and structured logging (D13) included.

**Benchmarks** (release build, Apple Silicon macOS, single-threaded, real fsync per commit):

| Workload               | Throughput (ops/s) | p50 (ms/op) | p99  | Peak RSS  | Baseline (SQLite, PRAGMA synchronous=FULL) |
|-------------------------|--------------------|-------------|------|-----------|---------------------------------------------|
| single-table INSERT    | ~313–323 elem/s    | ~3.10–3.19  | n/a¹ | ~27.8 MB² | ~4,600–4,970 elem/s (~0.21–0.22 ms/op)      |
| point SELECT by key    | ~1.17M elem/s      | 0.000856    | n/a¹ | ~27.8 MB² | ~330K elem/s (~3.04 µs/op, Python driver)³  |
| UPDATE by key          | ~327 elem/s        | ~3.06       | n/a¹ | ~27.8 MB² | ~4,970 elem/s (~0.20 ms/op)                 |

¹ Criterion reports a 95%-CI point estimate, not true p50/p99 percentiles; the
  point estimate is used as a p50 proxy here. Outlier counts were low (2–8%)
  across all runs. A true percentile histogram is deferred to later load-test
  tooling.
² Peak RSS measured via `/usr/bin/time -l` on the `select_point` benchmark
  (1000-row working set); INSERT/UPDATE were not separately RSS-profiled but
  share the same mmap-backed buffer pool, so peak RSS is expected to be
  comparable at this data size.
³ SQLite baseline measured through Python's stdlib `sqlite3` driver (includes
  Python interpreter overhead, ~17 MB baseline RSS) — not a pure C-to-Rust
  comparison, but representative of embedded-engine order of magnitude.

**Analysis:** unidb is ~14–15x slower than SQLite on INSERT/UPDATE, both doing
a real fsync per commit — expected and consistent with `CLAUDE.md` §1/§6: M0
is unoptimized (no group commit, no WAL batching) and the project explicitly
does not aim to beat a specialized incumbent on its home turf. Point SELECT is
in-memory (no fsync) and fast relative to the Python-driver SQLite baseline,
though that comparison is skewed by driver overhead more than engine design.

**Crash harness:** P1 (post-WAL/pre-flush), P2 (mid-checkpoint), P3
(post-mutation/pre-commit), P4 (during WAL truncation), P5 (post-commit-fsync)
— all 6 crash tests green (`committed_rows_survive_after_reopen` plus P1–P5).
**What changed:** initial M0 implementation — all 8 source modules
(`format`, `control`, `page`, `bufferpool`, `wal`, `heap`, `checkpoint`,
`recovery`) plus `lib.rs`'s Engine API and `mmap.rs`'s isolated unsafe block.
**Known limitations / tech debt:** FSM is a linear scan over heap pages;
`Heap`'s page list is in-memory only (rebuilt lazily across reopen); WAL
truncation rewrites the entire file. See `MEMORY.md` for the full list.
**Deferred to later milestones:** MVCC, catalog, SQL subset, JSON/RLS (M1);
group-commit/WAL-batching throughput optimizations are not scheduled — only
relevant if the project pivots toward competitive single-model throughput,
which contradicts §1's stated non-goal.
**Locked-decision changes (if any):** none.

_Baseline note: SQLite is the honest M0/M1 comparison (both embedded, single-file). The replaced-stack benchmark (Postgres + vector + graph + queue) becomes the headline from M2, when cross-domain transactions exist — see `CLAUDE.md` §6._

---

## M1 — MVCC + CRUD   [DONE]   2026-07-06

**PR:** _pending (not yet opened; benchmarks recorded ahead of PR per session workflow)_
**Summary:** Transactional MVCC on top of M0's storage core — READ COMMITTED
default / REPEATABLE READ available (D10), SI's abort-on-conflict conflict
handling (D12), the `on_read`/`on_write` seam for future SSI (D11), a
catalog, and a SQL subset (`CREATE TABLE`/`INSERT`/`SELECT`/`UPDATE`/
`DELETE`) with RLS folded in as a planner rewrite and JSON columns
supporting `->`/`->>` path extraction. Shipped as four internal checkpoints
(M1.a MVCC core, M1.b conflict handling, M1.c catalog+SQL, M1.d hardening).

**Benchmarks** (release build, Apple Silicon macOS, single-threaded, real fsync per commit, 10 samples):

| Workload                                | Throughput (ops/s) | p50 (ms/op) | Peak RSS | M0 comparison       | Baseline (SQLite) |
|------------------------------------------|--------------------|-------------|----------|----------------------|-------------------|
| single-table INSERT (own txn per op)    | ~155–162 elem/s    | ~6.2–6.5    | ~27.0 MB | ~2.0x slower than M0 | ~4,600–4,970 elem/s |
| point SELECT by key (own txn per op)    | ~328 elem/s        | 3.05        | ~27.0 MB | ~3,570x slower¹      | ~330K elem/s (Python driver) |
| UPDATE by key (own txn per op)          | ~154 elem/s        | 6.38        | ~27.0 MB | ~2.1x slower than M0 | ~4,970 elem/s |
| contention: conflict + abort + retry²   | ~65 elem/s         | 15.44       | ~27.0 MB | n/a (new in M1)      | n/a (new in M1) |

¹ **This is the headline finding of M1's benchmark pass, not a red flag to
  paper over.** M0's point SELECT was a pure in-memory read (855ns). M1's
  wraps the same read in `begin()`/`commit()` — and `commit()` unconditionally
  calls `wal.commit_user_txn()`, which fsyncs, even though a read-only
  transaction wrote nothing that needs to become durable. That single
  unnecessary fsync (~3ms) is the entire regression. **Tracked as a real,
  fixable inefficiency** (see Known limitations below), not fixed in M1
  since it wasn't part of the agreed M1 scope.
² New in M1: two "concurrent" (interleaved, single-threaded) transactions
  race for one row; the second aborts immediately per SI (D12) and retries
  against the now-current version. Cost is dominated by 5 fsyncs per cycle
  (2 mini-txn commits + 3 user-txn commits/aborts) — consistent with the
  ~3ms-per-fsync cost observed elsewhere in this table.

**Why INSERT/UPDATE are ~2x slower than M0, not more:** each benchmark
iteration is a *single-statement transaction* (`begin()` → one op →
`commit()`), which is the worst case for M1's overhead — it pays both the
existing per-statement mini-txn fsync (D2, unchanged from M0) **and** a new
per-transaction `WAL_TXN_COMMIT` fsync (M1) on every single operation. A
transaction batching multiple statements before one commit would amortize
the second fsync across all of them and approach M0's original per-op cost
— this benchmark deliberately does not do that, to measure the worst case
honestly rather than flatter the number.

**Crash harness:** P1–P5 (M0), P6/P7 (M1.a, user-txn boundaries), P9 (M1.b,
crash mid-undo) — all 10 crash tests green, plus a new combined crash+MVCC
property test (`property_crash_recovery_reflects_only_committed_transactions`)
running random `BEGIN`/`INSERT`/`COMMIT`/`ROLLBACK` sequences with random
crash points across 6 seeds; recovered state exactly matches the transactions
that reached `WAL_TXN_COMMIT` in every case.

**What changed:** tuple versioning (xmin/xmax/prev-chain), transaction
manager, lock manager, catalog, SQL parser/planner/executor — see `MEMORY.md`
for the full module-by-module breakdown across all four checkpoints.

**Known limitations / tech debt:**
- **Read-only transactions pay a full commit fsync for nothing** (see
  footnote 1) — the fix is straightforward (skip `WAL_TXN_COMMIT`/fsync in
  `TransactionManager::commit` when the transaction's undo log is empty,
  i.e., it never wrote anything) but wasn't in M1's agreed scope. Flagged
  here explicitly so it doesn't get lost.
- No vacuum/GC (dead tuple versions accumulate); no wait queue/deadlock
  detection in the lock manager (deliberate, D12); RC's EvalPlanQual-style
  re-evaluation path is unimplemented; catalog DDL is not transactional.
  See `MEMORY.md`'s "Known issues / tech debt" for the complete, current list.
**Deferred to later milestones:** vector/text search (M2), graph (M3), event
queue (M4), API/server (M5). Group-commit/WAL-batching and the read-only-txn
fsync fix are both real, identified throughput opportunities not scheduled
against a specific milestone yet.
**Locked-decision changes (if any):** none. (`FORMAT_VERSION` 1→2 for the
tuple header extension is a version bump under D9's own rules, not a
re-litigation of a locked decision — no migration path needed since M0 never
shipped externally.)

_Baseline note: SQLite remains the honest M1 comparison (both embedded, single-file). The replaced-stack benchmark (Postgres + vector + graph + queue) becomes the headline from M2, when cross-domain transactions exist — see `CLAUDE.md` §6._

---

## M2 — Vector & Text search   [DONE]   2026-07-06

**PR:** _pending (not yet opened; benchmarks recorded ahead of PR per session workflow)_
**Summary:** `VECTOR(n)` column type, an asynchronous background indexing
worker (the engine's first background thread — `src/index_worker.rs`), an
HNSW vector index (`src/vector.rs`, wrapping `instant-distance`) and a
full-text inverted index (`src/fulltext.rs`), an explicit `CREATE INDEX
... USING HNSW|FULLTEXT` SQL surface, and a `NEAR(column, [...], k)` query
operator with over-fetch-then-filter execution that stays fully
MVCC/RLS-correct. Shipped as four internal checkpoints (M2.a `VECTOR(n)`
foundation, M2.b background worker, M2.c full-text + `CREATE INDEX`, M2.d
`NEAR` + benchmarks).

**Benchmark scope note (§6):** the full four-system "replaced stack"
comparison (Postgres + vector store + graph DB + queue, one cross-domain
transaction touching all four) isn't achievable until M4 completes and all
four data models exist. This entry uses **Postgres 18 + pgvector 0.8.4 as
an interim proxy**, covering just the vector-search slice M2 actually
competes on — confirmed with the user ahead of implementation, not a
silent scope narrowing.

**Benchmarks** (release build, Apple Silicon macOS, single-threaded caller,
128-dim embeddings, `--sample-size 10`; Postgres numbers are server-side
`EXPLAIN ANALYZE`/summed `\timing` execution time, excluding `psql` client
process overhead, for an apples-to-apples comparison against unidb's
in-process cost):

| Workload                                          | unidb            | Postgres 18 + pgvector 0.8.4 |
|----------------------------------------------------|------------------|-------------------------------|
| INSERT 200 rows, 1 txn, **no** vector index         | ~234–241 elem/s (~4.2 ms/row) | ~10,668 elem/s (~0.094 ms/row) |
| INSERT 200 rows, 1 txn, **with** HNSW index active  | ~83–86 elem/s (~11.8 ms/row)  | ~1,916 elem/s (~0.52 ms/row) |
| Index-active overhead vs. no-index                  | ~2.8x slower     | ~5.6x slower |
| `NEAR`/`ORDER BY <->` query, k=5, 300 rows indexed  | ~4.0–5.0 ms      | ~0.43 ms (planner chose seq scan + sort over HNSW at this row count — realistic at small scale) |
| Raw `VectorIndex` upsert, building to 100 points¹   | ~7.7–7.9 ms/point (cumulative) | n/a (internal primitive, no Postgres equivalent) |
| Raw `InvertedIndex` term search, 300 docs           | ~14.2 µs         | n/a (internal primitive) |

¹ `index_primitives/vector_index_upsert_100`: 100 sequential upserts,
  each rebuilding the whole HNSW graph from scratch (see the design note
  below) — the ~781ms total reported by `cargo bench` divided across 100
  points, not a per-op cost at steady state.

**Honest read of these numbers, not a flattering one:**
- unidb's absolute INSERT throughput is far behind pgvector's in both
  configurations. Most of that gap **predates M2 and isn't vector-specific**:
  M1's benchmark pass already found and documented that every statement
  pays a WAL fsync (D2's per-statement mini-txn, unchanged since M0) —
  Postgres's group-commit and OS-level write batching amortize this in a
  way unidb's single-threaded, no-group-commit M0/M1 storage layer does not
  yet. This is tracked, known tech debt (see `MEMORY.md`), not something
  M2 introduced.
- **The vector-specific overhead is real and worth stating plainly**:
  `instant-distance` (the chosen HNSW crate) has no incremental single-point
  insert in its public API — confirmed by reading the vendored source before
  committing to the design, not assumed. `VectorIndex` therefore rebuilds
  its entire graph from scratch on every upsert (M2.b's design note in
  `MEMORY.md`), which is why unidb's index-active INSERT overhead (2.8x)
  doesn't scale to larger datasets the way an incremental HNSW's would —
  this is flagged as real tech debt, not hidden behind the "row write is
  the only synchronous cost" claim, since at 200 rows the cost is already
  measurable even though the rebuild happens off the foreground thread
  (CPU contention between the foreground and worker threads on a
  finite-core machine is the actual mechanism, not a blocking call).
- unidb's `NEAR` latency (~4ms) is dominated by transactional overhead, not
  the vector search itself: every `SELECT` still pays a full
  begin-snapshot/commit round trip (the same read-only-transaction fsync
  inefficiency M1 already found and deferred), while the raw index-search
  primitive underneath resolves in microseconds once that wrapper is
  stripped away (see `index_primitives/fulltext_search`'s ~14µs as a proxy
  for how fast the underlying data structures actually are).
- pgvector's planner chose a sequential scan over its own HNSW index for
  the 300-row `NEAR`-equivalent query — expected, correct behavior at this
  small scale, and left as-is rather than forcing index usage to produce a
  more flattering number.

**MVCC correctness (the single most important test in M2):**
`tests/vector_mvcc.rs::aborted_insert_never_surfaces_in_near_results` —
inserts a row, polls (deterministically, not via a timing guess) until the
background worker has demonstrably indexed it, aborts instead of
committing, then proves a fresh transaction's `NEAR` query never returns
that row. This is the concrete proof that "the index has no concept of
transactions" never leaks into a correctness bug, since `exec_select_near`
re-checks every index-sourced candidate against MVCC visibility through the
same `predicate_matches` path an ordinary scan uses.

**Crash/rebuild correctness:** `tests/index_rebuild.rs` — engine restart
correctly rebuilds both index kinds from committed rows and `NEAR` still
works afterward; a `NEAR` query issued before the worker reports `Ready`
returns a partial (never incorrect, never erroring) result set. No new
crash-injection P-number was added (`tests/crash/main.rs` stays at P1–P9):
the index is derived, rebuildable state with zero WAL footprint by design,
so losing it on crash is expected, not a durability violation.

**What changed:** `ColumnType::Vector(u32)` + hand-rolled row encoding
(tag 5); `src/index_worker.rs` (new, the engine's first background
thread); `src/vector.rs`/`src/fulltext.rs` (new); `LogicalPlan::CreateIndex`
+ `Expr::Near`; `Catalog::set_column_index` (a primitive shared by both the
M2.b Rust API and M2.c's `CREATE INDEX`); `exec_select_near`'s
over-fetch-then-filter execution. See `MEMORY.md` for the full
module-by-module breakdown across all four checkpoints, including two
design corrections found and fixed during implementation (the
`instant-distance` incremental-insert assumption, and a rebuild-on-open gap
that would have silently dropped `FullText` indexes on reopen).

**Known limitations / tech debt (new in M2, on top of M1's carried-forward list):**
- `VectorIndex`/`InvertedIndex` never reclaim entries for rows superseded
  by UPDATE — the same shape of gap as M1's "no vacuum," just for the
  secondary index instead of the heap (correctness is unaffected; it's an
  unbounded space leak under update-heavy workloads on indexed columns).
- No SQL-level full-text query surface — `InvertedIndex::search` exists and
  is tested directly, but only `NEAR` (vector) has a `WHERE`-clause operator
  in M2's scope.
- `instant-distance`'s full-rebuild-per-upsert cost (see benchmark
  discussion above) means unidb's vector-index-active INSERT overhead will
  grow with dataset size in a way a true incremental HNSW would not —
  flagged for a future milestone to revisit if it becomes a real blocker.

---

## M3 — Graph   [DONE]   2026-07-06

**PR:** _pending (not yet opened; benchmarks recorded ahead of PR per session workflow)_
**Summary:** Graph edges — `(from_id, to_id, edge_type, props)` — an
edge-list index by `from_id`, a hand-rolled Cypher subset (`MATCH
(a)-[:TYPE]->(b) WHERE ... RETURN ...`), per-edge write locking, and a
batch-latch adjacency-scan optimization. Shipped as four internal
checkpoints (M3.a edge storage foundation, M3.b locking verification +
batch-latch, M3.c Cypher subset, M3.d MVCC-correctness test + benchmarks).
The headline architectural finding: graph edges needed **zero new
storage-layer or locking code** — they're stored as ordinary rows in a
synthetic `__edges__` system table, and `RecordId::row`'s lock key was
already globally unique across every table in the database. Confirmed
with tests, not just code inspection.

**Benchmark scope note (§6):** as with M2, the full four-system "replaced
stack" comparison isn't achievable until M4 (queue) exists. This entry
uses **Postgres with an indexed adjacency-list table** as the interim
proxy (`CREATE TABLE edges(from_id, to_id, edge_type, props jsonb);
CREATE INDEX ON edges(from_id);`) — the direct "what would you do without
a graph DB" comparison, confirmed with the user ahead of implementation. A
dedicated embedded-graph-engine comparison is deliberately deferred: M3's
actual competitive claim is "one transaction across relational + vector +
graph," not "our traversal beats a purpose-built graph DB's traversal."

**Benchmarks** (release build, Apple Silicon macOS, single-threaded
caller, `--sample-size 10`; Postgres numbers are server-side `EXPLAIN
ANALYZE`/summed `\timing` execution time, excluding `psql` client process
overhead):

| Workload                                          | unidb            | Postgres (indexed adjacency table) |
|----------------------------------------------------|------------------|-------------------------------------|
| INSERT 100 edges, 1 txn                            | ~335.8 ms (~3.36 ms/edge) | ~9.6 ms (~0.096 ms/edge) |
| Adjacency scan, 1,000-edge hot hub — **naive**¹      | ~879 µs          | n/a (comparison baseline is unidb-internal) |
| Adjacency scan, 1,000-edge hot hub — **batched**    | ~94.3 µs         | ~98 µs (Seq Scan — 100% of rows match, planner skips the index) |
| Adjacency scan, 10,000-edge hot hub — **naive**¹     | ~9.06 ms         | n/a |
| Adjacency scan, 10,000-edge hot hub — **batched**   | ~930 µs          | ~568 µs |

¹ "naive" = one `BufferPool::fetch_page` call per candidate `RowId`, the
  pre-M3.b resolution strategy — kept only in `benches/graph.rs` for
  comparison; the shipped path is always the batched resolver.

**Honest read of these numbers:**
- **INSERT lags Postgres by ~35x, and this is not graph-specific.** It's
  the same root cause M1/M2 already found and documented: every statement
  pays a WAL fsync (D2's per-statement mini-txn), and Postgres's
  group-commit amortizes this in a way unidb's current single-threaded,
  no-group-commit storage layer does not yet. Tracked, known tech debt,
  not something M3 introduced.
- **The batch-latch adjacency scan is a genuine, competitive result, not
  just "better than before."** At 1,000 edges, unidb's batched scan
  (94.3 µs) is essentially even with Postgres's Seq Scan (98 µs); at
  10,000 edges it's within ~1.6x (930 µs vs 568 µs). The *naive*
  pre-optimization scan would have lost badly (9x and 16x slower,
  respectively) — so M3.b's batching work is what closes nearly the
  entire read-side gap with a mature, heavily-optimized database, not a
  marginal tweak. This is the clearest evidence yet in this project that a
  measured, targeted optimization (not a rewrite) can make the young
  engine competitive on the workload it's actually built for.
- Postgres's planner chose a sequential scan over its own `from_id` index
  in both cases — expected and correct: every row in the benchmark table
  has the same `from_id` (a single hot hub with no other data), so the
  index has nothing to discriminate. Left as-is rather than forcing index
  usage to manufacture a more flattering number — the same honesty
  standard M2.d's pgvector comparison used.

**MVCC correctness (the single most important test in M3):**
`tests/graph_mvcc.rs` — `EdgeIndex` has no concept of transactions and no
abort-time cleanup hook, so an aborted `create_edge` leaves a permanently
stale entry in the index. The test creates an edge, confirms
self-visibility from the *same* transaction (proving the index really
does have the entry, not a vacuous check), aborts instead of committing,
then proves a fresh transaction's `edges_from` *and* an equivalent Cypher
`MATCH` query both never return it. Unlike M2's `vector_mvcc.rs`, no
poll-before-abort dance is needed: `EdgeIndex` is synchronous (M3.a/M3.b —
no background worker to race), so there's no "did it catch up yet"
question to resolve first.

**Crash/rebuild correctness:** `tests/graph_rebuild.rs` — engine restart
correctly rebuilds the edge-list index from committed rows (no
`wait_for_ready` polling needed, unlike M2's async-worker-backed indexes —
a real simplification of the test itself, not just the implementation);
deletes are correctly reflected after reopen; Cypher queries work
immediately post-rebuild. No new crash-injection P-number: edges are
ordinary WAL-backed heap rows already covered by `tests/crash/main.rs`'s
P1–P9; only the edge-list index is derived/rebuildable state.

**Locking correctness:** `tests/graph_locking.rs` confirms — with tests,
not just code review — that per-edge write locking needed **zero new
code**. `RecordId::row(page_id, slot)` already produces a globally-unique
lock key across every table in the database, since `PageId` is allocated
from one shared `BufferPool`, not per-table. No `RecordKind::GraphEdge`
variant was added.

**What changed:** `src/graph/` (new module: `edges.rs`, `index.rs`,
`logical.rs`, `parser.rs`, `executor.rs`); `Engine::create_edge`/
`delete_edge`/`edges_from`/`execute_cypher`; `Catalog`/`Heap`/`LockManager`
reused entirely as-is (zero changes); `sql::executor::predicate_matches`/
`eval_expr` promoted from private to `pub(crate)` — the one deliberate
cross-module touch, enabling the Cypher executor to reuse the SQL layer's
expression evaluator verbatim instead of duplicating it. See `MEMORY.md`
for the full module-by-module breakdown across all four checkpoints,
including the two design corrections found and confirmed during
implementation (no `RecordKind::GraphEdge` needed; `ExecCtx` stays
untouched, with `edge_index` passed as an explicit extra argument instead).

**Known limitations / tech debt (new in M3, on top of M1/M2's
carried-forward list):**
- **`EdgeIndex` has no abort-time (or update-time) cleanup** — an aborted
  or logically-superseded edge's index entry is never retracted, an
  unbounded space leak under abort/update-heavy workloads on indexed
  `from_id`s. Correctness is unaffected (proven by `tests/graph_mvcc.rs`);
  this is the same shape of gap as M2's `VectorIndex`/`InvertedIndex`
  "no cleanup" tech debt, and M1's "no vacuum" gap before that.
- **No Cypher `CREATE`/`DELETE` mutation surface** — the locked v1 grammar
  (`MATCH ... WHERE ... RETURN`) is read-only; `create_edge`/`delete_edge`
  are Rust-API-only, mirroring M1's `set_rls_policy` and M2's
  `set_column_index` precedent of "Rust API now, SQL/query surface later
  if needed."
- **Nodes are opaque `i64` IDs only** — no `:label` node-type declarations,
  no property-graph joins to a backing table (`a.name` is rejected with a
  clear parse-time error). Confirmed scope decision, not an oversight; a
  property-graph join model is a natural future extension once a real
  workload demands it.
- **Composite/multi-hop Cypher patterns are out of scope** — v1 supports
  exactly one fixed-length directed hop; no `OPTIONAL MATCH`, no
  variable-length paths (`*1..3`), no aggregation, no `ORDER BY`/`LIMIT`.

## M4 — Event queue   [PLANNED]
_WAL-derived stream; durable consumer offsets; replay; slow-consumer durability contract resolved._

## M5 — API / server   [PLANNED]
_Embedded crate stabilized; optional REST + JWT auth + subscribe + `/metrics`._
