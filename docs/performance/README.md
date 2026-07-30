# docs/performance — benchmark reports index

> Added 2026-07-22. This folder holds the **committed measurement record**
> (the dated reports below ARE git-tracked — they are the durable evidence
> trail, see `.gitignore`'s note) plus two narrative reference docs. Durable
> per-milestone numbers live in `PROGRESS.md`; this folder is the raw record
> behind them.

## Which report is current?

- **Authoritative full baseline:** `report_20260728_102745.md` — the full
  Docker bench of 2026-07-28 on main `7c064f1` (all tables, 83m 38s, RSS
  521 MiB, environment canary quiet). First full-report capture of the
  items-115/116 SELECT/INSERT work: **SELECT filtered 0.58→0.77×** (one-shot
  warm-path fix landing exactly as the #210 cert predicted), UPDATE HOT
  1.19×, COUNT(*) 49.6×, DELETE all 4.12×, DELETE selected 1.91×. Unified
  multi-model commit moat intact (W4/W0 13.6× at 100k; four model-writes in
  one atomic txn vs the replaced stack's four round-trips).
  **Known gap:** item 106 Unit 3's NEAR latency win (gate 630→482 µs) is
  NOT in this report — `report.sh` does not measure NEAR; that result is
  certified only by the native `perf_item106` harness (standing Linux
  NEAR-spot-check gap).
- Superseded baseline: `report_20260723_124415.md` — full Docker bench of
  2026-07-23 on main `0324dc5` (was the `MM_BASELINE` carry-forward anchor;
  first record of item 107's W4/W0 ladder collapse 96→34× at 100k and the
  event-rung finding filed as item 114).
- Older: `report_20260721_035629.md` (2026-07-21 consolidated bench).
- `report_20260722_002217_ab_oldcode_51022be.md` is **not** a baseline — it is
  the item-108 controlled A/B evidence run (old code at `51022be`) proving the
  07-19→07-21 ratio drift was environmental.

## File families

| Pattern | What it is | Producer |
|---|---|---|
| `report_YYYYMMDD_HHMMSS.md` | CRUD decompose vs Postgres (Docker), per-operation throughput + WAL B/row + internal counters | `scripts/report.sh` / `benches/decompose.rs` |
| `multi_model_report_*.md` | Multi-model suite: W0→W4 ladder, filtered/aggregate tables, Table 4 replaced-stack comparison | `scripts/report.sh` (multi-model mode) |
| `conc_matrix_*.md` | Concurrency matrix (writers × readers) pass/fail + throughput grid | concurrency harness |
| `benchmark_*.md` | Older naming of the dated bench reports (2026-07-16/17 era), superseded by `report_*` naming | pre-rename `report.sh` |
| `stats_YYYYMMDD.csv` | Phase/stats CSV snapshot accompanying a run | `report.sh` tooling |
| `report_YYYYMMDD_HHMMSS.html` | Styled, self-contained HTML view of the same-named `.md` (winner pills, ratio coloring, light+dark). **Derived + git-ignored** — the `.md` is authoritative | `scripts/render_report.py` (auto, per run) |

Regenerate an HTML view at any time: `python3 scripts/render_report.py docs/performance/report_<ts>.md`.

Cross-run ratio caveat (item 108): unidb÷PG ratios are comparable across runs
**only when the PG-absolute environment canary in `compare_bench.py` is
quiet**; otherwise judge by absolute numbers + WAL B/row.

## Narrative reference docs

- `buffer_pool_tuning.md` — durable buffer-pool sizing/tuning reference (not a
  timestamped snapshot).
- `high_scale_concurrency.md` — high-scale concurrency investigation (base
  2026-07-10, with post-fix and default-ON addenda; predates the items 37–112
  perf era — read with that in mind).
