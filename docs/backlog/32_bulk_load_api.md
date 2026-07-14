# Item 32 — Bulk Load HTTP API

| Field        | Value                                              |
|--------------|----------------------------------------------------|
| **Type**     | Performance / Feature                              |
| **Priority** | High                                               |
| **Status**   | 🟡 BACKLOG                                         |
| **Blocks**   | Demo seeding > 200k rows; data-migration tooling   |

---

## Problem

The existing `POST /sql` endpoint processes ~600–700 rows/second for INSERT
workloads via HTTP. This is measured, not estimated:

| Batch size | ms/row (auto-commit) |
|------------|----------------------|
| 50 rows    | 1.56 ms              |
| 100 rows   | 1.57 ms              |
| 200 rows   | 1.68 ms              |
| 500 rows   | 1.94 ms              |
| 1000 rows  | 2.55 ms              |

At 1.5 ms/row:

- 200 k rows → **~5 minutes** (demo maximum)
- 1 M rows  → **~25 minutes**
- 3 M rows  → **~75 minutes**

This is a fundamental limitation of the REST/JSON path, not a tuning problem:
each `/sql` call pays full HTTP overhead, JSON deserialization, and individual
B-tree row insertions with no bulk-path short-circuit.

For comparison, PostgreSQL's `COPY` protocol loads at **100 k–1 M rows/second**
because it bypasses the per-call overhead entirely.

---

## Root Cause

1. **HTTP overhead** — TCP + header parsing + JSON serialize/deserialize: ~2 ms per call.
2. **Per-row B-tree cost** — each row requires an index insertion; there is no
   bulk-sort-then-append path for sequential PK ranges.
3. **WAL fsync per commit** — 4 ms per auto-commit batch (this is the *smaller*
   cost, ~15% of total; the per-row processing dominates).
4. **In-transaction inserts are SLOWER** — the MVCC versioning overhead makes
   transactional batches cost *more* per row than auto-commit, so wrapping in
   `/txn/begin` ... `/txn/commit` does not help bulk loading.

---

## Proposed Solution

### `POST /tables/{name}/bulk` endpoint

Accept a streaming request body in **NDJSON** (newline-delimited JSON) or
**CSV** format. Process rows in a server-side tight loop without one-HTTP-call-
per-batch overhead:

```
POST /tables/customers/bulk
Content-Type: application/x-ndjson
Authorization: Bearer <token>

{"id":1,"name":"Alice","email":"a@x.com","city":"NYC","country":"US","created_at":1700000000}
{"id":2,"name":"Bob","email":"b@x.com","city":"LA","country":"US","created_at":1700000000}
...
```

Response (streaming progress or final summary):

```json
{ "inserted": 200000, "errors": 0, "elapsed_ms": 1840 }
```

### Design decisions

| Decision | Recommendation |
|----------|----------------|
| Format | NDJSON first (trivially generated from Python/JS); CSV as stretch |
| Atomicity | Whole-body in one transaction (rollback on any error) |
| Max size | Stream chunked — no body-size limit at the handler level |
| Validation | FK checks deferred to commit (bulk mode); violators roll back whole batch |
| Auth | Same Bearer JWT as every other route |
| Idempotency | Not guaranteed; caller should truncate first if re-running |

### Expected performance gain

Eliminating per-call HTTP overhead and processing rows in a tight server loop
should yield **50 k–200 k rows/second** (10–100× improvement), consistent with
what PostgreSQL achieves with its `COPY` path.

---

## Demo / Test plan

1. `POST /tables/customers/bulk` with 1 000 rows NDJSON → verify `inserted: 1000`.
2. Seed 2 M rows from `demo/seed.py` with the new endpoint; time < 2 minutes.
3. `demo/benchmark.py` comparison: unidb bulk vs PostgreSQL `COPY` vs current SQL path.

---

## Effort estimate

- Server handler + NDJSON parser: ~1 day
- Integration into existing WAL / commit path: ~0.5 day
- `demo/seed.py` update to use new endpoint: ~1 hour
- Tests: ~0.5 day
