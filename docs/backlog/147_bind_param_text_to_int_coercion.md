# Bind-param text→int coercion missing (contradicts documented `POST /sql` contract)

**Type:** Improvement
**Status:** NOT STARTED

> Studio-flagged, found while running unidb-studio v2's parity/design-review
> checklist for real (2026-08-02) — traced to a genuine engine gap, not a
> Studio bug, and reproduced with plain `curl` against `POST /sql`, no
> Studio code involved.

## The contract, as documented

`docs/REST_API.md` line 237, describing the bind-parameter form of `POST /sql`
(`{"sql": "... VALUES ($1, $2)", "params": [...]}`):

> A JSON string binds as text (later coerced to the column's type — UUID,
> TIMESTAMP, etc.), a number as int/float, a numeric array as a vector.

This reads as a general "string param → target column type" coercion
guarantee, with UUID/TIMESTAMP given as examples, not an exhaustive list.

## What actually happens

A JSON **string** param does not coerce into an `INT`/`BIGINT` column — it
hard-fails with `SQL_PLAN_ERROR`. A JSON **number** param works fine. Repro
(fresh table, no Studio involved):

```bash
curl -X POST $URL/sql -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"sql":"CREATE TABLE t (id INT PRIMARY KEY, name TEXT)"}'

# number param -- works
curl -X POST $URL/sql ... -d '{"sql":"INSERT INTO t (id, name) VALUES ($1, $2)", "params":[998,"ok"]}'
# => {"results":[{"count":1,"type":"inserted"}]}

# string param -- fails, contradicting the documented contract
curl -X POST $URL/sql ... -d '{"sql":"INSERT INTO t (id, name) VALUES ($1, $2)", "params":["997","fails"]}'
# => {"error":"SQL planning error: table 't' column 'id': expected Int64, got Text(\"997\")","code":"SQL_PLAN_ERROR"}
```

Confirmed on both `INT` and `BIGINT` PRIMARY KEY columns. Not yet checked
against `FLOAT`/`DOUBLE`/`DECIMAL`/`NUMERIC` columns — worth confirming
whether the gap is int-specific or covers every numeric type.

## Why this matters beyond the one-line contract mismatch

unidb-studio's CSV import (`CsvUpload.tsx`) assumes exactly this coercion —
its design comment says "Values are quoted, not bound as params... [and]
coerced to each column's type by the engine" — so importing a CSV with an
`INT`/`BIGINT` id column into a fresh table fails outright on every row
(rolled back atomically, at least, so no partial-import corruption). This is
very likely to be the **first thing** anyone hits importing sample data with
a numeric primary key, which is a common shape.

(Note: the Studio's CSV path builds literal SQL text, not `$n` bind params,
so it's a step further removed from the documented contract than the repro
above — but the repro above is the cleaner one since it's the *exact*
documented mechanism, isolates the coercion question from SQL-literal
parsing, and is a strict subset of what CSV import needs anyway.)

## Two ways to close this — pick one, don't guess

1. **Implement the coercion** the docs promise: a bind-param string value
   destined for a numeric column attempts a numeric parse before binding,
   same posture already presumably used for UUID/TIMESTAMP (find that code
   path and extend it, or confirm it's actually the same generic "try to
   parse the target type" step and int was just never wired into it).
2. **Narrow the docs** if int/float coercion was never intended to be
   guaranteed (e.g. if UUID/TIMESTAMP get bespoke parsers but numeric types
   deliberately don't, for a real type-safety reason) — correct
   `docs/REST_API.md` line 237 to say so explicitly, and flag the CSV-import
   caller-side assumption as a known limitation instead.

Either is a legitimate outcome — this file doesn't presume which. Whoever
picks this up should find the actual coercion code (search around wherever
UUID/TIMESTAMP string-to-type binding happens) before deciding, per this
repo's own §0.6 "find the real code path first" rule, not guess from the
error message alone.

## Acceptance (once a direction is chosen)

- If coercing: `INSERT ... VALUES ($1)` with a JSON string param into an
  `INT`/`BIGINT` column succeeds when the string parses as a valid integer,
  and still fails cleanly (existing `SQL_PLAN_ERROR`, no crash) on a
  non-numeric string. Add a regression test alongside wherever UUID/TIMESTAMP
  coercion is tested. Crash harness unaffected (this is bind-time type
  resolution, not a storage-format change).
- If narrowing the docs: `docs/REST_API.md` line 237 no longer implies
  numeric coercion; note added here pointing at the corrected line.
