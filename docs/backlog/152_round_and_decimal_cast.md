# `ROUND()` function + `CAST … AS DECIMAL(p,s)` for numeric precision control

**Type:** Improvement
**Status:** NOT STARTED

> Filed 2026-08-03 from a unidb-studio walkthrough session. Two related gaps make
> it impossible to control numeric precision *inside a query*: there is no
> `ROUND()` function, and `CAST … AS DECIMAL(p,s)` is rejected. Exact arithmetic
> *is* available — but only if the column is declared `DECIMAL` up front; you
> can't round a `REAL`/`DOUBLE` result or coerce a float to a fixed scale in SQL.

## Summary

Aggregates over floating-point columns surface IEEE-754 artifacts
(`SUM(...) = 41565.849999999984`), and there is currently no in-query way to fix
the display or coerce to a fixed scale:

- **`ROUND(x, n)` is unimplemented** — `ROUND(SUM(...), 2)` →
  `unsupported function in query: ROUND`.
- **`CAST … AS DECIMAL(p,s)` is unsupported** — `CAST(x AS DECIMAL(10,2))` →
  `CAST to DECIMAL(10,2) is not supported in v1 (supported: TEXT/VARCHAR,
  INT/BIGINT, FLOAT/DOUBLE, BOOL)`.
- **`SUM`/aggregates over `DECIMAL` columns accumulate in f64** — the bigger
  finding. A `DECIMAL(10,2)` column *stores* exactly, but the aggregate
  accumulator is f64, so `SUM(3000 rows of 0.01) = 30.00000000000189` (verified
  live) instead of `30.00`. So switching money columns to `DECIMAL` fixes stored
  values but **not** report totals — the artifact survives.

What already works (so this is a targeted addition, not new machinery):

- **`DECIMAL(p,s)` column type is exact.** `SUM` over a `DECIMAL` column is
  exact — `SUM(10.10 + 20.20 + 0.01) = 30.31` (verified live), no float drift.
  So the fixed-point representation and exact addition already exist internally.

## Reproduction (live server)

```text
SELECT ROUND(SUM(line_total), 2) FROM order_items;
→ ERROR: unsupported function in query: ROUND

SELECT CAST(SUM(line_total) AS DECIMAL(10,2)) FROM order_items;
→ ERROR: CAST to DECIMAL(10,2) is not supported in v1

-- but a DECIMAL column already aggregates exactly:
CREATE TABLE t (amt DECIMAL(10,2));
INSERT INTO t VALUES (10.10),(20.20),(0.01);
SELECT SUM(amt) FROM t;   → 30.31   (exact)
```

## Impact

- Any report over `REAL`/`DOUBLE` money columns (revenue totals, averages) shows
  float noise and can't be cleaned up in SQL — callers must round client-side or
  redeclare the column as `DECIMAL`. (The Studio demo hit exactly this and worked
  around it by switching its money columns to `DECIMAL(10,2)`.)
- `AVG`/division still yields long fractions even over `DECIMAL` columns, with no
  `ROUND()` to trim them — so `DECIMAL` columns alone don't fully solve display.
- These are table-stakes analytics functions; their absence is a rough edge for
  the "run real SQL" story.

## Scope / proposal

Two independent, small additions (either can ship alone):

1. **`ROUND(x)` / `ROUND(x, n)`** scalar function in the executor's function
   table — banker's-or-half-up rounding to `n` decimals (pick one, document it),
   returning the input's numeric type (or `DECIMAL` when `n` is given).
2. **`CAST(expr AS DECIMAL(p,s))`** — reuse the existing `DECIMAL` fixed-point
   representation the column type already uses; scale/round `expr` to `s`.

Non-goals: full ANSI numeric tower, arbitrary-precision beyond the existing
`DECIMAL(p,s)` limits.

## Acceptance

- `ROUND(SUM(col), 2)` returns a value rounded to 2 places for `REAL`/`DOUBLE`
  and `DECIMAL` inputs.
- `CAST(<float expr> AS DECIMAL(10,2))` succeeds and yields the exact fixed-scale
  value.
- Unit tests covering rounding modes, negative numbers, and `n = 0`; the
  documented rounding mode noted in `docs/`. Executor-only change; crash harness
  unaffected.

## Provenance

Discovered 2026-08-03 building the unidb-studio walkthrough: the analytics
showcase query `SUM(oi.line_total)` over `REAL` columns rendered
`41565.849999999984`; neither `ROUND()` nor `CAST … AS DECIMAL` was available to
clean it up in-query. The demo was fixed by declaring money columns `DECIMAL`,
which confirmed exact `DECIMAL` aggregation already exists.
