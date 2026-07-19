**Type:** Performance
**Status:** ⏳ NOT STARTED

# Item 99 — `/batch-sql` endpoint: amortise HTTP overhead over N queries

## Problem

Every `/sql` POST pays ~10–12 ms of HTTP overhead on Docker-Mac (TCP +
HTTP headers + JSON encode/decode + axum handler + Python `requests` decode).
Engine execution for small-table queries (COUNT, filtered SELECT, GROUP BY) is
< 0.5 ms. The overhead-to-engine ratio is ~20–100×.

`compare.py` issues **9 sequential `/sql` calls** for its benchmark section.
Each pays the full 10–12 ms floor → **~90 ms overhead** on a workload whose
engine time is < 5 ms total. Result: unidb reports **109.9 ms vs PG 7.0 ms
= 15.7×** even though the engine itself is competitive.

Postgres escapes this via psycopg2 using the native PostgreSQL wire protocol —
no HTTP framing, no JSON serialization, persistent binary socket. unidb's REST
transport cannot match that per-request cost. The correct fix is to **amortise
the one unavoidable HTTP round-trip over all N queries**.

## What to build

### 1. `POST /batch-sql` route

```
POST /batch-sql
Content-Type: application/json

{
  "statements": [
    "SELECT COUNT(*) FROM customers",
    "SELECT COUNT(*) FROM orders",
    "SELECT status, COUNT(*) FROM orders GROUP BY status"
  ],
  "stop_on_error": false   // optional, default false
}
```

Response:

```json
{
  "results": [
    {"columns": ["count"], "rows": [[1000]]},
    {"columns": ["count"], "rows": [[2000]]},
    {"columns": ["status","count"], "rows": [["pending",120],["shipped",880]]}
  ],
  "errors": [null, null, null]
}
```

- Each statement runs as an independent **one-shot auto-commit** (same
  semantics as a single `/sql` without a `X-Txn-Id` header).
- `stop_on_error: false` (default): execute all statements; failed slots get
  `null` in `results` and an error string in `errors`.
- `stop_on_error: true`: stop at the first error; remaining slots are `null`
  with `"skipped"` in `errors`.
- No cursor mode, no isolation override, no params (follow-on).
- Auth: same `authorize_sql` check per statement (honour per-user grants).
- Max statements per batch: 256 (config: `UNIDB_BATCH_SQL_MAX`, default 256).

### 2. Router registration

In `src/server/router.rs`:

```rust
.route("/batch-sql", post(handlers::post_batch_sql))
```

### 3. Handler (`src/server/handlers.rs`)

```rust
pub async fn post_batch_sql(
    Extension(current_user): Extension<CurrentUser>,
    State(state): State<AppState>,
    Json(body): Json<BatchSqlRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // validate batch size
    // for each statement: authorize + execute_sql_read (or begin/exec/commit)
    // collect results + errors
    // return BatchSqlResponse
}
```

The hot path for read-only SELECTs uses `execute_sql_read` (same concurrent
read path as single `/sql`) — no begin/commit round-trips per statement.

### 4. DTO additions (`src/server/dto.rs`)

```rust
#[derive(Deserialize)]
pub struct BatchSqlRequest {
    pub statements: Vec<String>,
    #[serde(default)]
    pub stop_on_error: bool,
}

#[derive(Serialize)]
pub struct BatchSqlResponse {
    pub results: Vec<Option<serde_json::Value>>,
    pub errors: Vec<Option<String>>,
}
```

### 5. `docs/REST_API.md` update

Add the new route to the REST reference with request/response schema and
`stop_on_error` semantics.

## Projected impact on compare.py

| | Before | After |
|---|---|---|
| HTTP round-trips | 9 × 10–12 ms = ~100 ms | 1 × 10–12 ms = ~11 ms |
| Engine time (9 queries) | ~5 ms | ~5 ms |
| **Total unidb** | **~109 ms** | **~16 ms** |
| PG (psycopg2) | 7 ms | 7 ms |
| **Ratio** | **15.7×** | **~2.3×** |

compare.py needs a one-call update to use `/batch-sql` for its benchmark
section (the seeding section stays on individual `/sql` calls as-is).

## Targets

- `POST /batch-sql` with 9 queries identical to compare.py benchmark section:
  total unidb time **≤ 20 ms** (down from 109.9 ms).
- compare.py ratio: **≤ 3×** vs PG (down from 15.7×).
- No regression on single `/sql` endpoint.
- Max-batch safety: 257-statement request returns 400 with `BATCH_TOO_LARGE`.

## Acceptance criteria

- Unit test: batch of 3 SELECT + 1 failing stmt with `stop_on_error: false` →
  3 results + 1 error, all slots present.
- Unit test: `stop_on_error: true` → first error stops; remaining slots = null
  + "skipped".
- Auth test: batch with a stmt the user has no SELECT privilege on → that slot
  gets a permission error, others proceed.
- Max-batch test: 257 stmts → 400 `BATCH_TOO_LARGE`.
- `docs/REST_API.md` updated with the new route.
- No existing tests broken.

## ROI

- Single change eliminates ~90 ms of HTTP overhead that currently makes
  compare.py look 15× slower than PG.
- Useful beyond compare.py: any client running analytical dashboards or
  multi-query reports benefits from amortised transport cost.
- Low implementation risk: each statement uses the existing `execute_sql_read`
  / begin+commit path unchanged; the handler is new glue only.
