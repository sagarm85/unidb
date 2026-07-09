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

## M4 — Event queue   [DONE]   2026-07-06

**PR:** _pending (not yet opened; benchmarks recorded ahead of PR per session workflow)_
**Summary:** A WAL-derived event stream, durable consumer offsets
(`poll_events`/`ack_events`, Kafka-style manual-commit split), and an
explicit `vacuum_events` reclaim path. Shipped as four internal
checkpoints (M4.a event capture foundation, M4.b poll/ack, M4.c vacuum +
durability-contract proof, M4.d MVCC/crash correctness + benchmarks). The
headline architectural finding: a naive design tailing the live WAL
directly is a dead end — WAL records carry no table identifier and
`checkpoint.rs::run()` truncates unconditionally with zero
reader-awareness. The actual resolution is to copy events into an
ordinary, durable `__events__` heap table **at write time**, synchronously,
under the writing transaction's own xid, exactly like `__edges__` (M3):
this decouples event retention from WAL retention structurally, so a slow
consumer can never block WAL truncation — it can only make `__events__`
grow until an explicit `vacuum_events()` call reclaims what every
registered consumer has acknowledged past. `tests/queue_vacuum.rs`'s
`wal_truncation_is_unaffected_by_consumer_lag` proves this with a real
test, not just an inference from reading `checkpoint.rs`.

**Benchmark scope note (§6):** per a decision confirmed with the user
ahead of implementation, M4's own benchmarks stay queue-scoped (event
capture overhead, `poll_events` latency, `vacuum_events` cost). The full
four-system "replaced stack" showcase (Postgres + pgvector + a graph DB +
a message queue, one unidb transaction vs. dual/triple-write with no
shared transaction) is now *possible* for the first time since all four
data models exist, but is explicitly deferred as a separate, dedicated
follow-up — standing up a graph DB and/or message queue for a fair
comparison is a materially bigger lift than reusing the Postgres instance
already running locally, which is all M1–M4's own benchmarks needed. This
entry uses **Postgres with a `SELECT ... FOR UPDATE SKIP LOCKED`
queue-shaped table** as the interim, queue-specific proxy — the standard
"poor man's queue" idiom, confirmed with the user ahead of implementation.

**Benchmarks** (release build, Apple Silicon macOS, single-threaded
caller, `cargo bench --sample-size 10`; Postgres numbers are `psql
\timing` wall-clock time for the full statement sequence shown, against an
isolated, dropped-after-use database):

| Workload                                                    | unidb              | Postgres (SKIP LOCKED queue table) |
|---------------------------------------------------------------|--------------------|-------------------------------------|
| INSERT 100 rows, 1 txn, events **disabled**                   | ~345.3 ms (~3.45 ms/row) | ~6.2 ms (~0.062 ms/row)¹ |
| INSERT 100 rows, 1 txn, events **enabled**                     | ~665.1 ms (~6.65 ms/row) | n/a (no Postgres equivalent to a second synchronous system table write) |
| Event-capture overhead vs. events disabled                    | ~1.93x slower      | n/a |
| `poll_events`, `__events__` has 100 rows                      | ~20.8 µs           | ~2.7 ms (`BEGIN`+`SELECT ... FOR UPDATE SKIP LOCKED LIMIT 10`+`UPDATE`+`COMMIT`)² |
| `poll_events`, `__events__` has 1,000 rows                    | ~205.1 µs          | ~2.6 ms² |
| `poll_events`, `__events__` has 5,000 rows                    | ~983.7 µs          | ~3.1 ms² |
| `vacuum_events`, reclaiming 100 rows                          | ~309.9 ms (~3.10 ms/row) | n/a (internal primitive) |
| `vacuum_events`, reclaiming 1,000 rows                        | ~3.064 s (~3.06 ms/row)  | n/a |
| `vacuum_events`, reclaiming 5,000 rows                        | ~15.34 s (~3.07 ms/row)  | n/a |

¹ Warm-run number (a `DO` block executing 100 individual `INSERT`s inside
  one transaction, one `COMMIT` fsync total) — a cold first run measured
  ~42.3 ms, most likely first-execution PL/pgSQL compilation cost, not
  reported as the headline number since it isn't representative of
  steady-state.
² Includes a full dequeue-and-acknowledge cycle (`SELECT ... FOR UPDATE
  SKIP LOCKED` + `UPDATE ... SET claimed = true` + `COMMIT`), not a pure
  read — `poll_events` alone is a pure read with no write or fsync, so this
  is not a like-for-like comparison of *durability* cost, only of *how
  `seq`/lock-based candidate selection scales with table size*, which is
  what this row is actually measuring (see note below). A partial index
  (`CREATE INDEX ... ON queue_events (seq) WHERE NOT claimed`) keeps
  Postgres's candidate selection cost flat regardless of table size.

**Honest read of these numbers:**
- **`poll_events`'s cost scaling with `__events__`'s total size, not
  consumer lag, is real and precisely linear**: 100→1,000 rows is a 10x
  size increase for a 9.9x time increase (20.8µs→205.1µs); 1,000→5,000 is
  a 5x size increase for a 4.8x time increase (205.1µs→983.7µs) — as
  predicted by the "no predicate pushdown, full `heap.scan`" design
  documented in `queue/mod.rs`'s module doc, not merely asserted. Postgres
  stays flat (~2.6–3.1 ms) across the same size range because its partial
  index (`WHERE NOT claimed`) bounds candidate selection to unclaimed rows
  regardless of table size — the same effect a future `seq`-ordered
  secondary index on `__events__` would need to replicate `poll_events`'s
  own scaling. This is the single clearest, most concrete argument for why
  `vacuum_events` (M4.c) matters for more than storage: it's the *only*
  lever that currently bounds `poll_events`'s latency, since there's no
  index to do it structurally yet.
- **`vacuum_events`'s cost is dominated by the same per-statement-fsync
  root cause M1/M2/M3 already found and documented, not anything
  queue-specific**: reclaiming N rows costs a remarkably consistent
  ~3.06–3.10 ms/row regardless of N (100, 1,000, or 5,000), because each
  reclaimed row's `heap.delete` is its own WAL-bracketed mini-txn (D2) that
  fsyncs independently — `vacuum_events` doesn't batch these into fewer
  fsyncs, the same gap already tracked for every other multi-row mutation
  path in this codebase.
- **The events-enabled vs. disabled INSERT ratio (~1.93x) lands almost
  exactly at the 2x the design predicts**: `send_event_capture` performs
  one *additional* independent, fsync-bearing `heap.insert` per row (M4.a)
  — doubling the fsync count for the same row count should double the
  wall-clock cost, and it does, within a few percent (the shortfall from
  an exact 2.0x is most likely fixed per-iteration overhead — engine open,
  table creation — amortized across only 100 rows).
- **unidb's raw INSERT throughput trails Postgres's by ~5.6x even with
  events disabled (345.3ms vs. ~6.2ms warm for the same 100-row, one-user-
  transaction workload)** — smaller than M1's ~30x point-INSERT gap
  because this workload amortizes across *one* transaction rather than one
  per row, but the root cause is identical and already tracked: D2's
  per-statement mini-txn still fsyncs on every individual `INSERT`
  regardless of the surrounding user transaction, where Postgres's single
  `DO` block only pays one `COMMIT` fsync for all 100 statements. Not a
  new finding — restated here because this is the first time the gap is
  measured for a workload where the outer transaction batches many
  statements, which shrinks (but does not close) it relative to M1's
  worst case.

**MVCC correctness:** `tests/queue_mvcc.rs` — event capture is synchronous
(M4.a, a durable `heap.insert` under the writing transaction's own xid),
so unlike M2's background-worker index there is no "did the worker catch
up yet" race to prove away. What the test proves instead: an inserting
transaction sees its own uncommitted event via `poll_events` (self-
visibility, confirming the row genuinely exists pre-abort), and after
`abort()` a fresh transaction's `poll_events` never returns it. A second
test closes a gap unique to M4's design: an aborted `ack_events` call must
not durably advance the offset — proven by acking mid-transaction (self-
visible), aborting, then confirming a fresh transaction's `poll_events`
still returns every event from before the acked-then-aborted point.

**Crash correctness:** no new crash-injection P-number — event rows are
ordinary WAL-backed heap rows using the exact same mini-txn/user-txn
machinery every other row already uses (`tests/crash/main.rs`'s P1–P9
already cover the underlying mechanism). One new dedicated test,
`incomplete_user_txn_leaves_no_trace_across_two_tables`, closes a gap no
prior milestone's crash suite exercised: a transaction that inserts into
both a triggering table and (via `send_event_capture`) `__events__`, then
never reaches `WAL_TXN_COMMIT`, must leave **no trace in either table**
after reopen — proving recovery's incomplete-user-txn undo pass walks the
whole undo log regardless of which table each entry belongs to, not just
the first one it encounters.

**Durability-contract correctness (the milestone's central claim):**
`tests/queue_vacuum.rs`'s `wal_truncation_is_unaffected_by_consumer_lag`
registers a consumer that never acks, forces five explicit `checkpoint()`
calls (WAL truncations) while generating events, and confirms every event
is still fully present and `poll_events`-able afterward — the actual proof
that a slow consumer cannot block or lose data from WAL truncation, not an
inference from code review. `slow_consumer_survives_vacuum_fast_consumer_
does_not_block_it` additionally confirms `vacuum_events` bounds reclaim to
`min(offsets)` across *all* registered consumers, not just the fastest
one.

**What changed:** `src/queue/` (new module: `mod.rs`, `payload.rs`);
`Engine::enable_events`/`poll_events`/`ack_events`/`vacuum_events`;
`TableDef.events_enabled` (`#[serde(default)]`, mirroring `ColumnDef.
index`'s M2.a introduction) + `Catalog::set_events_enabled`; `sql::
executor::send_event_capture`, wired into `exec_insert`/`exec_update`/
`exec_delete`. `ExecCtx` gained a `next_event_seq: &mut u64` field — a
deliberate deviation from the original plan (which favored an extra
function argument, mirroring M3.c's `edge_index`): unlike `edge_index`,
which only ever needed to reach one top-level entry point
(`graph_executor::execute`), event capture must reach the deeply nested
private `exec_insert`/`exec_update`/`exec_delete`, exactly the same shape
`index_worker: Option<&IndexHandle>` already has on `ExecCtx` — adding a
field followed the existing precedent instead of forcing `execute()`'s
signature (and every call site) to change. `Heap`/`LockManager`/`txn.rs`
reused entirely as-is (zero changes) — confirmed, not assumed: `Heap::
insert`/`update`/`delete` never call `record_undo` themselves, so the
event row's fate is tied to the surrounding transaction purely by calling
the same `record_undo` every other write path already calls, with zero
new code in the abort path.

**Known limitations / tech debt (new in M4, on top of M1/M2/M3's
carried-forward list):**
- **`poll_events` has no predicate pushdown** — cost scales with
  `__events__`'s total row count, not consumer lag or `limit` (quantified
  above, not just asserted). `vacuum_events` is the only current lever
  that bounds this; a `seq`-ordered secondary index is the natural future
  fix once this becomes a real bottleneck in practice.
- **`__consumers__`'s `ack_events`-driven `heap.update` accumulates dead
  tuple versions with no cleanup** — the same "no vacuum" shape already
  accepted for the heap itself (M1), `VectorIndex`/`InvertedIndex` (M2),
  and `EdgeIndex` (M3), just for a new structure. `vacuum_events` reclaims
  `__events__` rows only; it does not touch `__consumers__`'s own dead
  versions — an asymmetry worth flagging explicitly rather than leaving
  implicit.
- **`apply_rls` is bypassed by `poll_events`/`ack_events`/`vacuum_events`
  entirely, by construction** — they are bespoke `Engine` methods, not
  `execute_sql`-routed plans, exactly like `edges_from` (M3). Consistent
  with existing precedent, not a new gap.
- **No automatic vacuum path** — `vacuum_events` is never called from
  `Engine::checkpoint()` or anywhere else automatically, matching M1's
  zero-automatic-vacuum precedent exactly; confirmed by reading `Engine::
  checkpoint`'s call site, not assumed.

## Bug fix (found during M5): xid reuse after checkpoint   2026-07-06

**Locked-decision change:** D3 (control file) and D9 (fixed on-disk
format) — control file format bumped v2 -> v3. **Human sign-off:**
confirmed with the user before implementation (asked directly whether to
fix immediately as its own commit vs. defer past M5; user chose to fix
immediately).

**What was found:** while manually smoke-testing the new M5 REST server
(`POST /sql` end-to-end against a running `unidb-server`), reopening the
engine after an explicit `checkpoint()` call reset the transaction
manager's xid counter back to 1, even though xids up to 15 had already
been committed in the same database. Root cause:
`TransactionManager::recover_next_xid` determines the xid to resume from
purely by scanning the WAL for `WAL_TXN_BEGIN` records and taking `max +
1` — but `checkpoint::run` truncates every WAL record before the
checkpoint LSN, which in ordinary use is *every* prior transaction's begin
record, since a checkpoint only ever runs after they've all committed.
The existing `xid_counter_survives_reopen` test never caught this because
it calls `flush()` (no truncation) before reopening, not `checkpoint()` —
no existing test combined "commit several transactions, checkpoint,
reopen" until M5's manual server testing exercised exactly that sequence.

**Impact if left unfixed:** silent MVCC visibility corruption — a reissued
xid could collide with, or be misordered relative to, a prior committed
xid still referenced by existing tuples' `xmin`/`xmax`, producing wrong
query results with no error raised. This affects every milestone (M1-M4),
not just M5 — flagged and fixed immediately given the severity, rather
than deferred as "M5 tech debt."

**Fix:** the control file gained a `next_xid: u64` field (44 bytes total,
up from 36; `FORMAT_VERSION` 2 -> 3), persisted by `checkpoint::run`
alongside `checkpoint_lsn`/`wal_tail_lsn` — captured *before* WAL
truncation, using a new `TransactionManager::next_xid()` accessor.
`Engine::open` now resumes at `max(WAL-scan result, control.next_xid)`,
correct whether or not a checkpoint ever ran. No migration path — no
prior version of this database has shipped externally (same precedent as
M1.a's v1->v2 tuple-header change).

**What changed:** `src/format.rs` (`FORMAT_VERSION` 3, documented
rationale), `src/control.rs` (`ControlData.next_xid`, updated
encode/decode, layout doc), `src/txn.rs` (`TransactionManager::
next_xid()`), `src/checkpoint.rs` (`run` takes an explicit `next_xid`
parameter), `src/lib.rs` (`Engine::open`'s resume logic, `Engine::
checkpoint()`'s call site).

**Tests added:** `control.rs::next_xid_defaults_to_one_and_round_trips`;
`checkpoint.rs`'s existing test extended to assert the persisted
`next_xid`; `lib.rs::xid_counter_survives_reopen_after_checkpoint` — the
actual regression test, proving a fresh open after checkpointing several
committed transactions resumes strictly past the highest one used. Full
suite (unit + crash + all integration tests) green both with and without
`--features server` before and after.

## M5 — API / server   [DONE]   2026-07-07

**PR:** _pending (not yet opened; benchmarks recorded ahead of PR per session workflow)_
**Summary:** The embedded crate stabilized (a compiler-enforced `Engine:
Send` assertion, a crate-level doc audit, transaction-boundary doc
comments, and an unwrap/expect audit confirming CLAUDE.md's "no unwrap/
expect outside tests" rule holds) plus an optional REST/JWT/SSE/metrics
server built entirely behind a new `server` Cargo feature, so a default
`cargo build`/`cargo test` of the embedded crate never depends on an
async runtime — "the engine stays sync" is literally true for a default
consumer, not just true when a flag happens to be off (verified via
`cargo tree --no-default-features --edges normal`, empty of tokio/axum/
jsonwebtoken throughout). Shipped as four internal checkpoints (M5.a
stabilization + writer-thread bridge, M5.b REST core, M5.c JWT/SSE/
metrics, M5.d hardening + tests + benchmarks + this closeout).

**The core architectural decision:** async HTTP handlers never touch
`Engine` directly. One dedicated OS thread (`EngineHandle`,
`src/server/engine_handle.rs`) owns the `Engine` for its entire life,
mirroring `index_worker.rs`'s spawn/channel/bounded-shutdown precedent
exactly — chosen over a shared `Mutex<Engine>` specifically to preserve
the engine's real invariant (single-thread ownership) rather than
introduce a "never `.await` while holding the lock" discipline every
future call site would have to remember. `/sql` and `/cypher` get atomic
multi-statement transactions over HTTP for free, since `execute_sql`
already accepts a full `;`-separated statement string executed under one
`xid` — zero new engine code needed for that.

**Critical bug found and fixed mid-milestone, not part of M5's own
feature scope:** manually smoke-testing the new server surfaced a real,
pre-existing (M1-era) xid-reuse-after-checkpoint bug — see the dedicated
"Bug fix (found during M5)" entry above. Flagged to the user immediately
given its severity (silent MVCC-visibility corruption), fixed as its own
commit with explicit sign-off before continuing M5's feature work, not
folded silently into an M5 commit or deferred.

**Benchmarks** (release build, Apple Silicon macOS, `cargo bench --bench
server --features server`, `--sample-size 10`; scope confirmed with the
user ahead of implementation — see the note below):

| Workload                                                  | Result |
|------------------------------------------------------------|--------|
| Direct `Engine::insert` (own txn per op)                    | ~6.30 ms |
| `POST /rows` (same op, over HTTP + writer-thread channel)   | ~6.69 ms |
| HTTP+writer-thread overhead vs. direct call                | **~1.06x** (~6%) |
| JWT verification alone (`jsonwebtoken::decode`, HS256)      | ~817 ns |
| SSE `/events/subscribe`, 1 concurrent subscriber            | ~5.22 ms |
| SSE `/events/subscribe`, 10 concurrent subscribers          | ~33.87 ms |
| SSE `/events/subscribe`, 50 concurrent subscribers          | ~162.60 ms |
| `POST /sql` throughput, 1 concurrent client                 | ~7.40 ms/op → ~135 ops/s |
| `POST /sql` throughput, 10 concurrent clients                | ~63.88 ms/10 ops → ~157 ops/s aggregate |
| `POST /sql` throughput, 50 concurrent clients                | ~316.36 ms/50 ops → ~158 ops/s aggregate |

**Benchmark scope note (§6):** per the decision confirmed with the user
ahead of implementation, M5's own benchmarks stay server-overhead-focused
— there is no external "REST+JWT+SSE embedded database server" incumbent
this project is trying to beat, so the only meaningful comparison is
"how much does wrapping the already-measured engine in HTTP cost." The
full CLAUDE.md §6 cross-domain "replaced stack" showcase (Postgres +
pgvector + a graph DB + a message queue, one unidb transaction vs.
dual/triple-write with no shared transaction) is now possible for the
first time since all four data models exist, but remains a separate,
dedicated future effort, not folded into M5 — standing up a graph DB
and/or message queue for a fair comparison is a materially bigger lift
than reusing the Postgres instance already running locally, which is all
M1-M5's own benchmarks needed.

**Honest read of these numbers:**
- **The HTTP/writer-thread layer itself is nearly free (~6% overhead)** —
  almost the entire per-request cost is the same fsync-per-statement
  round-trip M1-M4 already measured and documented, not anything new M5
  introduces. This is the single most reassuring number in this table:
  the architectural choice to bridge sync `Engine` into async handlers via
  a dedicated writer thread (rather than, say, `spawn_blocking` per
  request or a lock-contended `Mutex<Engine>`) costs almost nothing extra.
- **Concurrent `POST /sql` throughput is flat (~135 -> ~157 -> ~158 ops/s)
  across 1, 10, and 50 concurrent clients — not scaling with concurrency
  at all.** This is exactly the single-writer-thread design's actual
  throughput ceiling, made concrete rather than assumed: every write
  serializes through the one channel to the one writer thread, and every
  commit pays its own WAL fsync (D2's per-statement mini-txn, the same
  root cause M1-M4 already found), so adding more concurrent HTTP clients
  just queues more work behind the same bottleneck instead of unlocking
  more throughput. The ~135-158 ops/s figures land squarely in the same
  range M1's own `benches/load.rs` already recorded for single-table
  INSERT (~155-162 elem/s, own txn per op) — confirming this is the
  identical, already-documented bottleneck surfacing through a new
  interface, not a new one.
- **SSE polling overhead scales worse than linearly with subscriber count
  (1 -> 10 -> 50 is ~5.2ms -> ~33.9ms -> ~162.6ms, roughly a 6.5x and then
  ~31x increase for 10x and 50x more subscribers)** — quantifying the
  "N subscribers x poll interval x `poll_events`'s own linear-in-table-size
  cost" concern `sse.rs`'s module doc already flagged qualitatively.
  Every subscriber's poll tick contends for the same single writer thread
  as every other request, so this is the same bottleneck as the
  concurrent-throughput finding above, viewed from the subscribe side —
  not a separate SSE-specific inefficiency.
- **JWT verification (~817 ns) is genuinely negligible** next to
  millisecond-scale request costs — confirms rather than merely assumes
  that the auth layer isn't where any meaningful cost lives.

**Crash correctness:** no new crash-injection P-number — event rows and
every other row the server ever writes are ordinary WAL-backed heap rows
using the exact same mini-txn/user-txn machinery `tests/crash/main.rs`'s
P1-P9 already cover. `tests/server_shutdown.rs` proves the HTTP/
writer-thread layer itself introduces no *additional* way to lose
committed data or hang: several writes committed over HTTP, one more
request fired with its reply intentionally never awaited, then graceful
shutdown triggered immediately — shutdown completes within its bound and
a fresh `Engine::open` afterward sees every write committed before the
signal.

**What changed:** `src/server/` (new: `engine_handle.rs`, `error.rs`,
`dto.rs`, `handlers.rs`, `router.rs`, `auth.rs`, `sse.rs`, `mod.rs`),
`src/bin/unidb-server.rs` (new binary), a new `server` Cargo feature
gating `tokio`/`axum`/`tower`/`tower-http`/`jsonwebtoken`/`metrics`/
`metrics-exporter-prometheus`/`axum-prometheus`/`async-stream`/
`futures-util` as optional dependencies. `Engine: Send` compile-time
assertion + crate-level doc comment + transaction-boundary doc comments
on `insert`/`get`/`delete`/`checkpoint`/`begin_with_isolation`/`commit`/
`abort` (`src/lib.rs`). Plain `serde::Serialize` derives (unconditional —
`serde` is already a core dependency via `Literal`) added to `RowId`,
`Edge`, `Event`, `IndexStatus`. New `DbError::EngineUnavailable` variant
(the writer thread's channel closed — only ever produced by the server
layer). Control file format bump v2->v3 (`next_xid` field) — see the
dedicated bug-fix entry above, not part of M5's own feature scope but
landed during this milestone.

**Known limitations / tech debt (new in M5, on top of M1-M4's
carried-forward list):**
- **No explicit multi-request transaction *sessions*** — every route is
  one complete, self-contained transaction; multi-statement atomicity is
  available today via one `;`-separated `/sql` body, not via separate
  `/begin`-then-later-`/commit` calls across requests.
- **No REST surface for RLS** — `Expr` has no serde/SQL surface, and
  accepting an arbitrary predicate AST from an untrusted HTTP body is a
  real security question, not just a serialization gap. RLS stays
  Rust-API-only, exactly as it has been since M1.
- **REST only, no gRPC** — never confirmed in-scope beyond the
  architecture diagram's aspirational "REST/gRPC" label.
- **No TLS termination** — the server binds plain HTTP; production
  deployments are assumed to sit behind a reverse proxy that terminates
  TLS, a standard pattern for embedded/internal services, stated as an
  assumption rather than silently implied.
- **No login/token-issuing endpoint** — verify-only, stateless JWT per
  the locked decision; the server never issues tokens, has no user or
  credential database, and no session state.
- **No connection pooling/sharding** — single-primary, single writer
  thread, by design (CLAUDE.md §1's non-goals). Quantified directly above:
  concurrent `POST /sql` throughput is flat regardless of client count.
- **SSE `/events/subscribe` is "server polls, pushes to client," not
  WAL-level push** — `poll_events` has no wake primitive; cost scales with
  subscriber count as quantified above.
- **No writer-thread crash recovery/restart-in-place** — a panicked
  writer thread takes `Engine` down with it; the expected recovery is a
  process-level restart (systemd/k8s), not in-process self-healing.
- **Read-only routes still pay a full commit fsync**, inheriting M1's
  already-documented tech debt — now directly visible as REST-read
  latency rather than a Rust-API-only concern.
- **No admin-scope JWT claim distinction** — any validly-signed,
  unexpired token can hit `/checkpoint` and every other route alike.

---

## M6 — B-Tree secondary index   [DONE]   2026-07-07

**PR:** _pending (not yet opened; benchmarks recorded ahead of PR per session workflow)_
**Summary:** A general-purpose `IndexKind::BTree` secondary index
accelerating equality/range `WHERE` predicates on `Int64`/`Text`/`Bool`
columns, closing a real gap: `exec_select` previously always did a full
heap scan regardless of any index — `NEAR` was the only predicate that
ever consulted one. Backed by `std::collections::BTreeMap` (zero new
dependencies), reusing M2's existing async index-worker machinery
(`index_worker.rs`) unchanged in shape. Shipped as three internal
checkpoints (M6.a type + worker wiring, M6.b index-assisted `exec_select`,
M6.c benchmarks + hardening). Prompted by a comparison against a
competing project (FFS/ffsdb) that publishes B-Tree/HNSW/CSR benchmarks —
this is the first of three follow-on milestones (M6 B-Tree, M7 CSR graph,
M8 attach client) maturing unidb along the same axes; see
`docs/backlog/phase2_sql_capability_expansion.md` for the still-parked SQL
capability work this continues to defer.

**Design decisions:**
- `BTreeIndex` (`src/btree_index.rs`) tracks each `RowId`'s current
  indexed value internally (`by_id: HashMap<RowId, OrderedValue>`)
  alongside the value-sorted `BTreeMap<OrderedValue, Vec<RowId>>`, so
  `upsert` can safely remove a stale bucket entry when a row's indexed
  value changes — unlike `VectorIndex`/`InvertedIndex`, a `BTreeMap` is
  keyed by value, not by id, so this bookkeeping is new, not copied.
- Using the index in `exec_select` is a query-planning decision, not just
  a wiring exercise (unlike adding `FullText`/`Hnsw` in M2, which only
  needed a new `IndexKind` variant): `find_indexable_btree_predicate`
  detects a top-level (or AND'd) `Column <op> Literal` comparison whose
  column has a `BTree` index, and `try_exec_select_btree` reuses
  `exec_select_near`'s exact resolve-then-refilter template (candidate
  `RowId`s -> `heap.get` -> full `predicate_matches`), so MVCC visibility/
  RLS/remaining `AND`ed terms all still apply for free.
- **Correctness-critical difference from `NEAR`**: the index is only
  trusted once `IndexStatus::Ready` — an in-progress backfill has only
  indexed *some* rows, and an equality/range query silently returning an
  incomplete result set would be a real bug (unlike `NEAR`'s inherently
  approximate top-k, where fewer-than-`k` results during a backfill race
  is expected and documented). `try_exec_select_btree` falls back to the
  unchanged full scan whenever the index isn't `Ready`, can't be found, or
  the compared `Literal` isn't orderable — proven directly by
  `btree_select_before_index_ready_still_returns_correct_full_result`.

**Benchmarks** (release build, Apple Silicon macOS, `cargo bench --bench
btree`, 10 samples, indexed vs. unindexed full-scan on identical data):

| Workload | 1,000 rows | 10,000 rows |
|---|---|---|
| Point SELECT (`WHERE id = target`), indexed | ~3.12 ms | ~3.10 ms |
| Point SELECT, full scan | ~3.60 ms | ~4.95 ms |
| Range SELECT (`WHERE id > lo`, ~10 rows), indexed | ~3.18 ms | ~3.17 ms |
| Range SELECT, full scan | ~3.66 ms | ~4.54 ms |

**Honest read of these numbers:** the *scaling* difference is the real
finding, not the absolute latency — both paths still pay the same
per-statement `begin`/`commit` fsync overhead documented since M1 (a
read-only statement's `commit()` unconditionally fsyncs), which dominates
the absolute numbers at this row-count range. The indexed path stays flat
(~3.1 ms regardless of table size) while the full-scan path grows with row
count (3.60 ms -> 4.95 ms point, 3.66 ms -> 4.54 ms range, 1k -> 10k rows)
exactly as expected — the index avoids the growing scan cost, it doesn't
(and can't, at this scale) avoid the fixed fsync cost.

**A genuine discovery made while building this benchmark, unrelated to
B-Tree itself:** two 100,000-row tables in one engine hit
`DbError::BufferPoolFull` during setup, even after switching from one
giant transaction to one commit per 500-row batch. Root cause: the
fixed-capacity (256-frame) buffer pool (`POOL_CAPACITY` in `lib.rs`) keeps
every page a still-open transaction has touched pinned until commit, but
per-batch commits alone didn't fully resolve it at this scale — pointing
at a heap/FSM (free-space-map) page-allocation interaction that grows
pinned-page pressure as a table's total page count grows into the hundreds,
independent of any single transaction's size. **Not investigated further
or fixed here** — out of M6's scope (a B-Tree index, not the buffer
pool/FSM), but a real, previously-undocumented scaling constraint worth
tracking. `benches/btree.rs` scopes its row-count tiers to 1,000/10,000
accordingly, with the reasoning left in a code comment rather than
silently dropping the 100,000 tier.

**Crash correctness:** no new crash-injection P-number — `BTreeIndex` is
purely derived, non-durable state exactly like `VectorIndex`/
`InvertedIndex` (rebuilt from the heap's committed rows on next open, per
M2's already-established "index loss on crash is expected, not a new
durability contract" precedent). `tests/index_rebuild.rs` gained
`engine_restart_rebuilds_btree_index_and_select_still_works` (mirrors the
existing HNSW/FullText restart tests) and
`btree_select_before_index_ready_still_returns_correct_full_result` (the
correctness-critical pre-`Ready` fallback proof above).
`tests/btree_mvcc.rs::aborted_insert_never_surfaces_in_btree_assisted_results`
mirrors `tests/vector_mvcc.rs`'s single-most-important-test shape exactly:
the worker has no transaction concept, so an aborted insert's stale
`BTreeIndex` entry must never leak into a query result — proven by
polling until the worker has indexed the doomed row (a confirmed
precondition, not a timing guess), then asserting a fresh transaction
never sees it.

**What changed:** `src/catalog.rs` (`IndexKind::BTree`, additive), new
`src/btree_index.rs` (`BTreeIndex`, `OrderedValue`, `RangeOp`),
`src/index_worker.rs` (`IndexedColumn::Ordered`, `SecondaryIndex::BTree`,
one new `worker_loop` match arm — index-kind-agnostic call sites
unchanged), `src/sql/executor.rs` (`exec_create_index`'s validation match
extended; new `find_indexable_btree_predicate`/`flip_cmp_op`/
`try_exec_select_btree` in `exec_select`'s path), `src/sql/parser.rs`
(`USING BTREE` — note `sqlparser`'s `IndexType::BTree` is a *native*
built-in variant, unlike `HNSW`/`FULLTEXT`'s `IndexType::Custom` fallback,
discovered when a pre-existing test asserting `USING BTREE` was
"unsupported" broke immediately upon implementing this). New
`benches/btree.rs`, new `tests/btree_mvcc.rs`, extended
`tests/index_rebuild.rs`.

**Known limitations / tech debt (new in M6):**
- **Single-column indexes only** — no composite/multi-column `BTree`
  index, matching M2's identical single-column scope for `HNSW`/`FullText`.
- **No `IN (...)` list-predicate support** — the parser doesn't produce
  that `Expr` shape yet, so `find_indexable_btree_predicate` has nothing
  to detect even if it wanted to.
- **No cost-based index selection** — `exec_select` uses the first
  indexable top-level (or AND'd) predicate term it finds; if a query has
  multiple indexed columns in its `WHERE` clause, there is no comparison
  of which index would be more selective.
- **The `BufferPoolFull`-at-scale discovery above** — a real, separately
  trackable buffer-pool/FSM scaling limit, not fixed here.
- **Deferred to `docs/backlog/`:** none new from M6 itself; the Phase 2
  SQL capability plan remains the standing deferred item.

---

## M7 — CSR (Compressed Sparse Row) graph index   [DONE]   2026-07-07

> **Correction (2026-07-07, during M8 merge verification):** the original
> ship of this milestone wired CSR directly into the `edges_from`/Cypher
> traversal path with a "prefer CSR once `Ready`" policy (see the original
> design-decision bullet below, kept for the record). That was a real
> correctness bug, not a shipped tradeoff: `Ready` means "the initial
> backfill completed," not "every edge write since then is reflected in a
> rebuild" (CSR's rebuild is debounced/async). A transaction could create
> an edge and immediately fail to see it via `edges_from`/Cypher, breaking
> a guarantee M3 shipped with (`tests/graph_mvcc.rs::
> aborted_edge_creation_never_surfaces_in_traversal`, which has no retry
> loop, failed consistently once reproduced in isolation via `cargo test -p
> unidb --test graph_mvcc`, run repeatedly outside the full workspace test
> suite — the bug was invisible in `cargo test --workspace` runs). Fixed by
> reverting `edges_from`/the Cypher executor to consult `EdgeIndex`
> unconditionally again, exactly as before this milestone. `CsrIndex`
> itself, its debounced rebuild, and its being kept warm on every live edge
> write all remain correct, tested, and benchmarked — only the "prefer it
> for traversal" wiring was removed. See `src/graph/index.rs`'s module
> comment for the full writeup. The sections below are left as originally
> written (for an accurate history of what shipped and when) except where
> explicitly marked corrected.

**PR:** _pending (not yet opened; benchmarks recorded ahead of PR per session workflow)_
**Summary:** A read-optimized adjacency structure for graph traversal, built
asynchronously (like M2's HNSW index) on top of the existing background
worker, sitting alongside — never replacing — the synchronous `EdgeIndex`
`create_edge`/`delete_edge` already maintain inline. Unlike HNSW's
still-unfixed "rebuild the whole structure on every single upsert"
pattern, CSR's rebuild is **debounced**: the worker drains every
currently-queued edge message before rebuilding, coalescing a burst of
writes into one rebuild pass. Shipped as three internal checkpoints (M7.a
`CsrIndex` type + debounced rebuild, M7.b wiring into `edges_from`/Cypher
traversal, M7.c benchmarks + hardening). Second of the three follow-on
milestones (M6 B-Tree, M7 CSR, M8 attach client) prompted by a comparison
against a competing project (FFS/ffsdb); M8 is next, then the parked
Phase 2 SQL work in `docs/backlog/phase2_sql_capability_expansion.md`.

**Design decisions:**
- `IndexKind::Csr` (`src/catalog.rs`) is **engine-managed only** — there is
  no SQL keyword for it and no way to set it via `CREATE INDEX`/`ColumnDef.
  index`. It exists purely so CSR can reuse `index_worker.rs`'s generic
  `(table, column)`-keyed machinery for `__edges__`'s `from_id`, registered
  as `("__edges__", "from_id")`, the same way a real column index would be.
- `CsrIndex` (`src/csr_index.rs`) splits raw accumulation from the
  queryable structure: `stage(from_id, row_id)` appends to a plain `Vec`,
  and only `rebuild()` recomputes the sorted `from_ids_sorted`/`row_ptr`/
  `col_ind` CSR arrays — the classic layout, O(n log n) per rebuild, not
  incrementally patchable (directly analogous to `instant-distance`'s HNSW
  having no incremental insert, per M2.b's design note).
- **The debounce mechanism**: `index_worker.rs`'s `worker_loop` was
  restructured from a plain `for msg in rx` into `apply_msg` (applies one
  message, staging CSR edges without rebuilding) plus an explicit
  drain-via-`try_recv()` loop that coalesces every currently-queued message
  into one `rebuild_dirty` pass before returning to a blocking `recv()`.
  Every non-CSR variant (`Vector`/`Text`/`Ordered`) behaves identically to
  before — this only changes CSR's timing, not its correctness contract.
  Proven by `burst_of_edge_upserts_coalesces_into_far_fewer_rebuilds_than_
  messages` (`index_worker.rs`): 200 messages sent back-to-back, real
  rebuild count observed to be far below 200 (`CsrIndex::rebuild_count()`,
  a test-only counter), not asserted at exactly 1 since the sender/worker
  race can't be pinned down more precisely than "coalesced, not absent."
- **[ORIGINAL, CORRECTED — see the correction note above] `EdgeIndex` stays
  the default, always-current tier; CSR is preferred only once `Ready`**
  (`graph::index::graph_candidates`, consulted by both `Engine::edges_from`
  and the Cypher executor's fast path). Reasoning worked through
  explicitly, not assumed: CSR's async lag can only cause a *missed*
  very-recent edge (a false negative), never a phantom one, since every
  candidate — from either index — is still re-validated against MVCC
  visibility downstream (`resolve_candidates_batched`). That's the same
  staleness characteristic every other async secondary index already has
  once `Ready`; no "only use CSR above N candidates" heuristic was needed.
  **This reasoning was wrong**: it correctly rules out a *phantom* edge but
  misses that a debounced rebuild can also cause a false negative for an
  edge created *by the current transaction, moments ago* — which violates
  self-visibility, a stronger guarantee than "eventually consistent
  candidate source" that `edges_from` had always provided pre-M7 and that
  `NEAR`/full-text's "may return fewer results while `Building`" contract
  does not have to meet. `graph_candidates` was removed; `edges_from`/
  Cypher now call `EdgeIndex` directly and unconditionally.
- No live-delete message for CSR (`delete_edge` sends nothing) — matches
  the existing "deletion is implicit, filtered out by MVCC re-validation at
  read time" convention every other secondary index already has.

**Benchmarks** (release build, Apple Silicon macOS, `cargo bench --bench
graph`, 10 samples, extending the existing `adjacency_scan` group with a
CSR variant):

| Hot hub size | naive | batched (EdgeIndex) | csr |
|---|---|---|---|
| 1,000 edges (8 pages) | 899 µs | 97.7 µs | 97.4 µs |
| 10,000 edges (78 pages) | 9.15 ms | 972 µs | 998 µs |

**Honest read of these numbers:** CSR is at parity with the already-fast
`EdgeIndex`+batched-resolve path — no meaningful win or loss (differences
are within noise). This is the expected, honest result, not a
disappointment: for this single-hop workload, the batched-resolve step
(grouping candidates by page, M3.b) already dominates cost, and a binary
search into a sorted array (CSR) costs about the same as an O(1) HashMap
lookup (`EdgeIndex`) once that's the bottleneck. CSR's actual value
proposition — cache-friendly, contiguous adjacency for repeated lookups in
multi-hop traversal — isn't exercised here because Cypher itself only
supports single-hop patterns today (see Known limitations). Reporting this
plainly rather than searching for a workload that flatters the number,
per CLAUDE.md §6.

**Crash correctness:** no new crash-injection P-number — `CsrIndex` is
purely derived, non-durable state exactly like `EdgeIndex`/`VectorIndex`/
`BTreeIndex` (rebuilt from `__edges__`'s committed rows on next open).
**[ORIGINAL, CORRECTED]** `tests/graph_rebuild.rs` originally gained
`engine_restart_rebuilds_csr_index_and_traversal_still_works` and
`engine_restart_csr_reflects_deletes_from_before_close` (both explicitly
waited for CSR `Ready` before asserting, to provably exercise the
CSR-preferring path); `tests/graph_mvcc.rs` originally gained
`aborted_edge_creation_never_surfaces_via_csr_path_once_ready`. All three
were removed during the M8 merge correction: `edges_from`/Cypher no longer
ever consult CSR, so a test asserting "the CSR path is safe" would just be
re-testing `EdgeIndex` under a misleading name. The pre-existing
`aborted_edge_creation_never_surfaces_in_traversal`/`..._in_cypher_query`
(M3) and `engine_restart_rebuilds_edge_index_and_traversal_still_works`/
`engine_restart_reflects_deletes_from_before_close` (M3.d) remain and are
what actually cover this path now — no coverage was lost, since those
never depended on CSR's involvement.

**What changed:** `src/catalog.rs` (`IndexKind::Csr`, engine-managed-only),
new `src/csr_index.rs` (`CsrIndex`), `src/index_worker.rs`
(`IndexedColumn::Edge`, `SecondaryIndex::Csr`, `worker_loop` restructured
into `apply_msg`/`rebuild_dirty` for debouncing). `src/lib.rs`
(`create_edge` sends a live CSR upsert alongside its existing synchronous
`EdgeIndex.insert`; new `rebuild_csr_index` backfill function, called
during `Engine::open` alongside `rebuild_secondary_indexes`) — these parts
shipped as originally designed and remain unchanged. **[CORRECTED during
M8 merge]** `src/graph/index.rs`'s `graph_candidates` (the CSR-preferring
selection function) and `src/graph/executor.rs`'s extra `index_worker`
parameter were both added, found buggy, and then removed —
`edges_from`/`execute_cypher` route through `EdgeIndex` directly again,
and `graph_executor::execute`'s signature is back to its pre-M7 3
arguments. Extended `benches/graph.rs` (unaffected by the correction — it
builds `CsrIndex` and calls `candidates()` directly, not through
`graph_candidates`).

**Known limitations / tech debt (new in M7):**
- **CSR indexes only `from_id` adjacency** (forward traversal) — no
  `to_id`/reverse-traversal CSR structure.
- **No multi-hop CSR-accelerated BFS** — Cypher itself only supports
  single-hop `(a)-[:TYPE]->(b)` patterns today, so this isn't a regression,
  just headroom CSR doesn't yet get to fill. The benchmark parity finding
  above is a direct consequence of this: CSR's real advantage only shows up
  once multi-hop traversal exists to exploit its contiguous layout.
- **Rebuild is still O(n log n) over the *entire* edge set per
  debounce-triggered pass** — debouncing reduces *frequency*, not the
  fundamental non-incremental nature of the structure. Acceptable for now,
  same category of tech debt as HNSW's, just less severe.
- **CSR is not currently consulted by any query path** (post-correction) —
  it is built, kept warm, and benchmarked in isolation, but `edges_from`/
  Cypher always use `EdgeIndex`. A future fix needs a staleness/generation
  marker proving CSR has incorporated every write up to a specific point
  before it can be safely preferred again; not attempted here since it's
  new design work, not a bug fix.
- **Deferred to `docs/backlog/`:** none new from M7 itself; Phase 2's SQL
  capability plan remains the standing deferred item, now one milestone
  closer (M8 attach client is next).

---

## M8 — Attach client (Rust, blocking `reqwest`)   [DONE]   2026-07-07

**PR:** _pending_
**Summary:** A third deployment mode alongside embedding `unidb::Engine`
directly or running the standalone REST server: `unidb-attach`, a Rust
crate giving one-shot, `Engine`-like method calls to a process that isn't
running its own `Engine`, built entirely on the existing REST API
(`docs/REST_API.md`) — no new protocol, no new server-side capability.
Third and last of the three follow-on milestones prompted by the FFS/ffsdb
comparison (M6 B-Tree, M7 CSR, M8 attach client); the parked Phase 2 SQL
plan (`docs/backlog/phase2_sql_capability_expansion.md`) is next up.

This milestone was developed in a separate git worktree
(`m8-attach-client` branch) in parallel with M6/M7 landing on `main`, then
merged onto `main` after independent verification (build, full test suite,
clippy, fmt, and a check that the embedded `unidb` crate's dependency
graph stays free of `reqwest`/tokio — confirmed via `cargo tree -p unidb
--no-default-features --edges normal`). The merge verification pass is
also what surfaced and fixed the M7 CSR-traversal bug documented above —
not something M8 introduced, but found while independently re-verifying
the tree before combining the two milestones' work.

**Design decisions:**
- **Workspace, not a nested subdirectory move.** The root `Cargo.toml` does
  double duty as both `[workspace] members = ["unidb-attach"]` and
  `[package] name = "unidb"` in the same file — `src/`, `tests/`,
  `benches/` all stay exactly where they were. This keeps `reqwest` and its
  dependency tree completely out of the embedded `unidb` crate (it's a
  `unidb-attach` dependency only), while avoiding a disruptive file-move
  migration for a change that a virtual-workspace-plus-nested-crate layout
  would otherwise require.
- **One call = one complete operation**, not a mirror of embedded
  `Engine`'s explicit `begin`/op/`commit` shape. There is no multi-request
  transaction session over HTTP — every mutating REST route already does
  its own internal begin→execute→commit. Multi-statement atomicity is
  available via `;`-separated SQL passed to `execute_sql`, exactly as REST
  already documents. This is a deliberate, documented API-shape difference
  from embedded `Engine`, not an oversight (`unidb-attach/src/lib.rs`'s
  module doc says so explicitly).
- **`AttachError`, not `DbError`, is the client's error type.** `DbError`'s
  variants are storage-internal (`PageNotFound`, `ChecksumMismatch`, ...)
  with no meaningful mapping from an HTTP response. `AttachError` instead
  mirrors the server's documented `code` field 1:1 (`TableNotFound`,
  `ColumnNotFound`, `NotFound`, `TableAlreadyExists`, `WriteConflict`,
  `SerializationFailure`, `SqlParse`, `SqlPlan`, `SqlUnsupported`) plus
  transport-level variants (`Http`, `Json`, `InvalidToken`) and a generic
  `Api { status, code, message }` catch-all for anything unmapped.
- **Blocking `reqwest`, no tokio runtime, no background thread** — matches
  the confirmed decision that a synchronous call blocking its calling
  thread for one HTTP round-trip is acceptable; there's no stated
  concurrency requirement that would justify the complexity of a
  sync-to-async bridge.
- **`unidb-attach` depends on `unidb` only as a `dev-dependency`** (for
  shared DTO shapes used by its integration tests, which spin up a real
  `unidb-server`), not a production dependency — it defines its own
  independent wire-format types (`RowId`, `ExecResult`, `IndexKind`,
  `EdgeResult`) matching the server's JSON shapes. A production consumer of
  `unidb-attach` never pulls in the embedded engine's dependency graph.
  `IndexKind` here deliberately excludes `Csr` (M7) — that variant is
  engine-managed only, never settable via `CREATE INDEX`/`POST /indexes`,
  so there's nothing for a REST client to ever send or receive for it.

**Benchmarks** (release build, Apple Silicon macOS, `cargo bench -p
unidb-attach --bench attach`): compares `direct_engine` (embedded `Engine`
call), `raw_reqwest` (hand-rolled HTTP call, no client wrapper), and
`attach_client` (`AttachClient::execute_sql`) for the same `execute_sql`
call — isolating whether the client wrapper adds overhead beyond what HTTP
itself already costs.

**Honest read:** `attach_client` tracks `raw_reqwest` closely (the wrapper
is a thin, direct pass-through — one JSON serialize, one HTTP call, one
JSON deserialize, no extra buffering or indirection), both an order of
magnitude slower than `direct_engine`, as expected for anything crossing a
network/loopback boundary. This is the same finding M5's server benchmarks
already established for HTTP-vs-embedded overhead — M8 doesn't change that
tradeoff, it just gives Rust callers ergonomic access to the same REST
surface without hand-rolling JSON+HTTP themselves.

**What changed:** new `unidb-attach/` crate (`Cargo.toml`, `src/lib.rs`,
`tests/attach_{crud,sql,graph,extras}.rs`, `tests/attach_common/mod.rs`,
`benches/attach.rs`); root `Cargo.toml` gains a `[workspace]` table;
`docs/REST_API.md` and `README.md` gain a "Rust attach client" section and
project-layout entry; `docs/backlog/m8_attach_client_plan.md` records the
original planning document for this milestone.

**Known limitations / tech debt (new in M8):**
- **No multi-request transaction sessions** (by design — matches REST's
  own limitation, not a client-side gap).
- **`vacuum_events`, `set_rls_policy`, and `flush` are not exposed** — the
  server has no REST route for any of the three; tracked in
  `docs/backlog/` alongside future multi-language (Python/Node) client
  bindings, not silently dropped.
- **Rust-only in v1** — no other language bindings.
- **Blocking I/O** — one attach-client call blocks its calling thread for
  the HTTP round-trip; acceptable given no stated concurrency requirement.

---

## Performance: group commit + read-only fsync skip   [PROTOTYPE — branch `m9-group-commit`]   2026-07-08

**PR:** _pending_

Not a numbered milestone (the `m9_*` filename is taken by the parked
Python-bindings backlog doc). A post-M8 performance track addressing the
diagnosis from the FFSDB-eval session: the ~3–4 ms floor on every durable
operation is per-statement fsync, compounded by the server serializing all
requests through one writer thread. Full plan + correctness analysis:
`docs/backlog/group_commit_and_read_concurrency.md`.

**What shipped (3 of 4 changes):**
- **Read-only fsync skip** (`txn.rs`): `TransactionManager::commit` skips
  `commit_user_txn` (WAL record + fsync) when `undo_log.is_empty()`. A
  read-only txn has nothing to make durable; recovery treats the orphan
  `WAL_TXN_BEGIN` as an incomplete user txn whose undo pass finds no
  mutations to reverse. Resolves the M1.d open question.
- **Group commit** (`wal.rs` + `server/engine_handle.rs`): a default-off
  `Wal::deferred_sync` mode gates the fsync in all four commit/abort paths;
  the server writer thread drains all queued requests into a batch and
  issues **one fsync per batch**, withholding commit/abort replies until
  that fsync so no client observes a non-durable commit. The embedded API
  and crash harness keep per-statement durability (deferred mode is
  server-writer-thread-only).
- **Buffer-pool force-WAL-on-evict** (`bufferpool.rs` + `heap.rs` + `lib.rs`):
  the pool tracks the durable WAL frontier (`durable_wal_lsn`) and
  `find_victim` writes back + evicts a dirty page once its WAL is durable
  (ARIES steal); `BufferPool::fetch_page_for_write` — used by every heap
  write/undo path + the FSM scan — force-syncs the WAL and retries when the
  pool is full of not-yet-durable dirty pages. Makes deferred mode
  unconditionally safe for working sets larger than the pool, and **largely
  fixes the pre-existing M6 `BufferPoolFull`-at-scale limitation** (dirty
  pages were previously never evictable — the D5 hint was hardwired to
  `INVALID_LSN`).

**Metrics (M5 Pro, 2026-07-08):**

| Concurrent `POST /sql` INSERT | before ops/s | after ops/s | speedup |
|---|---|---|---|
| 1 client | ~131 | ~242 | 1.8× |
| 10 clients | ~149 | ~756 | 5.1× |
| 50 clients | ~153 | **~4,780** | **31×** |

Throughput went from **flat** (the single-writer ceiling) to **scaling**
with load. Embedded point SELECT (read-only fsync skip): ~3.05 ms →
**1.09 µs** (~2,800×). Peak RSS unchanged (no new buffering — batching
reuses the existing unbounded request channel).

**Verification:** 229 unit + 25 server integration + 11 crash-harness tests
green; clippy `-D warnings` + fmt clean. No §3 locked decision re-opened
(D1/D2/D5 upheld — the new write-back-on-evict path only writes pages whose
WAL is already durable, and the crash harness confirms recovery is intact).

**6b concurrent read path — point reads landed** (branch
`m9-concurrent-reads`, stacked): a `Send + Sync` `ReadHandle` (over an
`Arc<RwLock>` page-file mmap + `Arc<Mutex>` txn snapshot state) lets `get` /
`GET /rows/:id` run off the single writer thread — reads take no xid, write
no WAL, and never touch the writer's request channel. `tests/
concurrent_reads.rs` proves 4 concurrent readers see exact committed bytes
(no torn pages) while the writer inserts 1000 rows; `benches/server.rs`'s
`concurrent_read_throughput` shows reads scale with concurrency (~3.0k →
~4.3k → ~4.5k reads/s at 1/10/50, HTTP-client-bound in the microbench)
rather than the flat writer-serialized ceiling. `Engine` stays non-`Sync`;
`ReadHandle` is the shared reader.

**Concurrent SQL `SELECT` also landed** (branch `m9-concurrent-select`):
`Engine.catalog` → `Arc<RwLock<Catalog>>` (readers need the live
`TableDef.pages`), a `PageReader`-generic `exec_select_readonly` reusing the
existing decode/predicate/projection helpers, and `ReadHandle::execute_sql`
+ an `is_concurrent_read_sql` classifier so the server routes read-only
`POST /sql` to the read path and writes/DDL/`NEAR` to the writer thread.
`tests/concurrent_reads.rs` proves 4 readers running `SELECT` see consistent
rows (every `name` pairs with its `id` — no torn reads) while the writer
inserts 500 rows. Lock order is consistent (catalog → txn → mmap), so no
deadlock. `NEAR`/graph/queue reads remain on the writer thread by design —
additive on the same foundation if a workload needs them concurrent.

---

## M11 — SQL Constraints   [SQL lane — landing]   2026-07-08

**Branch:** `sql-constraints` (SQL lane worktree; hand-merged to `main` at land-time per the roadmap's parallel-lane operating rules).
**Summary:** PRIMARY KEY / FOREIGN KEY / UNIQUE / NOT NULL / CHECK / DEFAULT,
both as column-level options and table-level constraints, are now parsed off
`CREATE TABLE`, persisted on the catalog, and enforced on the INSERT/UPDATE
write path. Before this, `convert_create_table` read only a column's name +
data type and **dropped its `options` entirely** — every constraint clause
was silently ignored. Delivered without touching any storage-core file
(`heap`/`bufferpool`/`wal`/`txn`/`mvcc`/`recovery`/`read_handle`) and with
`lib.rs` untouched — enforcement reuses the existing heap scan, MVCC
snapshot, and predicate evaluator.

**What changed:**
- `catalog.rs`: new `ColumnConstraints` (not_null / unique / primary_key /
  default / check / references) grouped into one `#[serde(default)]` field on
  `ColumnDef`, and `TableConstraints` (table-level PK / UNIQUE sets / CHECKs /
  FKs) as one `#[serde(default)]` field on `TableDef`; plus `ForeignKeyRef` /
  `ForeignKey`. All `#[serde(default)]`, so pre-M11 catalog blobs deserialize
  unchanged (same forward-compat discipline as M4's `events_enabled`). Dropped
  the now-incompatible `Eq` derive from `ColumnDef` (it carries `Expr`/`Literal`,
  which aren't `Eq`); nothing depended on it.
- `sql/parser.rs`: `convert_create_table` now maps every column option
  (`NotNull`/`Null`/`Default`/`Unique`/`PrimaryKey`/`ForeignKey`/`Check`) and
  every table constraint (`PrimaryKey`/`Unique`/`ForeignKey`/`Check`) into the
  new fields. Table-level PK columns are folded to `NOT NULL` at parse time so
  enforcement has one source of truth. (sqlparser 0.62 shapes confirmed
  against the vendored AST before coding.)
- `sql/logical.rs`: `LogicalPlan::CreateTable` carries `constraints:
  TableConstraints`.
- `sql/executor.rs`: `exec_create_table` persists table constraints;
  `exec_insert`/`exec_update` run DEFAULT fill (INSERT only), NOT NULL, CHECK,
  UNIQUE, and FK-referenced-table existence.
- `error.rs`: `NotNullViolation` / `UniqueViolation` / `CheckViolation` /
  `ForeignKeyViolation`. `server/error.rs` maps them to 4xx (UNIQUE → 409,
  the rest → 400) — an additive arm on the existing exhaustive match.

**Key design decisions (evidence-based, recorded honestly):**
- **UNIQUE is enforced by a synchronous heap scan under the writer's own MVCC
  snapshot — deliberately NOT via the M6 B-Tree index**, despite the task
  prompt's suggestion to reuse it. The B-Tree index is maintained by the async
  background worker, and `IndexStatus::Ready` only means "initial backfill
  drained," not "every write since is reflected" — the exact staleness that
  caused the documented M7 CSR-traversal bug (`MEMORY.md`). A stale/absent
  index entry is a *false* "no conflict," which for a correctness check would
  silently admit duplicates. A heap scan is the only source guaranteed current
  for the writing transaction; it also sees the transaction's own uncommitted
  writes, so a duplicate *within a single multi-row INSERT* is caught. The
  B-Tree index stays a read-side query accelerator only. This is the one
  deliberate deviation from the prompt, made for correctness and flagged here.
- **FK enforcement is referenced-table-existence only** (M11 scope, as
  prompted). Referenced-*row* existence and `ON DELETE`/`ON UPDATE` actions are
  out of scope — there is no `DROP TABLE` yet and row-level FK is a materially
  bigger lift. `CREATE TABLE` with a forward reference is allowed; the check
  fires on write.
- **CHECK reuses the SELECT/WHERE `eval_expr` evaluator** and inherits its
  documented two-valued NULL semantics: a comparison with a NULL operand is
  non-true and so fails the check (stricter than SQL's "NULL ⇒ unknown ⇒
  pass"). Pair CHECK with NOT NULL/DEFAULT if a nullable column must skip it.
- **DEFAULT fills any NULL-valued column at INSERT** (never UPDATE). Positional
  ordering can't distinguish an explicit `NULL` from an omitted column, so
  `INSERT ... VALUES (NULL)` into a defaulted column fills the default — a
  minor, documented divergence.

**Tests:** new `tests/constraints.rs` — 12 integration tests covering each
kind, its violation rejection, DEFAULT fill, self-update-not-a-conflict,
NULLs-are-distinct, table-level composite UNIQUE/CHECK/FK, and
survive-reopen. Full suite green: `cargo test -p unidb` (226 unit + 12
constraints + 11 crash + all other integration) and `cargo test -p unidb
--features server` both pass; `cargo clippy --workspace --all-targets -- -D
warnings` and `cargo fmt --all --check` clean.

**Benchmark note (§6):** constraints are correctness features, not a
throughput workload; no new benchmark table. The added per-row cost is a
UNIQUE heap scan *only when a UNIQUE/PK constraint exists* (O(rows) per
inserted row — a known, documented cost that a future secondary-index-backed
uniqueness check could reduce once the index is made synchronously
authoritative). Tables with no UNIQUE/PK pay near-zero extra (a few per-column
flag checks).

**Locked-decision changes:** none. (`ColumnDef` losing its `Eq` derive is an
internal type change, not a §3 locked decision; on-disk format stays
forward-compatible via `#[serde(default)]`, so no `FORMAT_VERSION` bump.)

**Known limitations / tech debt (new in M11):**
- UNIQUE scan is O(rows)/insert; no index-backed fast path yet (see design
  note for why the async B-Tree index can't be trusted for this).
- FK is existence-only (no row-level referential integrity, no cascade).
- CHECK inherits two-valued NULL semantics.
- Constraints are not retro-validated against pre-existing rows (there is no
  `ALTER TABLE ADD CONSTRAINT`); they apply to writes after `CREATE TABLE`.

---

## Track D — Semantic search (cosine metric + embedding CLI) — 2026-07-08

**Lane:** Surface (worktree `../unidb-embed`, branch `surface-embed`). Disjoint
from Core/SQL: the *only* engine file touched is `src/vector.rs`; everything
else is a new workspace-member crate. Proposed as its own milestone per the
roadmap (§3 Track D, ~1 unit, "mostly client").

**What shipped (two independent deliverables):**

1. **Cosine distance in the vector index** (`src/vector.rs`, small & contained).
   New `pub enum Metric { Euclidean, Cosine }` (Euclidean is `#[default]`, so
   `VectorIndex::new()` and the `index_worker.rs` construction site are
   byte-for-byte unchanged — backward compatible). Added
   `VectorIndex::with_metric`, `metric()`, and `set_metric()`. The metric is a
   **per-index** choice carried on every `VectorPoint`, applied identically
   during HNSW build and search. Cosine is `1 - cos(a,b)` (`pgvector`'s `<=>`),
   with a zero-norm guard returning max distance. `set_metric` **handles the
   rebuild**: because the graph's edges were chosen *by* the old metric, a
   metric change re-runs `rebuild()` over the buffered point set (no-op if
   unchanged). 9 new unit tests (cosine ranks by direction not magnitude;
   Euclidean vs cosine provably disagree on the same points; set_metric
   re-ranks; zero-vector guard) — engine lib tests 225 → 234, all green.
   *Not done here (out of the Surface lane's file scope):* wiring a
   `USING HNSW <metric>` choice through `CREATE INDEX`/catalog/executor — that
   is SQL-lane work; the engine API supports cosine today.

2. **New crate `unidb-embed/`** (workspace member, like `unidb-attach`): a CLI
   that turns text into vectors via a **pluggable HTTP embedding endpoint**
   (OpenAI-compatible; API key via `UNIDB_EMBED_API_KEY` env var), then stores
   and searches them through the UniDB REST server using the `unidb-attach`
   client. Commands: `embed-insert` (embed text → `INSERT ... VALUES (id,
   'text', [vec])`) and `search` (embed query → `SELECT ... WHERE NEAR(col,
   [vec], k)`). Column names default to `id`/`content`/`embedding`, overridable.
   Modules: `embed.rs` (HTTP embedding client, parses OpenAI `data[0].embedding`
   or a flat `embedding` shape), `sql.rs` (pure, tested SQL builders with
   single-quote escaping), `main.rs` (clap CLI + result printer). 11 unit tests.
   Short `README.md` with an end-to-end worked example (create table + HNSW
   index, embed-insert three docs, semantic `search`).

**Deliberate constraint honored:** embedding *generation* is client-side ONLY.
`unidb-embed` depends on `reqwest` + `unidb-attach`; **no model/network dep
reaches the `unidb` engine crate** — verified by it not being added to the
engine's `[dependencies]`.

**Gates:** `cargo test --workspace` green (234 engine lib + 11 `unidb-embed` +
all server/attach/crash/concurrency suites); `cargo clippy --workspace
--all-targets -- -D warnings` clean; `cargo fmt --all` clean. No benchmark
table: this milestone adds no hot-path change to measure (cosine is an
alternate metric on the existing index; the CLI is a thin client). No locked
decision (§3) touched.

## M10 — Heap vacuum / MVCC garbage collection   [DONE]   2026-07-08

**PR:** _(branch `core-vacuum`, Core lane)_
**Summary:** The engine now physically reclaims space held by dead tuple
versions via an explicit `Engine::vacuum() -> VacuumReport` (no autovacuum in
v1 — same explicit-call model as `vacuum_events`). This closes the one place
the engine stood *in* the MVCC bloat trap rather than sidestepping it. Built on
top of the already-merged concurrent-read model (PRs #2–#4): the visibility
horizon includes live `ReadHandle` readers, not just the writer's active
transactions. Checkpoints M10.a→M10.d all landed.

**Benchmarks** (release build, `benches/vacuum.rs`, Apple Silicon / macOS):

| Workload | Result |
|---|---|
| Update-churn heap file, 200 keys × 30 rounds, **no vacuum** | 606,208 bytes (grows unbounded with churn) |
| Same churn, **vacuum after each round** | 73,728 bytes (**8.2× smaller** — slots reused, leak closed) |
| `Engine::vacuum()` on a 200×30 churned DB (~6,000 dead versions) | ~25.7 s total, ~4.3 ms/version (516,800 bytes reclaimed in-page) |

The headline is the **bounded-vs-unbounded** comparison, not a single-vacuum
file shrink: v1 vacuum makes freed intra-page slots reusable but does **not**
lower the file's high-water mark (that's a `VACUUM FULL`-class op, backlog). So
under update churn, periodic vacuum keeps the heap file bounded while the
un-vacuumed baseline grows without limit — the number that proves the leak is
closed. Peak RSS tracks heap-file size (memory-mapped page store), so the same
bounded-vs-unbounded relationship holds for RSS.

Vacuum's own cost is **fsync-bound** at ~4.3 ms per reclaimed version on the
default per-statement-durability path: each `mark_dead` and each `compact_page`
is its own fsyncing mini-txn (D2/D5), so reclaiming N versions costs ~N+ fsyncs
— the same ~3–4 ms floor every durable op in this engine pays (see M1/M3
notes). It is correct and crash-safe as-is; batching vacuum's mini-txns behind
one fsync (the M9 group-commit `deferred_sync` mechanism) is the obvious future
speedup and is noted below, not done here.

**Crash harness:** P1–P10 all green (new **P10** = kill mid-vacuum → reopen →
committed-visible row survives, reclaimed version stays reclaimed, re-running
vacuum is a no-op). Property crash test unchanged and green.

**What changed:**
- **M10.a horizon.** `TransactionManager::vacuum_horizon()` = `min snapshot.xmin`
  over all live writer txns **and** live concurrent readers. Readers register a
  `ReadRegistration` RAII guard (from `txn::read_snapshot`) held for the whole
  read in `read_handle.rs`, so an off-thread scan genuinely holds the horizon
  back. `mvcc::is_reclaimable(xmax, horizon)` is the deliberate inverse of
  `is_visible`, cross-checked against it in a table-driven test.
- **M10.b heap removal + WAL.** New `SlotState` LIVE→DEAD→UNUSED (encoded in the
  existing slot `(offset,length)` pair, no format change). `Heap::
  collect_reclaimable` + `mark_dead`, logged as redo-only idempotent
  `WAL_VACUUM` mini-txns (D2/D5, no undo). `scan`/`get`/`resolve_candidates_
  batched` skip non-live slots (also fixes a pre-existing latent scan fragility
  around recovery-undone insert slots).
- **M10.c index vacuum (the hazard).** `Engine::vacuum` scrubs every reclaimed
  `RowId` from secondary indexes (`EdgeIndex::remove_rowid`, `IndexHandle::
  remove_rows` for BTree/FullText/Vector) **before** any slot becomes reusable.
  Reproduced the aliasing bug deterministically (aborted `create_edge` leaves a
  stale `EdgeIndex` entry; with the gate off, slot reuse makes `edges_from`
  return a wrong-but-visible edge) and proved the real `Engine::vacuum` makes it
  impossible — the M10 analogue of `graph_mvcc.rs`'s single most important test.
- **M10.d space reuse + API.** `Page::compact` (drop dead bodies, coalesce free
  space, promote DEAD→UNUSED, logged as a full-image `WAL_VACUUM`), UNUSED-slot
  reuse in `insert_versioned`, `Engine::vacuum()` + `VacuumReport`.

**Resolved plan decisions (documented, not silent):**
- `VectorIndex` *does* have a (rebuild-based) `remove`, so vector-indexed tables
  are cleaned rather than excluded from slot reuse.
- `CsrIndex` is deliberately left un-scrubbed: no incremental remove, rebuilt on
  open, and consulted by no read path (M7's prefer-CSR wiring was reverted), so
  a stale CSR candidate can never surface.

**Known limitations / tech debt:** manual vacuum only (no autovacuum);
long-lived RR txns / readers hold the horizon back (surfaced in
`VacuumReport.horizon_blocked`, not swallowed); intra-page compaction only (no
cross-page / `VACUUM FULL` high-water-mark shrink); catalog pages still leak;
index structures shrink by entry removal, not physical rebuild. All parked in
`docs/backlog/m10_heap_vacuum_gc.md`'s backlog.

**Deferred to later milestones:** autovacuum; `VACUUM FULL`-equivalent; vacuum
over REST; `VectorIndex` true incremental remove; index bloat reclamation;
**group-committing vacuum's mini-txns** (one fsync per batch via the existing
`deferred_sync` mechanism) to cut the ~4.3 ms/version fsync-bound cost.

**Locked-decision changes (if any):** none. New `WAL_VACUUM` record type is an
additive extension of the existing WAL wire format (like M1's `WAL_TXN_*`), not
a change to a locked decision; `FORMAT_VERSION` is unchanged (no on-disk page or
control-file layout change).

---

## Phase 1 — ACID & storage foundation (Core lane, `acid-hardening`)

The feature-freeze gate (`docs/backlog/phase1_acid_hardening.md`): close the
silent correctness holes before any scale/feature work. One PR per checkpoint.

### P1.a — Full-page-writes (WAL_FPI, torn-page protection)   [shipped]   2026-07-08

**PR:** #6 — https://github.com/sagarm85/unidb/pull/6 (Core lane, branch `acid-hardening`)
**Summary:** Closes the #1 silent data-loss hole (roadmap Tier 0). An 8 KiB
page write is not atomic; a crash mid-write leaves a half-old/half-new page
that CRC detects but cannot repair. Now, on the **first modification of a page
after each checkpoint**, the buffer pool logs the whole clean page image to the
WAL as a new redo-only `WAL_FPI` record before the first incremental change
record; recovery replays that image as the clean base and re-applies the
interval's later incremental redo records on top, so a torn on-disk page is
fully reconstructed. `FORMAT_VERSION` bumped **3 → 4** (new WAL record kind, D9).

**What landed:**
- `format.rs`: `WAL_FPI = 12`; `FORMAT_VERSION = 4`.
- `wal.rs`: `Wal::log_fpi` (redo-only whole-page record, `slot = u16::MAX`).
- `bufferpool.rs`: `fpi_logged: HashSet<PageId>` tracking; `maybe_log_fpi`
  (logs one image per page per checkpoint interval, before the first change),
  `mark_fpi_logged`, `clear_fpi_tracking`, and `restore_page_image` (recovery
  overwrite that bypasses CRC on the possibly-torn on-disk page, extending the
  file if needed). Tracking by `PageId` (not a per-frame flag) deliberately
  survives eviction → exactly one FPI per page per interval, strictly less WAL
  than a per-frame flag would emit, equally correct.
- `heap.rs`: every mutation path (`insert`/`update` [both pages]/`delete`/
  `undo_xmax_stamp`/`undo_insert`/`mark_dead`) logs its FPI right after
  fetching the page and before the incremental record, chaining `prev_lsn`.
  `compact_page` already writes a full page image, so it just marks the page
  FPI-covered.
- `recovery.rs`: `WAL_FPI` redo arm — unconditional, idempotent restore of the
  clean base before the interval's incremental redos (higher LSN) replay.
- `checkpoint.rs`: `clear_fpi_tracking()` after `flush_all` re-arms the next
  interval (the checkpoint re-established a clean on-disk base for every page).

**Why one FPI per page per interval is sufficient (and why incomplete txns are
safe without one):** a page can only reach disk (torn) *after* its mini-txn
commit record is durable — D5 forbids flushing a page whose WAL is not yet
durable — so any torn on-disk page belongs to a committed mini-txn whose FPI is
in the committed redo set. Incomplete mini-txns never reach disk torn, so their
undo pass always reads a clean page. The single interval-opening image plus all
of the page's subsequent WAL records reconstruct it regardless of torn bytes.

**Crash harness (grew, per the gate):** new **P11** — `p11_torn_page_restored_
from_full_page_image`. Commits a row, flushes + checkpoints (clean base on
disk, FPI tracking reset), inserts a second row on the same page (logs
`WAL_FPI` + the incremental insert), then **manufactures a genuine torn page**
by clobbering the second half of the on-disk page (CRC now invalid, asserted as
a precondition), and asserts recovery restores *both* rows. Full P-series (P1–
P11) + property test green: `cargo test -p unidb --test crash` = 13 tests.

**Benchmark** (`benches/fpi.rs`, release; insert-only, no manual checkpoint —
close to the worst case for FPI overhead, since every page is written once so
the fixed 8 KiB image amortizes over only the rows that fit in it):

| rows | payload | WAL bytes | #FPI | FPI bytes | FPI % of WAL | ins/s |
|------|---------|-----------|------|-----------|--------------|-------|
| 2000 | 8 B     | 614,951   | 9    | 74,169    | **12.1 %**   | 162   |
| 2000 | 64 B    | 844,383   | 23   | 189,543   | 22.5 %       | 160   |
| 2000 | 256 B   | 1,639,395 | 72   | 593,352   | 36.2 %       | 154   |
| 2000 | 1024 B  | 4,970,427 | 286  | 2,356,926 | 47.4 %       | 137   |

- **WAL overhead:** one 8 KiB image per page per interval. It falls as more
  rows share a page (small rows: 12 %) and rises as rows approach page size
  (1 KiB rows: 47 %). This is the standard `full_page_writes` cost, and exactly
  why it pairs with auto-checkpoint (P1.e), which bounds total FPI volume to one
  image per page per interval rather than the once-ever seen here.
- **Throughput: unchanged** vs. pre-FPI (~137–162 ins/s across payloads). The
  embedded write path is fsync-bound (two fsyncs per autocommit row — the
  mini-txn commit and the user-txn commit, the same M1 floor); an FPI adds WAL
  *bytes* but no extra fsync, so wall-clock is untouched.
- **Update-heavy note:** because the image is per-page-per-interval, a workload
  that writes a page many times per checkpoint interval amortizes the single
  image over far more records, so its FPI % is far below these write-once
  figures.

**Locked-decision changes:** none reversed; D1/D5 **strengthened** (FPI makes
redo torn-page-safe). D9 `FORMAT_VERSION` 3 → 4 for the new record kind (no
migration path — no version shipped externally).

**Known limitation (documented, not silent):** P1.a protects the heap write
path (where committed row data lives) and its recovery. A brand-new page that
is allocated, flushed torn, and then never written again (heap alloc without a
following insert, or the catalog's fresh-page blob persist in `catalog.rs`) is
*not* FPI-covered — but such a page holds no independently-committed data and is
not referenced by any committed heap, so a torn copy causes no committed-data
loss. Closing the fresh-page/catalog case (torn-tolerant reconstruction) is
tracked for a later Phase-1/Phase-3 pass; it is out of P1.a's declared file
scope (`wal`/`bufferpool`/`recovery`/`checkpoint`).

### P1.b — fsync-failure handling (fsyncgate) + ordering   [shipped]   2026-07-08

**PR:** #7 — https://github.com/sagarm85/unidb/pull/7 (Core lane, branch `acid-hardening`)
**Summary:** Closes the fsyncgate hazard (roadmap Tier 0). A failed
`fsync`/`msync` may leave the OS having dropped the dirty data while clearing
its dirty bit, so a naive retry can return success without the data ever
reaching disk. The WAL and the buffer pool now treat a durability-primitive
failure as **fatal for the session**: they latch into a poisoned state and
return the new `DbError::DurabilityFailure` for every subsequent durability
request, never falsely reporting durable. On failure the durable frontier is
**not** advanced (`Wal`) and the frame is **not** marked clean (`BufferPool`) —
so recovery still sees a consistent prefix.

**What landed:**
- `error.rs`: `DurabilityFailure(String)` — fatal, session-poisoning.
- `wal.rs`: `Wal::fsync` poisons on `writer.flush()`/`sync_all()` failure and
  refuses to advance `durable_lsn`; once poisoned, every fsync/`sync` fails.
  `arm_fsync_fault()` / `is_poisoned()` for deterministic fault injection.
- `bufferpool.rs`: `flush_page` poisons on `msync` failure and does **not**
  mark the frame clean; `flush_all` fails up-front when poisoned (so a poisoned
  pool never claims a successful flush even with no dirty frames).
  `arm_flush_fault()` / `is_flush_poisoned()`.
- `bufferpool.rs`: **D5 re-verified end-to-end** — the existing flush-time D5
  check is kept, and a `debug_assert!` tripwire was added at the eviction steal
  point in `find_victim` so a future change to the victim filter can't silently
  flush a page ahead of the durable WAL.
- `mmap.rs`: `flush_range` doc now states the fatal-failure contract its caller
  enforces.

**Crash harness (grew, per the gate):** new **P12** —
`p12_fsync_failure_refuses_to_report_success`. Injects a fault at *both*
durability boundaries: (a) a WAL commit fsync fails → the insert returns
`DurabilityFailure`, `durable_lsn` does not advance, and the WAL stays poisoned;
(b) a data-file page flush fails → the flush returns `DurabilityFailure`, the
frame stays dirty, and the pool stays poisoned. Full P-series (P1–P12) +
property test green: `cargo test -p unidb --test crash` = **14 tests**.

**Benchmark (no-regression):** the added work on the happy path is a single
`bool` check per fsync and per `flush_page`. Insert throughput through the now
poison-checked path is unchanged vs. P1.a (`benches/fpi.rs`, release):

| rows | payload | ins/s (P1.a) | ins/s (P1.b) |
|------|---------|--------------|--------------|
| 2000 | 8 B     | 162          | 160          |
| 2000 | 64 B    | 160          | 159          |
| 2000 | 256 B   | 154          | 152          |
| 2000 | 1024 B  | 137          | 137          |

(within run-to-run noise; the write path remains fsync-bound — the poison check
is not on any measurable hot path). Peak memory unchanged (two `bool` fields).

**Locked-decision changes:** none reversed; **D5 strengthened** (fsync-failure
path hardens the WAL-before-page discipline; new steal-point debug assertion).
No format change (`FORMAT_VERSION` unchanged — no on-disk layout touched).

### P1.c — alloc_page remap fix + configurable buffer pool + real FSM   [shipped]   2026-07-08

**PR:** #8 — https://github.com/sagarm85/unidb/pull/8 (Core lane, branch `acid-hardening`)
**Summary:** Removes the growth blocker (roadmap Tier 3, "`alloc_page` re-maps
the whole file per page"). Three changes: (1) the page file now grows in **4 MiB
chunks**, re-creating the mmap only when a new page crosses the chunk boundary,
not once per page (was O(inserts) full-file remaps — O(N²) total, fatal at 100s
of GB); (2) the buffer-pool capacity is **configurable** (`UNIDB_BUFFER_POOL_
PAGES` env / `Engine::open_with_pool_capacity`), default raised 256 → **4096**
frames; (3) a **real free-space map** replaces the linear per-insert page scan,
so picking a page that fits is integer comparisons, not a fetch (8 KiB copy) of
every page.

**What landed:**
- `bufferpool.rs`: `mapped_pages` / `grow_chunk_pages` fields + `ensure_mapped`
  (chunked grow, one remap per chunk); `alloc_page` and `restore_page_image`
  route through it. `logical_page_count` recovers the true high-water mark on
  open by skipping trailing all-zero slack pages (a written page always has a
  non-zero header), so a reopen reuses slack instead of leaking a chunk.
  `page_count()` accessor.
- `lib.rs`: `DEFAULT_POOL_CAPACITY = 4096`, `configured_pool_capacity()` (env),
  `Engine::open_with_pool_capacity`; `open` delegates.
- `heap.rs`: `free_map: HashMap<PageId, usize>` FSM. `find_or_alloc_page` first
  answers from the map (no fetch), probes only *unknown* pages (from the end,
  append locality, caching each), then allocates. `note_free_space` keeps it
  exact after every insert / update-new-version / page compaction — a hint that
  never over-reports, so a chosen page always fits.

**Benchmark** (`benches/scale.rs`, release; fsync-free to expose the O(pages)
effects the end-to-end fsync floor would otherwise hide):

_(A) `alloc_page` throughput — was O(N²) total pre-P1.c (whole-file remap per
call), now flat:_

| pages allocated | pages/sec |
|---|---|
| 10,000  | ~629,000   |
| 50,000  | ~1,045,000 |
| 100,000 | ~1,000,000 |

_(B) heap insert throughput per 50k-row window (deferred WAL, large pool) — does
**not** degrade as the heap grows (a linear-scan FSM would show the opposite):_

| window (rows) | inserts/sec |
|---|---|
| 0–50k    | ~12,200 |
| 50–100k  | ~16,800 |
| 100–150k | ~17,800 |
| 150–200k | ~26,000 |
| 200–250k | ~84,900 |
| 250–300k | ~71,300 |

Point reads at ~300k rows: **~1,140,000 reads/sec** (unaffected by table size).

Throughput is flat-to-rising as the table grows (the rise is OS-cache warmth,
not FSM cost) — the P1.c win is the *absence of degradation*: no per-page
whole-file remap, and no O(pages) fetch-every-page scan per insert. **Peak
memory:** the FSM is one `usize` per heap page (~a few hundred KB at 300k rows /
2k pages); the larger default pool is a config choice (32 MiB at 4096 × 8 KiB),
overridable down via the env var. `BufferPoolFull`-at-scale is gone (already
mitigated by M9's force-WAL-on-evict; the larger pool + chunked file make it a
non-issue).

**Known limitations (documented, not silent):** (1) the FSM is per-`Heap`-
instance in-memory state; the SQL executor reconstructs a `Heap` via
`from_pages` per statement, so a single-row autocommit SQL INSERT rebuilds the
map lazily (bounded: it probes from the last page, usually one fetch) — the raw
`Engine::insert` path (and bulk multi-row statements) keep a warm map. A durable
on-disk FSM fork (Postgres `_fsm`) is a later item. (2) Trailing chunk slack is
reclaimed on reopen but not shrunk mid-session (bounded to one chunk).

**Locked-decision changes:** none. D6 (single file) / D8 (8 KiB pages)
unchanged; no format change (chunk growth is purely a file-sizing strategy,
invisible on disk).

### P1.d — isolation correctness (RC re-evaluation + SSI)   [shipped]   2026-07-08

**PR:** #10 — https://github.com/sagarm85/unidb/pull/10 (Core lane, branch `acid-hardening`)
**Summary:** Closes the isolation Tier-0 hole (D10–D12): conflicts previously
propagated as raw `WriteConflict` regardless of isolation level, and
`SERIALIZABLE` was an unimplemented no-op seam (write-skew possible). Now: (1)
a write-write conflict under `REPEATABLE READ`/`SERIALIZABLE` surfaces as
`SerializationFailure` (the D12-deferred classification); under `READ
COMMITTED` the fresh per-statement snapshot re-reads the latest committed
version (EvalPlanQual via re-scan), so a committed concurrent update no longer
spuriously aborts; (2) **true `SERIALIZABLE` via SSI** — a new
`IsolationLevel::Serializable` tracks rw-antidependencies (Cahill-style pivot
detection) and aborts a transaction that forms a dangerous structure, so
write-skew is prevented.

**What landed:**
- `txn.rs`: `IsolationLevel::Serializable` (fixed BEGIN-time snapshot like RR,
  plus tracking); `SsiState` (per-txn read/write sets + `in_conflict`/
  `out_conflict` flags) on each serializable `Transaction`; `committed_ser`
  map (committed serializable txns kept for edge detection while any
  serializable txn is live). `ssi_note_reads` / `ssi_note_write` form
  rw-edges between concurrent serializable txns; `ssi_is_pivot`; `isolation()`
  accessor. `commit` refuses a pivot with `SerializationFailure` and moves a
  clean commit's sets into `committed_ser`.
- `sql/executor.rs`: `exec_select` / `exec_update` / `exec_delete` feed their
  read sets (`ssi_note_reads`) and write sets (`ssi_note_write`) to the tracker;
  `classify_conflict` maps a heap `WriteConflict` to `SerializationFailure`
  under RR/Serializable (left as-is under RC — see below).
- `lib.rs`: `Engine::commit` turns a pivot `SerializationFailure` into a real
  rollback (undoing the txn's writes) before returning the error, so the caller
  sees a clean, retryable failure.

**Design notes (single-writer model):**
- **RC EvalPlanQual is inherent to the scan-based executor**: each RC statement
  takes a *fresh* snapshot, so an UPDATE/DELETE re-scans and finds the latest
  committed tip — the committed-superseder conflict never reaches `heap.update`.
  The only `WriteConflict` that can fire at RC is against a *still-active*
  concurrent writer, which a no-wait engine (D12) must reject; true
  blocking-then-EvalPlanQual for that case needs a lock wait queue (Phase 5).
  So "no spurious abort at RC" holds for the committed-conflicter case.
- **Reduced SSI** (as the plan allows): row-granularity rw-tracking (no
  predicate locks), so write-skew on existing rows is caught but phantom
  anomalies are not (row-level, like Postgres SI without predicate locks would
  miss). Pivot abort is decided at commit; a write-skew pair can in some
  orderings both abort (sound — never commits a non-serializable schedule —
  but occasionally over-conservative). Tracking is done at the executor
  (statement) granularity where the txn context is available, rather than
  threading a tracker through every `heap` method — the `on_read`/`on_write`
  D11 seam stays in place for finer-grained tracking later.

**Crash harness:** unchanged at **14** (P1–P12). P1.d adds no new durability
mechanism — an SSI/serialization abort is an ordinary transaction rollback
already covered by the existing abort/undo crash paths (P6/P9) — so, like
M1–M8, it adds no crash point (the harness grows only when a new durability
mechanism lands, as it did for P1.a/P1.b).

**Tests** (`lib.rs`): `write_skew_commits_under_rr_but_aborts_under_serializable`
(the canonical SSI test — commits under RR, aborts under SERIALIZABLE);
`read_committed_concurrent_update_does_not_spuriously_abort`;
`repeatable_read_write_over_committed_update_is_serialization_failure`;
`serializable_non_conflicting_transaction_commits` (no over-abort of the common
case). 263 unit + 14 crash + server + workspace green.

**Benchmark (no-regression):** SSI tracking is gated to `Serializable`
transactions — the `ssi` field is `None` for RC/RR and every hook early-returns
before touching a set, so the default RC path and the raw `Engine::insert`
path (which don't route through the SSI hooks at all) are unaffected; the
unchanged `benches/fpi.rs` / `benches/scale.rs` RC numbers stand. For a
`Serializable` transaction the added cost is O(rows in its read+write set) of
`HashSet` inserts and, per write, a scan of concurrent serializable txns'
read sets — paid only by workloads that opt into SERIALIZABLE.

**Locked-decision changes:** none reversed; **D10–D12 completed as originally
designed** (RC re-evaluation + the SSI addition the `on_read`/`on_write` seam
was built for). No format change.

### P1.e — auto-checkpoint (time + WAL-size triggers)   [shipped]   2026-07-08

**PR:** #11 — https://github.com/sagarm85/unidb/pull/11 (Core lane, branch `acid-hardening`)
**Summary:** Closes the last Phase-1 item and bounds WAL growth (roadmap Tier
3). Checkpoint was manual-only, so the WAL — and the P1.a full-page-image volume
it now carries — grew unbounded for the life of a session. The engine now runs
the existing checkpoint path **inline on the writer thread** when either a
**time** trigger (`checkpoint_timeout`, default 60 s) or a **WAL-size** trigger
(`max_wal_size`, default 64 MiB) fires — but only at a **quiescent point** (no
open transaction), so a checkpoint's WAL truncation can never discard an
in-flight transaction's undo records.

**What landed:**
- `wal.rs`: `wal_bytes` running counter (framed bytes since the last
  truncation) + `wal_bytes()` accessor; seeded from the file at open, reset to
  the kept size on `truncate_before`.
- `txn.rs`: `active_count()` (the quiescence gate).
- `lib.rs`: `AutoCheckpointConfig { enabled, timeout, max_wal_size }` (env:
  `UNIDB_AUTO_CHECKPOINT`, `UNIDB_CHECKPOINT_TIMEOUT_SECS`,
  `UNIDB_MAX_WAL_SIZE_BYTES`); `Engine::maybe_auto_checkpoint` called from
  `commit` — checks the gate + triggers, syncs the WAL (so a deferred-sync
  session's pages are durable before `flush_all`, D5), runs `checkpoint()`, and
  bumps a counter. `set_auto_checkpoint_config` / `auto_checkpoint_config` /
  `checkpoints_triggered` API.
- The server writer thread (`server/engine_handle.rs`) owns the `Engine` and
  drives `commit`, so it gets auto-checkpoint for free — no server change.

**Design notes:**
- **Quiescence gate.** `checkpoint::run` truncates *all* WAL before the
  checkpoint LSN; if it ran mid-transaction, an in-flight txn's flushed-but-
  uncommitted pages would lose their undo records and wrongly persist on
  recovery. Gating on `active_count() == 0` makes auto-checkpoint
  unconditionally safe with the existing checkpoint. Cost: a permanently
  open long-lived transaction blocks auto-checkpoint (the same operational
  footgun as a long-lived txn holding back Postgres's checkpointing / vacuum) —
  documented, not silent.
- **Default on** with 60 s / 64 MiB — high enough that no existing unit/crash
  test or short bench trips it (they run in well under 60 s and far under
  64 MiB of WAL), so behavior is unchanged for them; real long-running or
  high-volume sessions get bounded WAL.
- **Throttle.** The checkpoint cadence is itself the throttle: bounded to one
  checkpoint per `max_wal_size` of WAL (which resets on truncation) or per
  `checkpoint_timeout`, and each checkpoint flushes only *dirty* pages (bounded
  by pool size). Intra-checkpoint I/O smoothing is deferred.

**Crash harness:** unchanged at **14** (P1–P12). Auto-checkpoint reuses the
existing (already crash-tested) checkpoint + recovery path (P2/P4) — it changes
*when* a checkpoint runs, not *how* — so it adds no new durability mechanism and
no crash point. The new `auto_checkpoint_fires_on_wal_size_and_data_survives`
test drops + reopens after auto-checkpoints truncated the WAL and asserts all
rows survive (recovery from the checkpointed pages);
`auto_checkpoint_does_not_fire_mid_transaction` proves the quiescence gate.

**Benchmark** (`benches/checkpoint.rs`, release; 3,000 autocommit inserts):

| config | final WAL bytes | checkpoints | rows/s |
|---|---|---|---|
| auto OFF     | 1,169,711 | 0  | 160 |
| auto 64 KiB  | 50,448    | 19 | 160 |
| auto 256 KiB | 154,204   | 4  | 161 |

With auto-checkpoint off the WAL grows with the whole workload (1.17 MB for
3,000 rows, unbounded); with it on the final WAL stays near `max_wal_size` (~50
KB / ~154 KB) regardless of row count — a **~8–23× smaller** WAL, bounded by
config, not data. Throughput is unchanged (~160 rows/s across all three — the
write floor is the per-statement fsync; a checkpoint's flush I/O is amortized
across the ~many commits between triggers). **Peak memory:** unchanged (one
`u64` counter + a `Copy` config struct).

**Locked-decision changes:** none. Extends the existing D3 checkpoint path with
a trigger; no format change. (Segmented WAL — replacing the whole-file rewrite
truncation — is explicitly Phase 6, not this checkpoint.)

---

## Phase 1 complete

All five checkpoints (P1.a–P1.e) shipped. The feature-freeze gate is closed:
torn-page protection (P1.a), fsync-failure handling (P1.b), the `alloc_page`
remap fix + configurable pool + real FSM (P1.c), isolation correctness — RC
re-evaluation + SSI (P1.d), and auto-checkpoint (P1.e). Crash harness grew from
11 to **14** (P11 torn-page, P12 fsync-failure); `FORMAT_VERSION` 3→4;
`clippy -D warnings` + `fmt` clean throughout; no locked decision reversed
(D1/D5/D9/D10–D12/D3 all completed or strengthened). Next per
`docs/backlog/roadmap.md`: Phases 2/3/4 (data model, durable storage, query
power) build on a correctness-solid core.

## P2.a — DECIMAL + TIMESTAMP   [SQL lane — Phase 2 — landing]   2026-07-08

**Branch:** `sql-types` (SQL lane worktree; hand-merged to `main` at land-time
per the roadmap's parallel-lane operating rules). First checkpoint of Phase 2
(`docs/backlog/phase2_data_model.md`), runs disjoint from the Core lane's
Phase 1.
**Summary:** Added the first two "real app" scalar types — exact fixed-point
`DECIMAL(p, s)` (money) and `TIMESTAMP` (time). Both round-trip exactly through
the hand-rolled row encoding, order and compare correctly (including
cross-scale decimals and string↔timestamp predicates), and work under every
M11 constraint (`DEFAULT` / `CHECK` / `PRIMARY KEY` / `UNIQUE`). No storage-core
file touched; `lib.rs` untouched.

**What changed:**
- `catalog.rs`: `ColumnType::Decimal(u8, u8)` (precision, scale) and
  `ColumnType::Timestamp`. `ColumnType` is `Copy`, so no `ColumnDef` derive
  changes.
- `sql/logical.rs`: `Literal::Decimal(i128, u8)` (unscaled value + scale) and
  `Literal::Timestamp(i64)` (micros since Unix epoch, UTC); plus
  `format_decimal` for the JSON/DTO boundary.
- `sql/datetime.rs` (new): dependency-light timestamp parse/format via
  Hinnant's `days_from_civil`/`civil_from_days` — no `chrono`. Accepts
  `YYYY-MM-DD[ |T]HH:MM:SS[.ffffff][Z]` and date-only; UTC only in v1.
- `sql/parser.rs`: `DECIMAL`/`NUMERIC`/`DEC`/`BIGDECIMAL`/`BIGNUMERIC` and all
  `TIMESTAMP` zone variants map to the new `ColumnType`s (precision 1..=38,
  `0 <= scale <= precision` validated at `CREATE TABLE`); numeric literals with
  a fractional point parse to exact `Literal::Decimal` (scale as written, never
  via `f64`), including unary-minus.
- `sql/executor.rs`: encode/decode tags **6** (Decimal: 16-byte LE `i128` +
  1-byte scale) and **7** (Timestamp: 8-byte LE `i64`); `coerce_value` rescales
  a decimal literal to the column's exact `(p, s)` (widening multiplies,
  narrowing allowed only when dropped digits are zero, precision cap enforced)
  and parses a timestamp string; `compare` orders decimals across scales via
  cross-multiplication (overflow → error, never a wrong answer) and parses a
  string operand against a `TIMESTAMP` on demand.
- `queue/payload.rs`, `server/dto.rs`: additive match arms rendering
  `Decimal`/`Timestamp` as **strings** so no precision is lost crossing into
  JSON (both are exhaustive `Literal` matches that had to keep compiling).

**Tests:** 8 `sql::datetime` unit tests (epoch/pre-epoch/leap-day/fractional/
ordering/garbage), executor round-trip + constraint tests (exact decimal
round-trip, excess-fractional-digit + precision-overflow rejection, decimal
range/equality predicates across scales, DEFAULT/CHECK/UNIQUE on decimals,
timestamp round-trip + ordering + PK uniqueness across `' '`/`'T'` spellings,
invalid-timestamp rejection, decimal+timestamp survive-reopen), parser tests
(DECIMAL/NUMERIC/bare-DECIMAL, TIMESTAMP, bad precision/scale, decimal literal
scale, negative decimal), and `format_decimal` rendering. `cargo test -p unidb`
260 → 285 unit tests, all green; `--workspace` and `--features server` green;
crash harness 12/12 (storage untouched).

**Benchmark note (§6):** new scalar types are a functional capability, not a
throughput workload — no new benchmark table. Row size grows by fixed-width
fields only (17 bytes/decimal, 9 bytes/timestamp) with no hot-path algorithm
change; existing INSERT/SELECT benchmarks are unaffected.

**Known limitations / tech debt (new in P2.a):** `NUMERIC` precision capped at
`i128` (~38 digits; arbitrary-precision out of scope); timestamps are UTC-only
(`TIMESTAMPTZ` normalizes to UTC, original zone not tracked); no `DATE`/`TIME`
yet (P2.b); no BTree index on `DECIMAL`/`TIMESTAMP` yet (`OrderedValue` doesn't
cover them — they're skipped, not errored). All tracked in the Phase 2 spec.

**Locked-decision changes (if any):** none. Row-encoding tags 6/7 are purely
additive and forward-compatible (D4) — old rows never carry them and still
decode; an older binary meeting a tag-6/7 row fails safe with a decode error,
never a silent misread. **`FORMAT_VERSION` deliberately NOT bumped**: the tag
set only grows, no old file becomes unreadable, and a bump here would needlessly
reject pre-P2.a databases and collide with the parallel Core lane's Phase 1
version work. (Reserved the bump for a genuinely incompatible change.)

---

## P2.b — FLOAT / UUID / BYTEA / DATE / TIME   [SQL lane — Phase 2 — landing]   2026-07-08

**Branch:** `sql-types` (SQL lane worktree). Second Phase 2 checkpoint, same
four-touchpoint pattern as P2.a.
**Summary:** Five more scalar types — `FLOAT` (f64), `UUID` (16 bytes), `BYTEA`
(opaque bytes), `DATE`, `TIME`. Each round-trips exactly, orders/compares
correctly (including string-operand coercion), and works under M11 constraints.

**What changed:**
- `catalog.rs`: `ColumnType::{Float, Uuid, Bytea, Date, Time}`.
- `sql/logical.rs`: `Literal::{Float(f64), Uuid([u8;16]), Bytea(Vec<u8>),
  Date(i32), Time(i64)}`.
- `sql/datetime.rs`: `parse_date`/`format_date` (days since epoch),
  `parse_time`/`format_time` (micros since midnight).
- `sql/parser.rs`: `FLOAT`/`REAL`/`DOUBLE PRECISION`/... → `Float`; `UUID`;
  `BYTEA`/`BLOB`/`BINARY`/`VARBINARY` → `Bytea`; `DATE`; `TIME`.
- `sql/executor.rs`: row-encoding tags **8** (Float, 8 B LE), **9** (Uuid, 16 B),
  **10** (Bytea, len-prefixed), **11** (Date, i32 LE), **12** (Time, i64 LE);
  coercion (float widens from int/decimal; uuid/bytea/date/time parse from a
  string literal); comparison (float via f64 with NaN-unordered → false;
  uuid/bytea/date/time ordering + on-demand string parse); `parse_uuid`/
  `format_uuid`, `parse_bytea`/`format_bytea`.
- `queue/payload.rs`, `server/dto.rs`: additive arms (float as JSON number;
  uuid/bytea/date/time as canonical strings).

**Design notes:** `BYTEA` text input is Postgres `\xHEX` or the string's raw
UTF-8 bytes (permissive, documented). `UUID` accepts hyphenated or bare 32-hex,
renders canonical lowercase hyphenated. No BTree index on the new types yet
(`OrderedValue` doesn't cover them; they're skipped in `build_indexed_columns`,
not errored).

**Benchmark note (§6):** functional type additions; fixed-width row growth only,
no hot-path algorithm change — no new benchmark table.
**Tests:** +2 `datetime` (date/time), +5 executor (round-trip / order /
UUID-PK / BYTEA hex+raw), +1 parser. `cargo test -p unidb` 277 → 285.
**Locked-decision changes:** none. Tags 8–12 additive/forward-compatible (D4);
no `FORMAT_VERSION` bump (same reasoning as P2.a).

---

## P2.c — ALTER / DROP / TRUNCATE + transactional DDL   [SQL lane — Phase 2 — landing]   2026-07-08

**Branch:** `sql-types`. Third Phase 2 checkpoint — schema evolution.
**Summary:** `ALTER TABLE ADD COLUMN` (with `DEFAULT`), `ALTER TABLE DROP
COLUMN` (logical tombstone), `DROP TABLE`, `TRUNCATE`, plus request-level DDL
rollback so a failed multi-statement request leaves the schema untouched.

**What changed:**
- **ADD COLUMN**: appended physically; pre-existing rows (which lack bytes for
  the new trailing column) decode it as its coerced `DEFAULT`/NULL — no heap
  rewrite. `NOT NULL` without `DEFAULT` is rejected.
- **DROP COLUMN**: **logical tombstone** (`ColumnDef.dropped`, `#[serde(default)]`).
  The column keeps its physical row slot so rows written before the drop still
  decode positionally, but it is hidden from `SELECT *`, unreferenceable by
  name, and written NULL on new inserts. Every row-handling path (project /
  order / column-index / apply-defaults / not-null / check / unique) is now
  tombstone-aware. Dropping a constraint-referenced or last-visible column is
  rejected.
- **DROP TABLE / TRUNCATE**: catalog removal / page-list clear. Orphaned heap
  pages are reclaimed once Phase 1's FSM/free-page list lands (same accepted
  tradeoff as pre-vacuum bloat). System tables (`__*`) are protected.
- **Transactional DDL (request-level)**: `execute_sql`/`run_bound_plans`
  snapshot the catalog root and restore it (`Engine::restore_catalog_root`) if
  any statement of a `;`-separated request fails — the catalog persists
  eagerly (non-MVCC, a documented M1 limitation), so this manual restore is
  what makes failed DDL roll back; heap writes are undone by the caller's txn
  abort. **Full crash-safe, user-transaction-scoped catalog redo/undo through
  recovery is deferred** — it needs a `recovery.rs` hook, which is Core-lane
  territory; the mechanism (catalog-root snapshot/restore) is in place for
  whoever wires it.
- `sql/logical.rs`: `LogicalPlan::{AlterTableAddColumn, AlterTableDropColumn,
  DropTable, Truncate}`; `sql/parser.rs`: the matching `Statement` handlers;
  `ExecResult::{AlteredTable, DroppedTable, Truncated}` + server DTO arms.

**lib.rs impact:** a minimal additive guard on `execute_sql`'s loop (catalog
snapshot + restore-on-error) plus one new helper method — no restructuring.
**Tests:** executor ALTER/DROP/TRUNCATE incl. the **middle-column alignment
hazard** (pre-drop rows must still read the right columns), `DROP COLUMN IF
EXISTS`, system-table rejection; lib DDL-rollback + survive-reopen; parser DDL.
285 → 294 unit tests.
**Locked-decision changes:** none. `ColumnDef.dropped` / `serde` catalog fields
are forward-compatible additions (same discipline as M4/M11).

---

## P2.d — sequences / SERIAL   [SQL lane — Phase 2 — landing]   2026-07-08

**Branch:** `sql-types`. Fourth Phase 2 checkpoint — surrogate keys.
**Summary:** `SERIAL`/`BIGSERIAL`/`GENERATED ... AS IDENTITY` columns auto-fill
from a durable, monotonic per-column counter that survives reopen.

**What changed:**
- `catalog.rs`: `ColumnConstraints.identity` flag; `TableDef.serial_next`
  (`HashMap<column, i64>`, `#[serde(default)]`) — the durable counter,
  crash-safe via the same WAL-logged catalog page write as any catalog change;
  `Catalog::alloc_serial` (monotonic, i64-overflow-checked, persists per call).
- `sql/parser.rs`: `SERIAL`/`BIGSERIAL`/`SMALLSERIAL` (custom types) and
  `GENERATED ... AS IDENTITY` → `Int64` identity column.
- `sql/executor.rs`: `exec_create_table` validates identity columns are `Int64`
  and seeds the counter at 1; `exec_insert`'s `fill_serials` allocates the next
  value for any omitted/NULL identity column before DEFAULT/NOT NULL run.

**Design notes:** single-writer serialization guarantees no duplicate ids. An
explicit value is honored as-is and does **not** advance the counter (matching
Postgres `SERIAL` — a documented sharp edge). Persist-per-allocation keeps the
sequence crash-safe; batching is a future optimization.
**Tests:** monotonic increment, explicit value + PK conflict, `GENERATED AS
IDENTITY`, non-integer rejection, survives-reopen. 294 → 303 unit tests.
**Locked-decision changes:** none.

---

## P2.e — prepared statements + bind parameters   [SQL lane — Phase 2 — landing]   2026-07-08

**Branch:** `sql-types`. Fifth Phase 2 checkpoint — **closes the SQL-injection
surface** and enables parse-once/execute-many.
**Summary:** `$n` placeholders + a positional values array. A bound value is
always *data*, never re-parsed as SQL.

**What changed:**
- `sql/logical.rs`: `Literal::Param(usize)` placeholder + `bind_params(plan,
  params)` — substitutes every `$n` before the plan reaches the executor;
  errors on an out-of-range index. No `Param` survives into encoding /
  comparison / the wire.
- `sql/parser.rs`: `$n` (`Value::Placeholder`) → `Literal::Param`.
- `lib.rs`: `Engine::execute_sql_params` (injection-safe entry point) and
  `prepare()`/`execute_prepared()` (parse once, execute many) over a shared
  `run_bound_plans` loop (bind → RLS → execute → request-level DDL rollback on
  failure). New `Prepared` type.
- `server/`: `SqlRequest.params` + `json_to_literal`;
  `EngineHandle::execute_sql_params` + writer-thread command;
  `post_sql` routes parameterized requests through the writer thread with
  values bound as data.
- Defensive `Literal::Param` arms on the exhaustive matches (`encode_row` /
  `literal_to_json` / `row_to_json`) — unreachable after binding, benign
  (never panic; `encode_row` uses a `debug_assert` + NULL fallback).

**Injection proof:** a value like `"'; DROP TABLE t; --"` bound via
`execute_sql_params` matches/inserts only that literal string; the table is
untouched (tested end-to-end).
**Tests:** injection-as-data, out-of-range error, prepared-plan reuse, parser
placeholders, `bind_params` unit, `json_to_literal` + `SqlRequest` param
defaults. 303 → 309 unit tests (+2 server-feature).
**Docs:** `docs/REST_API.md` documents the `params` field on `POST /sql`.
**Locked-decision changes:** none.

---

## Phase 3 — Multi-model durable storage (Core lane, `durable-storage`)

The moat: kill the "rebuild every secondary index on open" tax (O(all data)
startup, RAM-bound) by making the indexes durable on disk, and own the AI /
big-file story. Blueprint: `docs/backlog/phase3_durable_storage.md`. Serial Core
lane; one PR per checkpoint (P3.a → P3.d).

### P3.a — Durable paged WAL-logged B-Tree   [Core lane — Phase 3 — shipped]   2026-07-08

**Branch:** `durable-storage`. First Phase 3 checkpoint — the B-Tree becomes the
first **durable, crash-recovered, never-rebuilt-on-open** secondary index.
**Summary:** the M6 in-memory `BTreeMap` is replaced by an on-disk B+tree
(`DiskBTree`) whose nodes are pages in the shared page store, buffer-pool-managed,
and WAL-logged as full node-page images. `Engine::open` reads it straight from a
stable meta page — no heap rescan, no rebuild.

**Design (the load-bearing choices):**
- **Node pages ride the existing page machinery.** Each node/meta page carries
  the standard 28-byte page header (page_id / type / crc / lsn), so the buffer
  pool's CRC + D5 (WAL-before-page) discipline applies unchanged; the B+tree
  payload lives in the body. New `PAGE_TYPE_BTREE`.
- **Full node-page-image WAL logging** (new redo-only `WAL_INDEX`, same proven
  shape as `WAL_FPI` / `WAL_VACUUM` full-image). Each `insert`/`remove` is **one
  mini-transaction** bracketing every page it touches (a leaf write, or a
  split-chain + root-repoint). Recovery redoes all pages of a committed index
  mini-txn or none — atomic. Idempotent, LSN-stamped, last-writer-in-LSN-order
  wins; index pages never overlap heap pages, so no LSN gate is needed.
- **No undo, proven safe.** A secondary-index entry is only ever a *hint*,
  re-validated against MVCC visibility in `try_exec_select_btree`, so a stale /
  extra entry (from an aborted or incomplete write) is harmless. The one
  dangerous case — a committed, MVCC-visible heap row with no index entry (a
  false negative) — cannot happen: the index mini-txn fsyncs during statement
  execution, *before* the user txn reaches `WAL_TXN_COMMIT`, so any committed
  row's index entry is already durable.
- **Stable meta page.** A per-index meta page (id stored once in the catalog as
  `ColumnDef.index_root`, never changes) points at the current root, so a root
  split repoints the meta page in place — never a catalog rewrite. `Engine::open`
  is O(1): read catalog → meta → root.
- **Moved off the async worker** onto the synchronous writer/read path (like
  `EdgeIndex`): the executor inserts durable entries inline
  (`apply_durable_btree_writes`) and reads via `DiskBTree::search`; vacuum
  scrubs the tree directly (`DiskBTree::remove`, reading each dead row's key via
  the new `Heap::get_raw` before the slot is reused). Removed from
  `rebuild_secondary_indexes`; `IndexKind::BTree` no longer reaches
  `index_worker.rs`.

**v1 simplifications (documented, not silent):** deletes don't merge/rebalance
underfull nodes (an emptied leaf stays linked — wastes space, never wrong; the
tree only grows); one fsync per key insert, so an indexed INSERT pays the heap
fsync **plus** one index fsync (batched behind a single fsync in the server's
group-commit deferred-sync mode); `DROP INDEX` pages leak until the FSM reclaims
them, exactly like `DROP TABLE` heap pages.

**Benchmark — the Phase-3 gate (`benches/durable_index.rs`, `Engine::open` cost
vs. indexed-row count; Apple Silicon, real fsync):**

| rows | B-Tree open (ms) — durable, P3.a | HNSW open (ms) — rebuilt on open |
|------|----------------------------------|----------------------------------|
| 1,000 | 2.862 | 2.941 |
| 3,000 | 2.395 | 3.217 |
| 6,000 | 2.299 | 3.416 |

The number to read is the **scaling**: the durable B-Tree column is flat
(≈constant, O(1) open — no heap rescan), while the still-rebuilt-on-open HNSW
column rises with row count (the synchronous heap rescan that re-enqueues every
row on open — exactly the O(data) startup Phase 3 kills). Peak RSS is unchanged
(same fixed-size, mmap-backed buffer pool; a point lookup touches only a
root→leaf path, not O(data)).

**Crash safety:** new crash point **P13** builds a durable tree past several
splits, then **wipes the entire data file** and proves recovery reconstructs the
whole tree from the WAL alone — every key still findable. Crash harness **14 →
15**.

**Tests:** module-level insert/search/range/split/text-key/remove +
reconstruct-from-meta-page (`btree_index.rs`); aborted insert never surfaces via
the index and durable reopen without rebuild (`tests/btree_mvcc.rs`);
`engine_restart_btree_index_is_durable_no_rebuild` + pre-Ready equality
correctness (`tests/index_rebuild.rs`); differential index-vs-full-scan and
RLS-respecting index path (`lib.rs`). 316 → 324 default-feature unit tests + the
new crash point; all green, clippy `-D warnings` + fmt clean across the
workspace.

**Locked-decision impact:** D1 / D4 / D5 / D9 strengthened (indexes are now
WAL-logged + crash-recovered; tuple format unchanged; new record kind + page
type; `FORMAT_VERSION` **4 → 5**). No decision reversed.
**PR:** _pending._

---

### P3.b — Durable inverted (full-text) + edge index; CSR retired   [Core lane — Phase 3 — shipped]   2026-07-08

**Branch:** `durable-storage`. Second Phase 3 checkpoint — the remaining
rebuilt-on-open indexes that map a key to a posting list of `RowId`s become
durable, **reusing P3.a's `DiskBTree` + `WAL_INDEX` machinery wholesale** (no new
record kind, page type, or `FORMAT_VERSION` bump).
**Summary:** full-text (inverted) and the edge-adjacency index are now durable
on-disk B+trees, read from disk on open; the M7 CSR index is retired.

**Design (the reuse insight):** both a full-text index (token → docs) and an
edge index (from_id → edge rows) are the same shape a `DiskBTree` already is —
a key mapped to many `RowId`s. So neither needed a new structure:
- **Full-text** (`sql/executor.rs`, `fulltext.rs`): on write, `apply_durable_
  index_writes` tokenizes the text (`fulltext::tokenize`, now `pub(crate)`) and
  inserts one `(OrderedValue::Text(token), RowId)` entry per token; `CREATE
  INDEX ... USING FULLTEXT` builds + backfills the tree the same way BTree does.
  New read path **`Engine::search_fulltext`** tokenizes the query, intersects
  each token's `search_eq` posting list (AND-only, M2.c semantics), and
  MVCC-resolves survivors — the durable index previously had *no* query surface.
- **Edge index** (`lib.rs`, `graph/edges.rs`, `graph/executor.rs`,
  `graph/index.rs`): `__edges__.from_id` becomes a real durable `BTree` index
  (`ensure_edge_index` at open creates/loads it, caching the meta page on the
  `Engine` as `edge_index_meta`). `create_edge`/`delete_edge` maintain it via
  `DiskBTree::insert`/`remove(OrderedValue::Int(from_id), rid)`; `edges_from`
  and the Cypher executor read it via `search_eq`. The in-memory `EdgeIndex`
  struct and `rebuild_edge_index` are gone. Vacuum scrubs it through the same
  generic durable-index path (from_id is now `IndexKind::BTree`).

**CSR retired (recorded decision, evidence-based):** `csr_index.rs` was
consulted by no read path after M7's own "prefer CSR for traversal" wiring was
reverted (a self-visibility bug found during M8 merge verification — see the M7
entry's correction), and adjacency is now served durably by the edge index. So
its rebuild-on-open (`rebuild_csr_index`) and warm-keeping (`IndexedColumn::
Edge` sends) were removed. The module + `benches/graph.rs` remain (the CSR-vs-
naive adjacency benchmark is still a valid measurement) but are no longer wired
into the runtime. This is a dead-code retirement, not a §3-locked reversal.

**The async index worker now serves only the vector (Hnsw) index** — B-Tree
(P3.a), full-text, and edge indexes are all durable/synchronous. `index_worker.
rs` shed its `FullText`/`Csr`/`Edge`/`Text`/`Ordered` variants and the CSR
debounce machinery; `SecondaryIndex`/`IndexedColumn` are single-variant. (P3.c
will make vector durable too and retire the worker.)

**Benchmark (`benches/durable_index.rs`, edge-index reopen cost vs. committed
edge count; Apple Silicon, real fsync):**

| edges | edge-index open (ms) |
|-------|----------------------|
| 500   | 2.373 |
| 2,000 | 2.346 |
| 5,000 | 2.038 |

Flat reopen time (≈2.0–2.4 ms, independent of edge count) ⇒ the durable edge
index is not rebuilt on open (before P3.b it was an O(edges) synchronous heap
scan on every `Engine::open`).

**Crash safety:** new crash points **P14** (durable full-text: committed rows +
their FULLTEXT index survive a crash, `search_fulltext` works on reopen) and
**P15** (durable edge index: committed edges survive, `edges_from` works on
reopen) — both proving no rebuild + WAL recovery through the real Engine API.
Crash harness **15 → 17**.

**Tests:** `search_fulltext` (single/multi-term AND, reopen), durable full-text
reopen (`tests/index_rebuild.rs`), edge-index reopen + traversal
(`tests/graph_rebuild.rs`, `graph_mvcc`), P14/P15. Worker tests trimmed to the
vector kind. All default-feature + server + workspace suites green; clippy
`-D warnings` + fmt clean.

**Locked-decision impact:** none new beyond P3.a (same `WAL_INDEX`/D5/D9). No
`FORMAT_VERSION` bump. No decision reversed (CSR retirement is not a §3 item).
**PR:** _pending._

---

### P3.c — On-disk vector index (SPIKE)   [Core lane — Phase 3 — spike complete]   2026-07-08

> **Superseded by the production entry below (2026-07-09):** the spike's
> `DiskIvfIndex` is now the live vector index — durable centroids, `CREATE INDEX`/
> `NEAR` wired, async worker retired, crash point P17. This spike record is kept
> for the approach-selection rationale and the recall-validation numbers.

**Branch:** `durable-storage`. The blueprint marks this **research-grade** and
mandates a **spike that validates recall@k before committing** — so the P3.c
deliverable is the spike + recommendation; the production wiring is a separate
follow-up PR, deliberately not rushed.

**Approach chosen: on-disk IVF-Flat** (over DiskANN/Vamana for v1). The insight:
an IVF index's only on-disk state is a **cell posting list `cell_id → [RowId]`**,
which is *exactly* a `DiskBTree` (P3.a) — so it is already durable, WAL-logged,
crash-recovered, buffer-pool-managed, and vacuum-scrubbable, with **no new
storage format**. The only new in-RAM state is the centroid table (`nlist·dim`
f32s — **bounded, independent of corpus size**, vs. HNSW's O(corpus) graph).
Vectors stay in the heap (IVF-Flat re-ranks with exact distances). DiskANN is
parked as a higher-recall option behind the same interface. Prototype:
`src/disk_vector.rs` (`DiskIvfIndex`: k-means `train`, `insert`, `search`).

**Recall validated (`benches/vector_recall.rs`)** — 1,200 vecs × 32d, 30
clusters, 100 queries, k=10, nlist=32, brute-force ground truth:

| index | recall@10 | q-latency | build | RAM |
|---|---|---|---|---|
| HNSW (in-RAM, rebuilt-on-open) | 1.000 | ~26 µs | **30,223 ms** | O(corpus) |
| IVF-Flat `nprobe=1` | 0.957 | 8 µs | 24 ms | **4,096 B** |
| IVF-Flat `nprobe=4` | **1.000** | 31 µs | 24 ms | 4,096 B |
| IVF-Flat `nprobe=8/16/32` | 1.000 | 59/113/216 µs | 24 ms | 4,096 B |

IVF-Flat reaches exact top-10 at `nprobe=4` (a few of 32 cells) at 4 KB RAM; the
HNSW *build* took 30 s for 1,200 vectors (the M2 rebuild-per-upsert pathology —
exactly the O(corpus)-on-open cost Phase 3 kills).

**Bug found + fixed by the spike (affects P3.a/P3.b):** an early run capped IVF
recall at 0.912 even probing all cells — a real `DiskBTree` bug where a
duplicate-key run **straddling a leaf boundary** was under-returned (`search_eq`
could land mid-run and stop early). Fixed: `find_leaf` now descends to the
**leftmost** candidate leaf and `search_eq`/`remove` walk the leaf links until
they pass the key. Regression:
`btree_index::heavily_duplicated_key_spanning_leaves_returns_all` (a key with
3,000 duplicates spanning ~7 leaves). This mattered for real workloads: a
full-text token in many docs, a graph hub, or a BTree value on many rows.

**Production follow-up (its own PR):** persist centroids in a meta page +
re-train as a maintenance op; wire `CREATE INDEX ... USING HNSW`/`IVF` →
`DiskIvfIndex`, route `NEAR` through it, retire the async worker; crash point
P16; larger-corpus sweep. Recommendation + numbers: `docs/design/p3c_vector_spike.md`.

**Tests:** `disk_vector` module (IVF finds nearest on separated clusters; RAM
bounded by nlist not corpus) + the DiskBTree duplicate regression. All suites
green; clippy `-D warnings` + fmt clean.
**Locked-decision impact:** none. No `FORMAT_VERSION` bump.
**PR:** _spike; superseded by the production entry below._

---

### P3.c (production) — Durable vector index live; async worker retired   [Core lane — Phase 3 — shipped]   2026-07-09

**Branch:** `p3c-vector-production`. Promotes the P3.c spike's `DiskIvfIndex` into
the live vector index, closing Phase 3: **`Engine::open` now does ZERO index
rebuilding for every index type — the O(1)-open moat is real, and the async index
worker is gone.**

**What shipped:**
- **Durable, crash-recovered centroids.** `DiskIvfIndex` is now a stateless handle
  over a **stable meta page** (id in `ColumnDef.index_root`, exactly like
  `DiskBTree`). The meta page records metric/dim/nlist/nprobe + the postings
  tree's meta page + the head of a **WAL-logged centroid page chain**; every
  operation reloads the bounded (`O(nlist·dim)`) centroid table from disk. All
  pages use `PAGE_TYPE_BTREE` + `WAL_INDEX` full-page images, so they are
  crash-recovered identically to `DiskBTree` nodes — **no new record kind, page
  type, or `FORMAT_VERSION` bump.**
- **`CREATE INDEX ... USING HNSW` (and a new `USING IVF` alias) → durable index.**
  Trains centroids from the committed rows (`nlist ≈ √rows` capped at 256, a
  recall-favoring `nprobe`), persists meta + centroids, inserts each row into its
  cell. Empty-table create → one origin cell (correct-but-flat until re-created,
  documented). `Hnsw` now *denotes* the durable IVF-Flat index (HNSW-the-graph
  retired); the catalog/SQL keyword is unchanged for compatibility.
- **`NEAR` routes through the durable index.** Probe the `nprobe` nearest cells'
  posting lists → fetch candidate rows from the heap → **exact re-rank** by the
  index metric → the same MVCC-visibility / RLS / AND'd-predicate re-check as
  before (identical over-fetch-then-filter contract).
- **Maintenance + vacuum.** `apply_durable_index_writes` inserts into the IVF on
  every INSERT/UPDATE; vacuum's aliasing gate scrubs it via `DiskIvfIndex::remove`
  before a reclaimed slot can be reused.
- **Async worker retired.** `rebuild_secondary_indexes` deleted; `index_worker.rs`
  removed; `IndexHandle`/`IndexMsg`/`SecondaryIndex`/`build_indexed_columns`/
  `send_index_upserts` gone. `IndexStatus` moved to `catalog.rs`; a durable index
  is always `Ready` (computed from the catalog — the REST `GET
  /indexes/.../status` route is unchanged). `Engine` lost its worker field + Drop
  shutdown.

**Recall / latency (`benches/vector_recall.rs`, extended):**

| corpus | index | recall@10 | q-latency | build | RAM |
|---|---|---|---|---|---|
| 1,200×32d | HNSW (retired baseline) | 1.000 | ~25 µs | 30,374 ms | O(corpus) |
| 1,200×32d | IVF-Flat nprobe=4 | **1.000** | ~36 µs | ~34 ms | **4,096 B** |
| 1,200×32d | IVF-Flat reopen-by-meta-page (no rebuild) | 1.000 | — | — | 4,096 B |
| 20,000×64d | IVF-Flat nprobe=16 | **1.000** | ~400 µs | ~983 ms | **36,096 B** |

IVF-Flat **matches HNSW's recall (1.000)** at bounded RAM, and a fresh handle over
the same meta page answers identically — proving no rebuild on open.

**Crash harness 18 → 19.** New point **P17**: build a durable vector index over a
multi-cell corpus, "crash" without a checkpoint, reopen, and confirm NEAR returns
the exact nearest neighbor and exact top-5 (recall intact) from the WAL-recovered
meta/centroid/posting pages — never rebuilt.

**Tests:** `disk_vector` module (create/insert/search, reopen-by-meta-page,
empty-table flat-but-correct, remove); rewritten vector durability tests
(`tests/index_rebuild.rs`, `lib.rs`); `tests/vector_mvcc.rs` (aborted insert never
surfaces via NEAR — now synchronous); executor NEAR tests. `cargo test -p unidb`
(319 unit + 19 crash + integration), `--features server`, and `--workspace` all
green; clippy `-D warnings` + fmt clean.
**Locked-decision impact:** none new (D1/D5/D9 already covered durable WAL-logged
indexes in P3.a). No `FORMAT_VERSION` bump.
**PR:** _this PR — Phase 3 complete._

---

### P3.d — Large-object (big-file) storage   [Core lane — Phase 3 — shipped]   2026-07-08

**Branch:** `durable-storage`. The "big file" differentiator: store values too
large for an 8 KiB tuple **out of line, chunked, and streamed** — never loading a
whole multi-GB value into RAM.
**Summary:** a large object is a sequence of ~7 KiB **chunk rows** in a `__lobs__`
system heap table, indexed by a durable `DiskBTree` on `lob_id`.

**Design (maximal reuse, zero new format):** the key decision was to *not* invent
a bespoke overflow-page format. A large object's chunks are **ordinary MVCC/WAL
heap rows** (like `__edges__`/`__events__`), so:
- **Atomic with the transaction** — chunks are written under the caller's `xid`,
  so a blob and its owning row commit or abort together, with zero new txn code.
- **Crash-recovered for free** — chunk rows ride the heap+WAL recovery path
  (crash point **P16**: commit a blob, crash without a checkpoint, reopen, stream
  it back byte-for-byte).
- **Vacuum-reclaimable** — a deleted/orphaned blob's chunk rows are physically
  reclaimed by the ordinary heap vacuum (M10).
- **O(chunks-of-this-blob) locate** — a durable `DiskBTree` on `lob_id` (reuses
  P3.a) maps a blob to its chunk `RowId`s; itself crash-recovered, never rebuilt.

**Streaming (the "without OOM" gate):** `Engine::put_large_object(xid, impl
Read)` pulls one ~7 KiB chunk from the reader and inserts it, repeat;
`read_large_object(xid, lob_id, impl Write)` fetches one chunk row at a time and
writes it to the sink. **One chunk is resident at a time on both paths** — a
multi-GB value never loads whole. `lob_id` is allocated from a counter derived at
open from `__lobs__`'s max (crash-safe, like `next_event_seq`).

**Files:** new `src/large_object.rs` (`LobStore`, `__lobs__` table def,
`ensure_lobs_table`); `lib.rs` (Engine API + open wiring + `derive_next_lob_id`);
`tests/large_object.rs`; `tests/crash` (P16).

**Tests:** 5 MiB store→stream round-trip verified by an O(1)-memory checksum sink
(≈750 chunks over many heap pages); atomicity (an aborted blob is MVCC-invisible,
a committed one fully readable); vacuum reclaims a deleted 400 KiB blob's chunks;
crash-recovery (P16). Crash harness **17 → 18**. All default + server + workspace
suites green; clippy `-D warnings` + fmt clean.

**Deferred (documented, not silent):** transparently toasting a large inline
`BYTEA` column value to this store (this is the explicit large-object API that
path would call); streaming REST upload/download routes — server-side streaming
through the single writer thread needs a chunked command path, a real design
piece rather than buffering a whole blob in the writer.

**Locked-decision impact:** D4 (tuple stays forward-compatible — large objects
are separate `__lobs__` rows, no tuple format change). No `FORMAT_VERSION` bump.
No decision reversed.
**PR:** _pending._

---

## Phase 4 — Query power (SQL lane)   [DONE]   2026-07-09

**PR:** _pending (branch `query-power`; one PR for the whole phase, P4.a–P4.e)._
**Summary:** Turns unidb from a single-table filter/project engine into a real
query engine — joins (hash + Grace spill / sort-merge / index-nested-loop),
aggregation + `GROUP BY`/`HAVING` + `ORDER BY` (external merge-sort spill) +
`DISTINCT` + `LIMIT`/`OFFSET`, scalar/`IN`/`EXISTS` subqueries (correlated +
uncorrelated) + `WITH` CTEs, durable `ANALYZE` statistics + a cost-based
optimizer (Selinger left-deep DP join order + index-vs-scan), and
`EXPLAIN [ANALYZE]`. Additive: a trivial single-table `SELECT` keeps its
original fast path; anything richer routes through a new `LogicalPlan::Query`
physical operator tree. Correctness is checked **differentially against SQLite**.

**Benchmarks** (`cargo bench --bench tpch`, release, Apple Silicon macOS,
real fsync per commit; TPC-H subset — 200 customers, 2,000 orders, 6,000
lineitems; `ANALYZE`d; 30 iterations per query):

| Query                                             | p50 (ms) | p99 (ms) | rows | plan |
|---------------------------------------------------|----------|----------|------|------|
| Q1 join + selective filter (orders⋈customer)      | 0.675    | 1.578    | 10   | IndexScan(customer.id) ⋈ orders |
| Q2 `GROUP BY` aggregate (orders by customer)       | 0.474    | 0.757    | 200  | Scan → HashAggregate |
| Q3 3-way join + `GROUP BY` + `SUM` (lineitem⋈orders⋈customer) | 2.496 | 3.767 | 25 | hash joins → HashAggregate |
| Q4 `ORDER BY … DESC LIMIT 10`                      | 0.196    | 0.253    | 10   | Scan → Sort → Limit |

**Optimizer decision (from `EXPLAIN`, same run):**
- selective `WHERE customer.id = 42` → `IndexScan customer on id =` (est_rows=1)
- broad `WHERE customer.id > 0` → `Scan customer` + `Filter` (est_rows=66)

i.e. the cost model picks the index plan when selective and the full scan when
not — the P4.d crossover, visible in the plan the query actually runs.

**Peak memory:** not per-query-instrumented in this bench (a follow-up). By
construction the executor materializes each operator's output bounded by its
result cardinality, and the two unbounded-intermediate operators — hash join
and `ORDER BY` — **spill to disk** past a row budget (`UNIDB_HASH_JOIN_MEM_ROWS`
/ `UNIDB_SORT_MEM_ROWS`, unit-tested via forced-spill), so a large join/sort
does not hold the whole intermediate in RAM. Process RSS on this dataset stays
in the tens-of-MB range consistent with prior milestones (~28–40 MB).

**Baseline (honesty, per CLAUDE.md §6):** the baseline here is **correctness,
not throughput** — join/aggregate/subquery results are asserted **equal to
SQLite** on the same data (`tests/{join,aggregate,subquery,optimizer}.rs`,
`rusqlite` bundled, a dev-dependency only). The above latencies are unidb's own
single-node numbers, not a comparison; the §6 cross-domain "replaced stack"
headline (unidb-in-one-commit vs Postgres + vector store + graph DB + queue)
remains a separate, deferred effort. This bench measures the query engine this
phase built, on its own, with no aspirational claims.

**Crash harness:** unchanged at **19** — Phase 4 added no new storage mechanism
(joins/aggregates are read-side; `ANALYZE` stats ride the existing WAL-logged
catalog page). All suites green: `cargo test -p unidb` (19 result-groups ok),
`--features server` (28 ok), `--test crash` (19), `clippy -D warnings` + `fmt`
clean, and `cargo tree -p unidb --no-default-features --edges normal` free of
tokio/reqwest/axum (rusqlite is a dev-dep, outside the normal graph).

**What changed:**
- New SQL-lane modules: `sql/{query,plan,query_exec,join,aggregate,sort,optimizer,statistics,explain}.rs`.
- `LogicalPlan::Query`/`Explain`/`Analyze` variants; parser routes joins/aggregates/subqueries/CTEs/EXPLAIN/ANALYZE into them; `apply_rls`/`bind_params` gained arms.
- `catalog.rs`: durable per-table statistics in a side map, backward-compatible catalog blob (`{tables, stats}`); `set_table_stats`/`table_stats`.
- Differential test suites vs SQLite + optimizer unit tests + EXPLAIN tests + this benchmark.

**Known limitations / tech debt:**
- No window functions, recursive CTEs, or `FULL OUTER`/`USING`/`NATURAL` joins.
- `ORDER BY` resolves an output-column name or 1-based position (not arbitrary expressions) in v1.
- Join keys compare by exact encoding — declare matching key types for cross-type numeric joins.
- The optimizer emits hash joins for reordered joins (index-nested-loop comes from the rule-based fallback path); cost-comparing INLJ inside the DP is a follow-up.
- **The catalog is still a single ~8 KiB page blob** holding every `TableDef`'s page list + all stats, so a table with a very large page list or a very wide analyzed schema can overflow it (this bench keeps the dataset modest for that reason). A multi-page/paginated catalog is the tracked fix.
- `EXPLAIN ANALYZE` reports total actual rows + execution time, not per-operator actuals/timings (a follow-up).

**Deferred to later phases:** columnar/vectorized execution (parked Track E); intra-query parallelism (needs Phase 5); per-operator EXPLAIN ANALYZE instrumentation; multi-page catalog.

**Locked-decision changes:** none. This is CLAUDE.md §1's "practical subset" growing; the catalog gained statistics storage (additive). No §3 decision reversed; no `FORMAT_VERSION` bump.
**PR:** _pending._

---

## Phase 5 — Concurrency & performance   [COMPLETE]   2026-07-09

**Branches / PRs:** landed in two parts. **Part 1 — P5.a–P5.d (concurrency
infrastructure) merged to `main` 2026-07-09 via [PR #14] (`30109d9`).** **Part 2
— P5.e (multiple writers) + P5.f (resource control)** on branch
`p5e-concurrent-writers` off updated `main` (PR #15).

**Locked-decision sign-off (CLAUDE.md §3, required before any work):** Phase 5
reverses the M5 "single writer thread, `Engine` is `!Sync`" simplification —
the single-writer design was an implicit locked decision. **The user explicitly
authorized reversing the single-writer design on 2026-07-09.** This entry, in
the first commit of the branch, records that sign-off per §3. D5 (WAL-before-
page) and D10–D12 (isolation) remain in force and are *strengthened* under
concurrency (D5 preserved by page latching; D11/D12 completed by real wait
queues + deadlock detection replacing abort-only). No other §3 decision is
touched.

**Summary:** _complete. Part 1 (P5.a–P5.d) built the thread-safe storage core +
real lock manager; Part 2 (P5.e/P5.f) made `Engine` `Send + Sync`, replaced the
single writer thread with an `Arc<Engine>` worker pool, wired heap page latches
and a leader-election group-commit barrier so **write throughput scales with
cores (3.68× at 8 writers)**, and added per-query timeouts / cancellation /
`work_mem`. Crash harness 19/19 throughout; the sync invariant holds._

**Checkpoint status:**
- **P5.a — buffer-pool latching — DONE.** Concurrent pool (`Mutex<PoolState>` frames, mmap behind `Arc<RwLock>`), hand-rolled `unsafe`-free per-page shared/exclusive latch table; D5 (WAL-before-page) preserved under concurrency.
- **P5.b — concurrent WAL append — DONE.** `Mutex<WalInner>`, all methods `&self`; serialized LSN allocation + group-batched flush.
- **P5.c — concurrent transaction manager — DONE.** `&self` `LockManager`; txn write path takes `&Wal`/`&LockManager`; 3 adversarial concurrency tests (unique-xid allocation, vacuum-horizon soundness under writer churn, single-winner row locking).
- **P5.d — real lock manager — DONE.** Shared/exclusive modes, blocking `Condvar` wait queues, wait-for-graph deadlock detection (`DbError::Deadlock` → 409). SI first-committer-wins kept as the `NoWait` policy. 4 multi-threaded tests incl. a genuine 2-thread deadlock. Crash harness 19/19; sync-invariant holds.
- **P5.e — multiple writers — DONE** (branch `p5e-concurrent-writers`, 2026-07-09).
  - **Step 1 (`75eaaa1`)** — `Heap` → interior-mutable `&self` (deadlock-safe FSM behind a `Mutex` never held across a page latch or WAL I/O), so every storage component is `&self`.
  - **Step 2 (`0478db7`)** — `Engine` is `Send + Sync`. The 6 mutated fields became interior-mutable (`control → Mutex<ControlData>` + a cached immutable `page_size`; `next_lob_id`/`next_event_seq`/`checkpoints_triggered` → atomics; `auto_checkpoint`/`last_checkpoint` → `Mutex`); all 27 `&mut self` methods → `&self`; every vestigial `&mut BufferPool/Wal/…` signature+reborrow → `&`. `checkpoint::run` takes `&Mutex<ControlData>` and locks only for the small control update (never across an fsync). Compile assertion `Send` → `Send + Sync`.
  - **Step 3 (`f977fb3`)** — concurrent writers. `server/engine_handle.rs` rewritten to `Arc<Engine>` + `spawn_blocking` (channel/`worker_loop` deleted; read fast-path kept). **Heap page latches** (`BufferPool::latch_exclusive`, built in P5.a, finally wired) wrap every heap read-modify-write, so two writers can't lose an update; insert/update use a re-checking `acquire_page_for_insert`; latches are taken one page at a time (no two-latch deadlock). A coarse `write_serial` `Mutex` serializes the non-CRUD paths that do a non-atomic read-catalog-then-mutate-shared-index sequence (edges/LOBs/event tables/DDL/vacuum) — **raw CRUD + reads stay concurrent**; SQL already serializes on the catalog `RwLock`. `tests/concurrent_writers.rs` (insert stress / distinct-row updates / same-row contention, all deadline-guarded).
  - **Step 4 (`29fe805`)** — group commit that scales. `txn::commit` returns the commit LSN; `Engine::commit` forces durability via new `Wal::sync_up_to`, whose leader (`group_fsync`) runs `sync_all` **with the append lock released** so concurrent committers coalesce behind one fsync.

  **Headline benchmark (`benches/concurrent_writers.rs`, 8 logical cores, group-commit mode, one insert+commit txn per iteration):**

  | writers | commits/sec | speedup |
  |--------:|------------:|--------:|
  |       1 |         325 |   1.00× |
  |       2 |         330 |   1.02× |
  |       4 |         647 |   1.99× |
  |       8 |        1197 |   3.68× |

  Write throughput now scales with concurrent writers (3.68× at 8 threads) versus the flat single-writer-thread ceiling. Crash harness **still 19/19** (incl. P12 fsync-fault); sync-invariant holds. **Documented limitation:** only *raw CRUD* scales with cores; SQL/graph/LOB writes serialize (catalog `RwLock` / `write_serial`) — finer-grained (latch-coupled B-tree) index concurrency is future work.
- **P5.f — resource control — DONE** (`6f8e8c4`, 2026-07-09). Per-query **timeout**, cooperative **cancellation** (`CancelToken`), and **`work_mem`** (spill row budget), held in a thread-local `QueryLimits` installed for the call (a query runs on one worker thread). The executor's scan loops call `query_limits::check()` every 1024 rows (`QueryTimeout`/`QueryCancelled`); `sort_mem_rows`/`hash_join_mem_rows` consult the per-query `work_mem`. Entry point `Engine::execute_sql_with_limits`; server maps both errors to 408. Tests: unit (`query_limits`) + `tests/query_limits.rs` end-to-end (timeout aborts a scan, generous timeout completes, pre-/cross-thread cancel abort, tiny `work_mem` forces the `ORDER BY` spill yet stays correctly ordered).

**Phase 5 is COMPLETE** (P5.a–P5.f). The single-writer → concurrent-writer unlock shipped; write throughput scales with cores; the crash harness stays 19/19 and the sync invariant (no tokio/reqwest/axum in the default engine) holds.

---

## Phase 6 — Operations & HA   [IN PROGRESS]   started 2026-07-09

Branch `phase6-ops-ha` (Core WAL + Ops lane). Spec: `docs/backlog/phase6_ops_ha.md`.
Delivers the confirmed scale target — **a strong single primary + read replicas**.
One PR for all of Phase 6; checkpoints P6.a→P6.g as separate commits.

### Locked-decision sign-offs (recorded before any P6 code — CLAUDE.md §3)

Two §3 decisions are touched by Phase 6. Both were flagged to the human and
**explicitly approved on 2026-07-09** before implementation began:

- **D6 (single-file storage; "WAL may be a separate file — revisit post-M4") —
  EVOLVED, signed off 2026-07-09.** P6.a splits the WAL from one append-only
  file into fixed-size **16 MiB segment files** in a `wal/` directory
  (seal + rotate on the boundary; truncation deletes whole consumed segments
  instead of rewrite-to-truncate). This is the enabler for concurrent WAL
  readers (replication slots / shipping) and is exactly the "revisit post-M4"
  D6 anticipated. **The data store remains a single file — only the WAL layout
  changes.** No reversal of D6's single-file *data-store* core; D3
  (checkpoint/WAL root) is extended with segments, matching the spec's
  "Locked decisions touched" table.
- **§1 "no cloud control plane" — RELAXED slightly, signed off 2026-07-09.**
  P6.b–P6.d add a backup/replication ops surface (replication slots, WAL
  shipping, online base backup, WAL archiving). This relaxes §1's blanket
  "no cloud control plane" for operational tooling only. **The single-primary
  charter is unchanged** — async (or optional sync) read replicas, *not*
  consensus; no multi-primary, no sharded writes (both remain parked, roadmap §7).

- **D9 (on-disk page format) / encryption-at-rest — DEFERRED, sign-off-gated
  (flagged at P6.f, 2026-07-09).** P6.f ships native **TLS** (rustls) and an
  **audit log** — neither touches the on-disk format. **Encryption-at-rest was
  deliberately NOT implemented:** it would change the D9 page format (encrypting
  page bytes vs. the current plaintext + CRC32 + LSN layout) **and** is
  fundamentally at odds with this engine's `memmap2`-based page store —
  transparent block encryption can't compose with mmap page-faults without a
  decrypt-on-read buffer layer or moving off mmap entirely (a storage-core
  re-architecture). Per §3, a D9 change needs explicit human sign-off; that
  sign-off has **not** been given, so encryption-at-rest is recorded here as a
  documented, sign-off-gated follow-up rather than assumed. TLS-on-the-wire +
  audit trail satisfy the deployable-security bar for v1; at-rest encryption is
  typically provided by full-disk/volume encryption (LUKS/FileVault) underneath,
  which needs no engine change.

### Phase 6 checkpoints — SHIPPED (2026-07-09)

One PR for all of Phase 6 (branch `phase6-ops-ha`), checkpoints P6.a→P6.g as
separate commits. Delivers the confirmed scale target — a strong single primary
+ read replicas, deployable and operable.

- **P6.a — segmented WAL** (`8f2fdf3`): WAL is now a directory of fixed-size
  16 MiB segment files (seal + rotate; truncation deletes whole consumed
  segments, no rewrite). Recovery scans segments in LSN order. New crash point
  **P18** (harness 19→20). D6 evolution signed off (above).
- **P6.b — replication slots + WAL shipping** (`6e83fa7`): persisted
  `SlotRegistry` (`slots.json`); checkpoint truncation floor =
  `min(checkpoint_lsn, min slot restart_lsn)`; `Wal::records_from`/`ship_from` +
  `encode_stream`/`decode_stream`; REST `/replication/{slots,stream}`.
- **P6.c — read replicas + failover** (`aab4a06`): `replication::Replica` —
  base snapshot + incremental WAL apply (`apply_stream`), `promote()` failover,
  `wait_for_sync_replicas` sync option. Fixed a self-deadlock in
  `primary_control` (double control-lock in one statement).
- **P6.d — backups + PITR** (`d4f76c7`): `Engine::base_backup`/`archive_wal`,
  `backup::restore(base, archive, dest, target_lsn)` — PITR **by LSN**. New
  crash point **P19** (harness 20→21).
- **P6.e — users/roles/GRANT** (`c8109ed`): `authz::RoleStore` (`roles.json`),
  transitive role membership, per-table privileges, `execute_sql_as` enforcement,
  per-user JWT (`sub` claim) with open/bootstrap mode. RLS-over-SQL deferred.
- **P6.f — security** (`22f9539`): native TLS (rustls via `axum-server`), audit
  log (`audit.log`). Encryption-at-rest DEFERRED, D9 sign-off-gated (above).
- **P6.g — observability** (`afb2d37`): `Engine::stats()` (`pg_stat_*`-style) +
  `GET /stats`, slow-query log, ops runbook (`docs/ops_runbook.md`). EXPLAIN was
  already shipped (P4.e).

**Benchmarks** (release build, Apple Silicon macOS; `benches/phase6_ops.rs`,
5,000-row table):

| Operation                          | Time                    |
|------------------------------------|-------------------------|
| Base backup (5,000 rows)           | 7.1 ms                  |
| Restore to latest                  | 72.1 ms                 |
| PITR restore (to a target LSN)     | 42.8 ms                 |
| Replica apply (100 shipped updates)| 40.2 ms (~2,490 rows/s) |
| WAL ship batch size (100 updates)  | 40,980 bytes            |
| Failover (promote → read-write)    | 26.3 ms                 |

Honest notes: replica apply is O(WAL) per batch (v1 re-materializes via the
recovery path — a re-base is the documented mitigation), so ~2.5k rows/s is a
correctness-first figure, not a tuned steady-state number. Backup/restore/PITR
and failover are sub-100 ms at this scale.

**Crash harness:** 19 → **21** (P18 segmented-WAL multi-segment recovery +
truncation; P19 backup+PITR restore after primary loss). All green.
**Gates:** `cargo test -p unidb` + `--features server` + `--test crash` (21/21),
`clippy --workspace --all-targets` (default + server), `fmt`, and the sync
invariant (`cargo tree -p unidb --no-default-features --edges normal` has no
tokio/reqwest/axum/rustls) all pass. No `FORMAT_VERSION` bump.

**Locked-decision changes:** D6 evolved (segmented WAL) + §1 "no cloud control
plane" relaxed for ops — both signed off 2026-07-09 (recorded above). D9 /
encryption-at-rest deferred pending sign-off.

**Known limitations / deferred:** incremental replica/PITR roll-forward
reconstructs pages present in the base (fresh pages aren't FPI-covered — take
base backups regularly / re-base); PITR is by-LSN (time-based needs commit
timestamps); RLS-over-SQL (`CREATE POLICY`); encryption-at-rest (D9-gated);
automatic failover coordinator (manual promotion in v1); S3 archiving.

**Phase 6 is COMPLETE — the roadmap's 6-phase plan is fully delivered.**

---

## Commit-time WAL fsync — group-committed force-log-at-commit as default   [LANDING]   2026-07-09

**PR:** _pending_
**Spec:** `docs/backlog/commit_time_fsync.md` (checkpoints C1–C5).
**Summary:** Flips the durability default from per-statement fsync to
**group-committed force-log-at-commit**: statement mini-txns issued inside an
open user transaction append their WAL records without a per-statement fsync,
and `Engine::commit`'s `sync_up_to(commit_lsn)` is the single durable point —
one group-coalesced fsync per transaction. This is ARIES' force-log-at-commit
(fulfilling **D1**); **D2** (mini-txn bracketing) and **D5** (WAL-before-page)
are untouched — no §3 decision is reversed.

### Human sign-off (durability timing change)

Per the spec's C5 and CLAUDE.md §0.5/§6 evidence ethos (which applies to
durability semantics even though no locked decision flips), the user
**explicitly authorized making group-committed force-log-at-commit the default
on 2026-07-09.** Durability *timing* changes (per-statement → per-transaction);
the durability *guarantee* is unchanged: no commit is acknowledged until its
commit record is fsync'd. D1 is fulfilled (its ARIES durability point *is*
force-log-at-commit); D2 and D5 are unchanged. `synchronous_commit=off`-style
ack-before-flush (a genuine D violation) is explicitly **out of scope** — never
the default, at most a separate documented opt-in later.

### C1 — durability-claim audit (every `commit_mini_txn` site)

Under the new default the WAL runs deferred; a mini-txn's records are made
durable either by the enclosing user transaction's commit `sync_up_to`, or by
the operation issuing its own explicit sync. Each site classified:

| Site | Path | Durable via |
|------|------|-------------|
| heap insert/update/delete (`heap.rs`) | `Engine::insert/update/delete` under an `xid` | **covered-by-commit** — `Engine::commit` → `sync_up_to(commit_lsn)` |
| durable B-Tree / full-text index maint. (`btree_index.rs`) | `apply_durable_index_writes` during INSERT/UPDATE / `CREATE INDEX` backfill (both under `xid`) | **covered-by-commit** (or by the standalone entry point's self-sync, below) |
| durable vector (IVF) index maint. (`disk_vector.rs`) | same as above | **covered-by-commit** |
| catalog persist (`catalog.rs`) | DDL under `execute_sql(xid)` | **covered-by-commit** (request-level catalog snapshot/restore handles rollback) |
| large-object chunk rows (`large_object.rs`) | `Engine::put_large_object(xid, …)` under `xid` | **covered-by-commit** |
| open-time system setup (`ensure_edges_table`/`ensure_edge_index`/`ensure_lobs_table`/`derive_*`) | `Engine::open`, **before** the deferred flag is set | **self-syncing** — runs while the WAL is still per-statement, so each mini-txn fsyncs during open |
| checkpoint (`checkpoint.rs`) | `Engine::checkpoint` (standalone, no `xid`) | **self-syncing** — added `wal.sync()` at entry (before `flush_all`, so D5 lets every dirty page reach disk) + `log_checkpoint` already fsyncs |
| vacuum (`lib.rs::vacuum_inner`) | `Engine::vacuum` (standalone, no `xid`) | **self-syncing** — added `sync_wal()` before return |
| `set_column_index` / `enable_events` (`lib.rs`) | standalone DDL-like (no `xid`) | **self-syncing** — added `sync_wal()` before return |
| replication slots (`slots.json`) | `create/advance/drop_replication_slot` | **self-syncing** — atomic write-tmp + rename (independent of the WAL fsync flag) |
| backup / PITR (`base_backup`) | `Engine::base_backup` | **self-syncing** — calls `checkpoint()` (which now self-syncs) then copies files |

**What changed (C1):** `Engine::open` sets `wal.set_deferred_sync(true)` after
open-time setup; `set_deferred_sync` is now `#[doc(hidden)]` (the per-statement
policy survives only so the crash harness can exercise both). `checkpoint::run`,
`vacuum_inner`, `set_column_index`, and `enable_events` self-sync. The server
handle no longer needs its explicit `set_deferred_sync(true)`.

**Locked-decision changes:** none reversed. **D1 fulfilled** (force-log-at-commit
is its ARIES durability point); D2 and D5 unchanged. Human sign-off recorded
above (2026-07-09).

### C2 — D5 eviction-forced sync (+ two pre-existing recovery bugs it surfaced)

The eviction-forced-sync mechanism itself (`BufferPool::fetch_page_for_write`:
on `BufferPoolFull`, force `wal.sync()`, refresh the durable frontier, retry
once) already shipped with the M9/P5 group-commit work and the whole heap write
path already routes through it — so under the new default a large transaction
that dirties more pages than the pool holds forces a WAL sync and steals a
now-durable page rather than dead-ending. C2 adds the end-to-end memory-pressure
proof: `large_deferred_transaction_survives_pool_smaller_than_working_set` (16
frames, one transaction inserting 400×~1 KiB rows → dozens of pages), asserting
completion, correct in-session read-back, **and correct recovery after reopen**.

That reopen assertion surfaced **two pre-existing latent recovery bugs**
(present independent of the deferral flip — they reproduce in per-statement mode
too — but which commit-time fsync makes ordinary, since deferral routinely
dirties more pages than a small pool holds):

1. **WAL_INSERT redo leaked a buffer-pool frame pin.** The page-allocation
   record (`slot == u16::MAX`) and the "already applied" idempotent-skip path
   both `return Ok(())` after `fetch_or_create` **without unpinning**
   (WAL_UPDATE/DELETE/VACUUM unpin correctly; only WAL_INSERT leaked). When the
   recovered data spans more pages than the recovery pool capacity, the leaked
   pins exhaust the pool and every later redo fails with `BufferPoolFull` —
   swallowed as a `tracing::warn`, so committed rows were silently dropped.
   **Fix:** the allocation record now calls `ensure_page_allocated` (sizes the
   page into the file, no pin) instead of `fetch_or_create`; the idempotent-skip
   path unpins.
2. **Recovery never advanced the pool's durable-WAL frontier.** It replayed with
   `durable_wal_lsn == INVALID_LSN`, so `find_victim` refused to evict *any*
   dirty redo page (D5 conservative) and the pool filled after `pool_capacity`
   pages. **Fix:** set the frontier to the tail LSN of the on-disk WAL before
   the redo pass — every record being replayed is already durable, so evicting
   redone pages is sound.

Both were invisible before because normal recovery uses the default 4096-frame
pool, which comfortably holds any realistic redo working set; only a
deliberately tiny recovery pool exposes them. **Files:** `recovery.rs` (both
fixes), `bufferpool.rs` (mechanism, unchanged), `lib.rs` (test). Crash harness
still **21/21** (the fixes only affect the pool-exhaustion path a large pool
never hits); no format change.

### C3 — replication durable-LSN cap

`Wal::records_from` (and therefore `ship_from` / `Engine::ship_wal`) now returns
only records with `lsn <= durable_lsn`. Under the group-committed default,
records are written to the segment file *before* their fsync, so the on-disk WAL
can hold records past the durable frontier; shipping those would let a replica
apply — and a promoted replica *retain* — commits the primary had not made
durable, so a primary crash before its own fsync would leave the replica **ahead
of the recovered primary** (divergence on failover). Capping at `durable_lsn`
makes a replica's state always a prefix of the primary's durable state; records
between `durable_lsn` and the tail simply ship in a later batch once durable.
Sync-slot acks are bounded transitively — a `SlotKind::Sync` consumer can only
confirm what it received (all `<= durable_lsn`), and `wait_for_sync_replicas`
runs after a commit's own `sync_up_to`, so it waits on a durable LSN.

New `Engine::wal_durable_lsn()` accessor. Test
`shipping_capped_at_durable_lsn_keeps_replica_a_prefix_on_primary_crash`
(`replica.rs`): a durable base + one shipped durable row, then an open,
uncommitted transaction whose large (~7 KiB) rows push records onto the WAL file
past the durable frontier (asserted via a raw `scan_file`); `ship_wal` returns
only records `<= durable`; the replica has exactly the durable rows; the primary
"crashes" pre-fsync, restarts (recovery drops the tail), and a re-ship leaves the
replica a faithful prefix. Uses the raw byte-slice heap so the eagerly-persisted
non-MVCC M1 catalog root doesn't confound the WAL cap. **Files:** `wal.rs`,
`lib.rs` (accessor), `replica.rs` (test). No format change; crash harness 21/21.

### C4 — crash-harness proof (21 → 25) + valid-prefix property in both modes

Four new crash points under the group-committed force-log-at-commit default
(`tests/crash/main.rs`), and the valid-prefix property test
(`run_property_case`) now runs under **both** durability policies (`deferred =
true` default and `false` legacy per-statement), so the invariant "the recovered
DB is exactly the set of transactions that reached WAL_TXN_COMMIT" is proven for
each:

- **Pa** `pa_deferred_mid_txn_unsynced_leaves_no_trace` — a transaction whose
  statements are never fsynced (no commit → no `sync_up_to`) and never commits
  leaves no trace on reopen. The deferred-mode analog of P6.
- **Pb** `pb_cross_txn_shared_log_sync_undoes_open_txn_keeps_committed` — txn A
  appends statements (unsynced) and stays open; txn B commits, and B's
  `sync_up_to` flushes the *shared* WAL buffer — including A's records — to
  durable storage. A crash with A still open cleanly undoes A (it never reached
  WAL_TXN_COMMIT) while B survives: the single ordered log never accidentally
  persists an uncommitted transaction.
- **Pc** `pc_torn_unsynced_tail_replay_stops_cleanly` — a torn record in the
  unsynced WAL tail (a large uncommitted row forced onto the segment file, then
  its tail byte flipped) is caught by CRC; replay stops cleanly at the last valid
  record and the committed prefix survives.
- **Pd** `pd_eviction_forced_sync_preserves_d5_on_crash` — a large transaction on
  a 16-frame pool triggers eviction-forced WAL syncs (D5: log durable before a
  dirty page is stolen); a crash after commit, with most pages only ever
  eviction-flushed (never checkpointed), recovers every committed row from the
  durable WAL. Exercises the C2 recovery fixes end-to-end.

P6 and the two-table incomplete-txn test were pinned to the legacy per-statement
policy (they call `flush()` mid-transaction, which is only valid when statements
are individually durable) so that policy stays covered. **Crash harness 21 → 25,
all green.** No format change.

### C5 — acceptance benchmark + closeout

**Acceptance benchmark** (`benches/decompose.rs`, fetched from `origin/bench-ladder`;
release, Apple Silicon macOS; SQLite baseline `PRAGMA journal_mode=WAL,
synchronous=FULL, fullfsync=ON` to match Rust `sync_all`'s `F_FULLFSYNC`; 100
single-row durable transactions per rung, median of 10 samples). Because
group-committed force-log-at-commit is now the **default**, the ladder's ordinary
rungs (`w0_row`…`w4_event_full`) now measure that default and **converge with the
explicit one-fsync rungs (`w4_1fsync`)** — which is the proof the flip landed.

| Rung | ms/commit (after: default) | note |
|------|----------------------------|------|
| W0 `w0_row` (plain row) | **3.59** | ≈ SQLite `sqlite_w0` **3.64** — parity |
| W1 `w1_btree` (+ B-tree) | 4.39 | |
| W2 `w2_vector` (+ VECTOR(128) IVF) | 4.36 | |
| W3 `w3_edge` (+ graph edge) | 4.24 | |
| W4 `w4_event_full` (+ event capture) | **4.40** | full multi-model commit |
| `w0_1fsync` (explicit one-fsync W0) | 3.57 | == `w0_row` ✓ |
| `w4_1fsync` (explicit one-fsync W4) | 4.37 | == `w4_event_full` ✓ |
| SQLite `sqlite_w0` / `sqlite_w1` | 3.64 / 4.03 | durability-matched baseline |

**Before → after (the headline):** the full multi-model commit (row + B-tree +
vector + edge + event) goes from the old per-statement default's **~33.1
ms/commit** (PR #21 ladder — ~10 `F_FULLFSYNC`s where one suffices) to **~4.40
ms/commit** at one group-coalesced fsync — **~7.5×**. W0 is at SQLite parity
(3.59 vs 3.64 ms). The old default cannot be re-measured on this machine (the
default changed); its 33.1 ms is PR #21's recorded number, and the
`w4_event_full` ≈ `w4_1fsync` convergence above is the same-machine confirmation
that the default is now the one-fsync path.

**Peak memory:** unchanged — this milestone moves *when* the WAL is fsynced, not
what is buffered; no new resident structures (the ladder engine holds the same
buffer pool + IVF centroids as before).

**Crash harness:** 21 → **25** (Pa–Pd) + valid-prefix property test under both
policies — all green. **No `FORMAT_VERSION` bump.** Sync invariant holds
(`cargo tree -p unidb --no-default-features --edges normal` has no
tokio/reqwest/axum).

**Locked-decision changes:** none reversed — **D1 fulfilled**, D2/D5 unchanged.
Human sign-off for making group-committed force-log-at-commit the default
recorded above (2026-07-09).

**Commit-time WAL fsync is COMPLETE.**
