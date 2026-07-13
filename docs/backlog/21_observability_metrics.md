# Observability metrics enrichment + Studio Observability tab

**Type:** Improvement
**Status:** SHIPPED (2026-07-13) — PR #62, branch `21-observability-metrics`. Metrics live
in `PROGRESS.md` → "Observability metrics enrichment (item 21)". Studio
Observability tab is out of scope for the engine repo (the tab consumes the
documented `/stats` + `/metrics` surfaces this item ships; no engine change
beyond them). Widget-traceability table: `docs/engine_access_guide.md` §9.

> The engine already exposes `Engine::stats()` / `GET /stats` / Prometheus
> `/metrics` + slow-query log (P6.g) and cheap counters (WAL bytes, rows
> decoded, dead/live estimates, autovacuum gauges). This item adds the missing
> production-grade metrics at existing chokepoints and a studio tab that
> consumes ONLY the documented surfaces (Milestone-18 boundary — no bespoke
> endpoints).

## Engine-side metric capture (cheap atomics/histograms at existing chokepoints)

| Metric | Why (widget it drives) |
|---|---|
| Per-statement-kind latency histogram (p50/p99, INSERT/UPDATE/DELETE/SELECT) | Query latency panel |
| Commit-rate counter + WAL-fsync latency histogram | Commits/s + durability-cost panel |
| Buffer-pool hit/miss + eviction counters | Cache-efficiency panel |
| Lock-wait count/duration + deadlocks-detected counter | Contention panel |
| **Oldest-snapshot / vacuum-horizon age gauge** | The item-16 postmortem metric — a pinned horizon is the #1 silent bloat/degradation cause; alertable |
| Per-table size (pages) + dead-tuple estimate | Table-health list (joins item-18 catalog) |
| Parallel-scan worker utilization vs `GLOBAL_MAX` cap | Worker-governance panel (item 15) |
| Session count + idle-reaper aborts + cursor count | Server-session panel (item 12) |

## Surfacing

- All via `stats()` → `GET /stats` (JSON) and `/metrics` (Prometheus); document
  each name in `engine_access_guide.md`. Consider a
  `unidb_catalog.table_stats` relation for the per-table rows (item-18 shape).
- Studio "Observability" tab renders the widgets above from `/stats` polling +
  `/metrics` scrape; no engine change beyond this item.

## Acceptance

- [x] Every widget maps to a named, documented metric — widget-traceability
      table in `docs/engine_access_guide.md` §9 (and echoed in the PR body).
- [x] Overhead measured honestly in-report: Table C (`UNIDB_BENCH=hiconc
      HICONC_ONLY=c`) and the mmreport ladder within noise (<1%) with metrics
      compiled in — capture is lock-free on hot paths (atomics + a fixed-bucket
      `AtomicHistogram`; no mutex on the commit/scan path). Numbers in
      `PROGRESS.md`.
- [x] Horizon-age gauge proven by test:
      `txn::tests::horizon_age_grows_while_rr_idle_and_resets_on_commit` — an
      idle RR session makes it grow; commit/abort resets it to zero.
