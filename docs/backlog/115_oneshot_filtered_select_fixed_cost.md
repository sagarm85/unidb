# Item 115 — One-shot filtered SELECT: kill the first-query fixed cost

**Type:** Performance
**Status:** ✅ SHIPPED 2026-07-24 (PR #210) — target met — Unit 1 (open-time warmup) certified:
Docker one-shot **0.77×** (target ≥0.75, was 0.58×), unidb absolute 3.14M→4.65M rec/s
(`docs/performance/report_20260724_000942.md`, canary quiet). Units 2/3 (per-table
resolve first-use ~180 µs, per-page madvise) remain OPTIONAL margin work — un-park
if a future run dips below target.

**Target:** Table 3 `SELECT filtered` one-shot ratio ≥ **0.75×** vs PG (user-set
2026-07-24; was 0.58× in the 07-23 baseline). The bench times ONE cold
`execute_sql` — this item attacks the cold premium, complementing item 109
(which fixed the warm path).

## Step-0 attribution (probe: `tests/perf_item115.rs`, native, 200k rows)

One-shot premium = first-query total − warm total = **852 µs** (1,089 vs 237).
Decomposition by prewarm bisection (`ITEM115_PREWARM=1|2`):

| component | µs | evidence |
|---|---:|---|
| global SELECT-path first-use | ~590 | collapses when ANY prior SELECT ran (unrelated tiny table): parse/plan lazy init ~230, executor first-use ~180, resolve global share ~140, leaf ~20 |
| per-table resolve first-use | ~180 | collapses only after a SELECT on the SAME table (disjoint key range) |
| per-page first-touch | ~90 | remains even then — first page-copy+CRC of the target pages (~50-100 pages at 5% selectivity) |

Notable: the per-NEW-STATEMENT premium (plan-cache miss) is only **~22 µs** —
the plan cache was never the problem; the FIRST plan ever built was.

## Unit 1 — open-time warmup (SHIPPED, this PR)

`Engine::warm_query_path()` at the end of open: (a) `parse_sql_cached` on a
representative SELECT text (warms sqlparser + logical-plan machinery), (b)
`parallel_scan::warm_pool()` — one no-op dispatch with a per-worker allocation
(warms cond-var wake paths + worker allocator arenas). Read-only: **no
transaction, no WAL append, no storage access** (a `begin()` would append
WAL_TXN_BEGIN — deliberately avoided; safe for replicas/read-only media).
`UNIDB_WARM_QUERY_PATH=0` disables.

**Measured:** native one-shot 1,089 → **744 µs** (premium 852 → 490); warm and
plan-miss paths unchanged. Permanent `Q115_*` statement-phase timers added
(parse+plan / RLS / execute) — item-92-style, ~40 ns/statement.

## Remaining units

- **Unit 2 — per-table resolve first-use (~180 µs):** root-cause which
  per-table lazy state the first resolve pays (SharedPageReader setup,
  partition planning, or cache construction) and either move it to
  CREATE INDEX/open or make it cheap. Expected to be the difference between
  ~0.85× and ~1.0× one-shot in Docker.
- **Unit 3 — per-page first-touch (~90 µs):** only if the cert demands it —
  `madvise WILLNEED` on the candidate page set before resolve (item 70
  precedent for seq scans).

## Cert (2026-07-24, PR #210)

`MM_TABLES=3` fresh run (no stitch — shared layers touched), canary quiet vs the
07-23 baseline: **SELECT filtered one-shot 0.58× → 0.77×** (+33% ratio, +48%
unidb absolute); 0.70× vs PG-uncapped (the #213 sensitivity rows). First cert
attempt was discarded: a disclosed 2-3 min cross-session CPU overlap plus a
freshly-restarted Docker daemon produced INSERT 0.16× + FPI-shaped WAL
anomalies; the clean rerun restored all rows to normal bands — recorded here
as evidence for the exclusive-machine-time rule.
