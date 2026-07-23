# Vacuum — per-table accounting, cost throttle, whole-table compaction

**Type:** Improvement
**Status:** ✅ SHIPPED 2026-07-13 — per-table vacuum accounting, cost throttle, and whole-table compaction. See `backlog_index.md` row 27 / PROGRESS.md. _(Header corrected 2026-07-22 — was never flipped at ship time.)_

> Limitation from the architecture guide + `autovacuum.md` known-limits:
> "engine-global (not per-table) accounting; no cost-based throttle; no
> whole-table compaction." Autovacuum (item 9) ships and is correct; these are
> the refinements it explicitly deferred. Most self-contained of the three
> storage follow-ups (`autovacuum.rs` + `heap.rs`), so it parallelizes safely.

## Scope

- **V1 — Per-table dead/live accounting (MUST).** Replace the global
  `dead_tuples`/`live_tuples` estimates with per-table counters so autovacuum
  triggers on the table that actually churned, not the whole engine. Feeds the
  item-21 `unidb_catalog.table_stats` (join up if present).
  AC: churning one table triggers a vacuum of THAT table while others are
  untouched; per-table estimates exposed via stats.
- **V2 — Per-table vacuum (`vacuum_table`) (MUST).** The pass targets a single
  table instead of the whole engine (the M10 reclamation logic is unchanged —
  this only scopes *what* it walks).
  AC: `vacuum_table(name)` reclaims that table only; manual `vacuum()` still
  does all.
- **V3 — Cost-based throttle (SHOULD).** A Postgres-style vacuum cost budget
  (page-hit/miss/dirty accounting + a nap) so a background pass can't starve
  foreground writers on a hot system.
  AC: under a write-heavy workload, throughput drop during autovacuum is bounded
  vs the un-throttled pass (measured).
- **V4 — Whole-table compaction (SHOULD).** Beyond per-page compaction: relocate
  live tuples to pack pages and release trailing empty pages back to the FSM
  (bloat recovery a page-local compact can't do). Crash-safe (WAL-logged,
  redo-recoverable); reader-correct (M10.c aliasing gate).

## Landmines

- Compaction moves live tuples → RowIds change → every index entry for a moved
  row must be re-pointed atomically (the same forward-resolver hazard item-11/A1
  documented). Decide the approach before V4; may defer V4 if it needs a format
  concern.
- Crash harness must stay green (add a point for crash mid-compaction).
- Horizon-holding readers still bound what's reclaimable (M10) — throttle/pass
  must not violate it.

## Acceptance

- [x] Per-table trigger + `vacuum_table` proven (V1/V2). See tests
      `per_table_estimates_track_churn_independently`,
      `per_table_trigger_fires_only_for_churned_table`,
      `vacuum_table_scopes_to_one_table_only`, `manual_vacuum_covers_all_tables`.
- [x] Throttle bounds foreground impact (V3 measured). See test
      `vacuum_cost_throttle_reclaims_correctly_under_tight_budget` +
      `item27_measurement_bloat_and_throttle`.
- [x] V4 deferred — see §V4 deferral note below.
- [x] Crash harness green (33/33, +1 for P31 crash-mid-vacuum_table).
      PROGRESS.md entry with bloat numbers.

## V4 deferral note (2026-07-13)

Whole-table compaction (relocating live tuples to pack pages) requires that
every secondary-index entry for a moved row be re-pointed atomically to the
new RowId. Making this crash-safe in a single mini-txn requires bracketing the
WAL_INSERT (new location), WAL_VACUUM (old slot), and WAL_INDEX updates for all
affected indexes inside ONE redo/undo unit spanning multiple heap pages and
index pages — which needs a new multi-page "compaction" WAL record type. That
is a `FORMAT_VERSION` bump and a new WAL record kind. Per the spec's landmine
note ("may defer V4 if it needs a format concern") and CLAUDE.md §6 ("Escalate
honestly"), V4 is deferred until the multi-page compaction WAL record is
designed and signed off. The per-page compaction that already ships (M10.d,
`compact_page`) handles intra-page dead-slot reclamation; V4 is purely a
cross-page defragmentation win and is not needed for correctness.
