# 108 — CRUD ratio drift vs 2026-07-19 report: verify or explain

**Type:** Performance
**Status:** ✅ RESOLVED 2026-07-21 (same day) — **no unidb regression; the
drift was environmental.** Absolute-numbers comparison (§0.6 rule 4) showed
Postgres's own code-identical absolutes moved 2.1–28× between the runs while
unidb improved on every row (absolutes AND WAL-B/row). Shipped: environment
canary in `compare_bench.py` (PG-absolute median drift > 25% → warning),
ceilings-table refresh in `decompose.rs`, inline correction of the item-104
COUNT claim in PROGRESS.md. See PROGRESS.md "Item 108". No bisection was
run — none was needed.

## Problem

The 2026-07-21 full bench (main @ `b6d6e5f`) shows several CRUD ratios below
the 2026-07-19 report (`report_20260719_234504.md`, branch
`perf/items-67-51-68-69-92` pre-merge):

| Operation | 2026-07-19 | 2026-07-21 | drift |
|---|---:|---:|---|
| SELECT filtered (5%) | 0.74× | **0.45×** | −39% — largest, investigate first |
| UPDATE HOT | 1.51× | **1.06×** | −30% |
| UPDATE non-HOT | 0.81× | **0.65×** | −20% |
| DELETE selected | 2.73× | 2.01× | −26% |
| DELETE all | 7.06× | 4.29× | −39% |
| INSERT per-row | 0.53× | 0.47× | −11% |
| GROUP BY | 1.30× | 1.29× | stable ✓ |
| COUNT(*) | 6.93× | **41.25×** | item 104 ✓ (O(1) path now survives restart) |

~15 items merged between the two runs (96–104, 19-G*, 38, 70, 102-B, 93–95,
92). Ratios are unidb÷PG, so drift can come from either side (PG container
variance included) — absolute rec/s comparison between the two reports is
the first discriminator (§0.6 rule 4: trust absolutes over noisy ratios).

## Plan (cheap now, thanks to item 105)

1. Compare **absolute** unidb rec/s and PG rec/s per row across the two
   reports — classify each drift as "unidb slower", "PG faster", or "noise".
2. For any real unidb-side regression: bisect with selective runs —
   `MM_SKIP_LADDER=1 MM_SKIP_TABLE4=1 MM_SKIP_TABLE5=1 scripts/report.sh`
   (~20–30 min per point) over the merge range `51022be..b6d6e5f`.
   Prime suspects for SELECT filtered: item 102-B (covering-index optimizer
   changes), item 96 (plan cache), item 70 (madvise prefetch interplay in
   Docker VM).
3. Fix or document as honest ceiling; update the in-bench "known honest
   ceilings" table in `benches/decompose.rs` (currently stale — still quotes
   items-75-84-era numbers superseded by PR #171).

## Acceptance criteria

- [ ] Every drifted row classified with absolute-number evidence.
- [ ] Real regressions root-caused (bisect log recorded) and fixed or
      ceiling-documented with sign-off.
- [ ] `decompose.rs` ceilings table refreshed to current measured values.
- [ ] Confirming selective bench run recorded in PROGRESS.md.
