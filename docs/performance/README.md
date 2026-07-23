# docs/performance — benchmark reports index

> Added 2026-07-22. This folder holds the **committed measurement record**
> (the dated reports below ARE git-tracked — they are the durable evidence
> trail, see `.gitignore`'s note) plus two narrative reference docs. Durable
> per-milestone numbers live in `PROGRESS.md`; this folder is the raw record
> behind them.

## Which report is current?

- **Authoritative full baseline:** `report_20260721_035629.md` — the
  consolidated Docker bench of 2026-07-21 (the current `MM_BASELINE`
  carry-forward anchor, item 105).
- `report_20260722_002217_ab_oldcode_51022be.md` is **not** a baseline — it is
  the item-108 controlled A/B evidence run (old code at `51022be`) proving the
  07-19→07-21 ratio drift was environmental.
- The next full Docker bench on current main (queued in
  `docs/backlog/backlog_index.md` "Next up") will supersede the 07-21 baseline
  and become the first official record of item 107's W4/W0 ladder collapse.

## File families

| Pattern | What it is | Producer |
|---|---|---|
| `report_YYYYMMDD_HHMMSS.md` | CRUD decompose vs Postgres (Docker), per-operation throughput + WAL B/row + internal counters | `scripts/report.sh` / `benches/decompose.rs` |
| `multi_model_report_*.md` | Multi-model suite: W0→W4 ladder, filtered/aggregate tables, Table 4 replaced-stack comparison | `scripts/report.sh` (multi-model mode) |
| `conc_matrix_*.md` | Concurrency matrix (writers × readers) pass/fail + throughput grid | concurrency harness |
| `benchmark_*.md` | Older naming of the dated bench reports (2026-07-16/17 era), superseded by `report_*` naming | pre-rename `report.sh` |
| `stats_YYYYMMDD.csv` | Phase/stats CSV snapshot accompanying a run | `report.sh` tooling |

Cross-run ratio caveat (item 108): unidb÷PG ratios are comparable across runs
**only when the PG-absolute environment canary in `compare_bench.py` is
quiet**; otherwise judge by absolute numbers + WAL B/row.

## Narrative reference docs

- `buffer_pool_tuning.md` — durable buffer-pool sizing/tuning reference (not a
  timestamped snapshot).
- `high_scale_concurrency.md` — high-scale concurrency investigation (base
  2026-07-10, with post-fix and default-ON addenda; predates the items 37–112
  perf era — read with that in mind).
