# 105 — Selective bench runs + baseline carry-forward

**Type:** Improvement
**Status:** ✅ SHIPPED 2026-07-21 (same-day implementation; this file records the design)

## Problem

A full `scripts/report.sh` run takes ~4 h, which is unjustifiable for per-item
validation when most tables are unaffected by the change. Measured breakdown
(via the per-phase `docker stats` sample counts in `report_20260719_234504.md`,
230 min total):

| Block | Time | Root cause |
|---|---:|---|
| Tables 1+2 (W0→W4 ladder) | ~2.5 h | W2–W4 pre-grows build HNSW + graph indexes **synchronously**, row by row (items 63/65/92 — incremental HNSW insert is the open bottleneck) |
| Table 4 at 100k | ~45 min | 100k txns × incremental HNSW insert (12 txns/s) |
| Table 3/3.1 CRUD + bulk | ~15–20 min | includes 1M/2M bulk phases |
| Table 5 FK stress | ~5 min | |
| builds + PG setup + conc matrix | ~15 min | |

~85 % of wall clock is one thing: the slow incremental HNSW insert path. The
bench time is itself a benchmark finding — fixing item 92 shrinks the report
for free.

## Bugs found & fixed en route

1. **Docker mode ignored every table-selection knob.** `MM_TABLES`,
   `MM_SKIP_TABLE4`, `MM_SKIP_TABLE5` were never passed through
   `docker-compose.yml`, so the documented per-item profiles
   (`MM_SKIP_TABLE4=1 … scripts/report.sh --docker`) silently ran the full
   ~4 h bench. Now threaded through `docker_report.sh` → compose → container.
2. **`MM_TABLES` allowlist was only honored by Tables 4 and 5** — Tables 1, 2,
   3, 3.1 always ran. `MM_TABLES=3` claimed "only Table 3 (~15 min)" but still
   paid the ~2.5 h ladder. All tables now gated.
3. **`compare_bench.py` W4/W0 collision.** Any row with an integer first column
   and a `×` last column was parsed as W4/W0 — Table 4 rows share that shape
   and silently overwrote Table 1's entries. Parsing is now section-aware.

## What shipped

- `MM_SKIP_LADDER=1` — skip Tables 1+2 (the biggest sink). Tables 1+2 are one
  measurement; `MM_TABLES` listing either runs both; Table 3.1 is gated with 3.
- Skipped tables emit a `_Skipped:` marker under their `## Table N` heading.
- `MM_BASELINE=<report.md>` (`scripts/stitch_baseline.py`, hooked in
  `report.sh` post-processing, host-side so it works in both native and Docker
  modes): every skipped table is copied from the baseline with a provenance
  stamp — "**Carried forward — NOT re-measured in this run**", source file,
  commit, date. A baseline section that is itself skipped is never copied; a
  chained carry-forward keeps its ORIGINAL stamp and warns.
- `compare_bench.py` excludes carried-forward sections from the delta table.
- Report header row "Tables 1+2 (W0→W4 ladder): measured/SKIPPED".

## Honesty guardrails (§6)

- Carry-forward is only valid when the change provably does not touch shared
  layers (WAL, commit path, buffer pool, heap, page format) — those affect
  EVERY table; re-run the full bench for such changes.
- Take a fresh full baseline per major release so drift cannot accumulate
  across stitched reports.
- The provenance stamp makes a stale number impossible to mistake for a fresh
  measurement, in-report, with no external context needed.

## Recommended per-item profile (CRUD/WAL/B-tree items)

```bash
MM_SKIP_LADDER=1 MM_SKIP_TABLE4=1 MM_SKIP_TABLE5=1 \
  MM_BASELINE=docs/performance/report_<last_full>.md scripts/report.sh   # ~30–45 min
```

Vector/HNSW items still run everything — Tables 1/2/4 ARE the signal.
