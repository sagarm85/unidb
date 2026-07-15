# A3 scan-vs-index gate is a fixed selectivity threshold, not a real cost model

**Type:** Improvement
**Status:** SHIPPED — see PROGRESS.md "Item 43" entry (2026-07-15); PR pending merge
**Priority:** Medium — not a correctness issue and not urgent, but it's a
real, measured gap between unidb's query planning and Postgres's that widens
with table size, discovered while comparing two multi-model report runs at
different scales.

---

## Problem

Comparing `docs/performance/multi_model_report_20260715_091035.md` (small
sweep, ~1k-2k row tables) against `multi_model_report_20260715_092725.md`
(10k/20k sweep, ~20k-40k row tables), Table 3's `SELECT filtered (k<N)` flips
from a **unidb win** at small scale (+167%) to a **Postgres win** at larger
scale (+183%) — a ~4.5x relative swing on the same query shape, same
selectivity, same schema, same fair `ANALYZE` on both engines.

This is **not a benchmark misconfiguration** — verified directly:

- `ANALYZE` runs on both engines at the correct post-insert row count in both
  runs (`benches/decompose.rs`, the `t3_analyze` phase) — no
  un-analyzed-vs-analyzed asymmetry.
- The query (`SELECT id, body FROM t WHERE k >= 0 AND k < N`, on a table of
  `2N` rows after the benchmark's own INSERT step) selects **~50% of the
  table at both scales** — the selectivity ratio is identical, only the
  absolute row count differs.

## Root cause

Reproduced Postgres's actual query plan at both scales directly:

```
-- 2,000-row table, same 50%-selective filter:
Seq Scan on t  (cost=0.00..43.00 rows=1000 width=9) ... Buffers: shared hit=13

-- 40,000-row table, same 50%-selective filter:
Index Scan using t_k on t  (cost=0.29..754.23 rows=19947 width=10) ... Buffers: shared hit=184
```

Postgres's cost-based optimizer switches from `Seq Scan` to `Index Scan` as
the table grows, **even though selectivity never changed** — it's factoring
in the growing absolute page count, not just the selectivity ratio.

unidb's equivalent decision (`index_lookup_is_selective`,
`src/sql/executor.rs:2302`, the "A3 gate") is a **single fixed threshold**:

```rust
const INDEX_RANGE_SELECTIVITY_MAX: f64 = 0.3;  // src/sql/executor.rs:2295
```

At 50% selectivity (above 0.3), unidb takes the sequential scan — **always**,
regardless of whether the table has 2,000 or 40,000,000 rows. There is no
table-size or page-count term in the decision at all, only the selectivity
fraction from `ANALYZE` statistics.

At small scale this coincides with Postgres's own (correct) choice, so the
two engines are doing comparable work and unidb's lower fixed overhead wins.
At larger scale, Postgres's cost model correctly recognizes that touching
fewer of the now-much-larger set of pages is worth the random-access cost
even at 50% selectivity — a decision unidb's static threshold never reaches.

## Why this needs care, not a quick threshold bump

`INDEX_RANGE_SELECTIVITY_MAX` already caused a regression once when
miscalibrated the other direction (`CLAUDE.md` §0.6.5: "A3's selectivity
gate: forcing the index path *regressed* a 50%-selective DELETE" — the
*current* 0.3 value is itself the fix for that regression, chosen from "the
measured crossover between a 25%-selective UPDATE (index wins) and a
50%-selective DELETE (scan wins)" at whatever table size that measurement was
taken at). Simply raising the constant would very plausibly reintroduce that
exact regression at small-table sizes, just shifted. **The fix is not a bigger
constant — it's making the gate size-aware, the way Postgres's real cost
model already is.**

## Proposed scope (re-derive the exact mechanism per CLAUDE.md §0.6.2 before
implementing — this is a sketch, not a spec to implement as-is)

1. **A real (even if simplified) cost model**, not a single fixed fraction:
   something like `index_cost ≈ matched_rows * random_page_cost_factor` vs
   `scan_cost ≈ total_pages * seq_page_cost_factor`, using `ANALYZE`'s
   existing `row_count` (and, if not already tracked, an estimate of table
   page count) — the same two-sided comparison Postgres's planner already
   does, not necessarily as sophisticated.
2. **Re-derive the crossover empirically at multiple table sizes**, not just
   one — the existing 0.3 constant was calibrated at a single (unknown, not
   re-derivable from the comment alone) table size; a proper fix needs the
   crossover point measured across a size sweep (small/medium/large tables),
   matching this project's own measurement-first discipline (§0.6.4).
3. **Applies to both the SELECT path (`try_exec_select_btree`) and the
   UPDATE/DELETE path (`matching_rows`)** — both call through
   `index_lookup_is_selective`, so a fix here helps both, but both need
   re-verification, not just the SELECT case this finding surfaced.

## Correctness invariants the fix MUST preserve

1. **No regression at small scale** — the exact regression this constant
   already fixed once (forcing the index path on a 50%-selective DELETE) must
   not reappear, at any table size.
2. **No regression at large scale in the other direction** — don't
   overcorrect into always preferring the index path; the point is a real
   crossover, not a fixed answer either way.
3. **Measured, not assumed** — any new threshold/formula needs a size-swept
   benchmark proving the crossover behavior, the same rigor
   `multi_model_report_20260715_092725.md` applied to find this gap.

## Acceptance criteria

- [ ] `SELECT filtered (k<N)`-shaped queries (or an equivalent regression
      test) show unidb switching to the index path at large table sizes when
      Postgres would too, without regressing the small-table case where a
      sequential scan is genuinely cheaper.
- [ ] A permanent size-swept regression test (small/medium/large table, same
      selectivity) proving the crossover point behaves sanely at each size —
      not just a single-size snapshot.
- [ ] Existing A3-gated correctness tests (the 50%-selective DELETE case that
      motivated the current constant) still pass unchanged.
- [ ] `PROGRESS.md` records the before/after with real numbers, referencing
      this item and the two reports that surfaced the gap.

## Depends on / builds on

- `src/sql/executor.rs` — `index_lookup_is_selective`, `matching_rows`,
  `try_exec_select_btree` — the code to modify.
- `docs/performance/multi_model_report_20260715_091035.md` and
  `multi_model_report_20260715_092725.md` — the two reports whose comparison
  surfaced this gap.
- The existing `ANALYZE`/`table_stats` machinery (P4.d) — already tracks
  `row_count` and column selectivity; a page-count estimate (if not already
  available) is the main new input needed.
