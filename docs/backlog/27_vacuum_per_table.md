# Vacuum — per-table accounting, cost throttle, whole-table compaction

**Type:** Improvement
**Status:** NOT STARTED

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

- [ ] Per-table trigger + `vacuum_table` proven (V1/V2).
- [ ] Throttle bounds foreground impact (V3 measured).
- [ ] Compaction releases trailing pages, crash-safe, index-correct (V4) — or
      V4 deferred with a dated note if the re-point needs a format change.
- [ ] Crash harness green; PROGRESS.md before/after bloat numbers.
