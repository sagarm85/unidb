# 109 — Parallel B-tree candidate resolution (SELECT filtered 0.45× → ≥0.70×)

**Type:** Performance
**Status:** ⏳ NOT STARTED — filed 2026-07-22 (promoted from the ceilings
table's "revisit when" note; no dedicated item existed)

## Problem

`SELECT … WHERE k < N/20` (5% selectivity, 100k rows) sits at **0.45–0.51×**
vs Postgres (21/22 Jul healthy-environment runs; the A/B in item 108 proved
this is the true standing, not a regression). The report's own root-cause
row: PG runs a **parallel index scan**; unidb's `try_exec_select_btree`
resolves candidates **serially** — B-tree range scan → per-candidate heap
fetch → MVCC visibility → decode/project, one row at a time on one core.

unidb already beats PG on the ops where its parallel workers engage
(GROUP BY 1.29×, DELETE all 4.29×, DELETE selected 2.01× via item 66's
`parallel_collect_matching`). The filtered-SELECT path is the one heavy
read path still confined to a single core. Serial-path micro-optimisations
are exhausted (items 54/59/67/102-A history; ceiling ~0.50–0.60×).

## Step 0 (mandatory before building — decides the design)

1. **Phase-split the serial path** at 5% × 100k (timers, same pattern as
   item 92's `Q_ANN_NANOS`): B-tree leaf walk vs per-candidate heap fetch +
   visibility vs decode/project. Hypothesis to verify: heap fetch + decode
   dominate (the leaf walk is a few thousand comparisons); if the leaf walk
   dominates instead, the design below is wrong — stop and re-plan.
2. Measure PG at `max_parallel_workers_per_gather` 0 vs 2 on the same query
   to know how much of its lead is parallelism vs per-row cost.

## Design sketch (validate against Step 0)

- Keep the B-tree **range scan serial** (cheap, ordered, produces candidate
  RowIds). Partition the candidate list into **page-grouped contiguous
  chunks** and fan out to the existing worker pool (Milestone P infra +
  item 15 governance): each worker does heap fetch → visibility → residual
  predicate → project for its chunk; results concatenate in partition order
  (B-tree emits sorted candidates, contiguous chunks preserve order — no
  merge needed even under ORDER BY k).
- **Batched heap-page prefetch** per chunk before fetching (group RIDs by
  page; item 70's `madvise(WILLNEED)` machinery).
- **Gate by measured conditions (§0.6 rule 5):** parallel only when
  candidate count ≥ threshold (calibrate; likely ~2–5k) AND workers
  available; below threshold the serial path runs unchanged. The A3
  index-vs-scan selectivity gate stays in front. (Lesson: A3's forced-index
  regression; item 15's governance exists precisely to keep small queries
  off the pool.)
- **Correctness seams:** workers share the statement's read-only snapshot
  (same pattern as `parallel_scan.rs`); the `on_read()` seam (D11) must fire
  in the worker path; hint-bit writes (item 68) from concurrent workers on
  one page follow the same latch discipline `parallel_collect_matching`
  already uses — verify, don't assume.

## Targets

- SELECT filtered (5%, 100k, Docker fair-fsync): **≥ 0.70×** vs PG
  (stretch 0.85×).
- **No regression** at low candidate counts (point lookups, 0.1%
  selectivity) or with `UNIDB_PARALLEL_WORKERS=0` / toggle off.
- Concurrency matrix 32/32 PASS (readers-during-writes cells cover the new
  path); index-vs-scan agreement oracle stays green.

## Acceptance criteria

- [ ] Step-0 phase split recorded before any code (decides go/no-go).
- [ ] Parallel path gated by candidate-count threshold, measured not guessed.
- [ ] `on_read` seam + snapshot semantics verified in worker path (tests).
- [ ] Docker bench (item-105 selective run, `MM_TABLES=3` + `MM_BASELINE`)
      shows ≥0.70× filtered with no other Table-3 row regressing.
- [ ] Conc matrix green; full suite + crash harness green; clippy/fmt clean.
- [ ] Ceilings table in `decompose.rs` updated with the new measured row.
