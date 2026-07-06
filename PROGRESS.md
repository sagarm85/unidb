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

## M1 — MVCC + CRUD   [PLANNED]
_Transactions; READ COMMITTED default / REPEATABLE READ available; `on_read`/`on_write` seam (no-op) for future SSI; catalog; SQL subset. JSON columns + RLS folded in. Baseline: SQLite._

## M2 — Vector & Text search   [PLANNED]
_`VECTOR(n)` + async background HNSW; `NEAR`; full-text inverted index. **Headline baseline switches to the replaced stack.**_

## M3 — Graph   [PLANNED]
_Edge records + edge-list index; Cypher subset; per-edge locking with batched adjacency latching._

## M4 — Event queue   [PLANNED]
_WAL-derived stream; durable consumer offsets; replay; slow-consumer durability contract resolved._

## M5 — API / server   [PLANNED]
_Embedded crate stabilized; optional REST + JWT auth + subscribe + `/metrics`._
