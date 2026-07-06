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

## M5 — API / server   [PLANNED]
_Embedded crate stabilized; optional REST + JWT auth + subscribe + `/metrics`._
