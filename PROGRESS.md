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

## M0 — Storage core   [NOT STARTED]   (target date TBD)

**PR:** _pending_
**Summary:** _Single-file page store, buffer pool, WAL, control file, crash recovery, durable single-table CRUD. No MVCC. Plus crash-injection harness and structured logging._

**Benchmarks** (release build, machine TBD) — _to be filled when M0 lands_:

| Workload               | Throughput (ops/s) | p50 (µs) | p99 (µs) | Peak RSS | Baseline (SQLite) |
|------------------------|--------------------|----------|----------|----------|-------------------|
| single-table INSERT    | _tbd_              | _tbd_    | _tbd_    | _tbd_    | _tbd_             |
| point SELECT by key    | _tbd_              | _tbd_    | _tbd_    | _tbd_    | _tbd_             |
| UPDATE by key          | _tbd_              | _tbd_    | _tbd_    | _tbd_    | _tbd_             |

**Crash harness (target):** post-WAL/pre-flush · mid-checkpoint · post-mutation/pre-commit · during WAL truncation · post-commit-fsync — all must be green.
**Definition of done:** durable CRUD survives all crash points; recovery verified by property tests; metrics recorded above; no locked decision violated.

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
