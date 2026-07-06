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

## M2 — Vector & Text search   [PLANNED]
_`VECTOR(n)` + async background HNSW; `NEAR`; full-text inverted index. **Headline baseline switches to the replaced stack.**_

## M3 — Graph   [PLANNED]
_Edge records + edge-list index; Cypher subset; per-edge locking with batched adjacency latching._

## M4 — Event queue   [PLANNED]
_WAL-derived stream; durable consumer offsets; replay; slow-consumer durability contract resolved._

## M5 — API / server   [PLANNED]
_Embedded crate stabilized; optional REST + JWT auth + subscribe + `/metrics`._
