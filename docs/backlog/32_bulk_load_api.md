# Item 32 — Bulk Load HTTP API

| Field        | Value                                              |
|--------------|----------------------------------------------------|
| **Type**     | Performance / Feature                              |
| **Priority** | High                                               |
| **Status**   | ✅ SHIPPED — branch `32-bulk-load-api` (see `PROGRESS.md`) |
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

This is a limitation of the **per-request REST/JSON path**, not the engine's
insert speed: each `/sql` call pays full HTTP overhead + JSON deserialization +
its own auto-commit, with no bulk-path short-circuit. (The engine's own batched
insert — B-tree included — is ~30 µs/row / ~31k rows/sec; see the Root Cause
attribution note.)

For comparison, PostgreSQL's `COPY` protocol loads at **100 k–1 M rows/second**
because it bypasses the per-call overhead entirely.

---

## Root Cause

> **Attribution correction (2026-07-14).** The ~1.5 ms/row is **not** engine
> B-tree insert cost — do not read it that way. The engine inserts **~30 µs/row
> *including* B-tree index maintenance** (measured: 31k–34k rows/sec batched,
> 10k→2M rows, `docs/performance/multi_model_report_*.md` Table 3.1). The
> ~1.5 ms/row is the **per-request HTTP + per-statement auto-commit path** — a
> ~50× envelope *around* a fast engine insert. So the lever is amortizing the
> HTTP/transaction boundary over many rows (this bulk API), **not** optimizing
> the B-tree, which is already cheap. Corroboration: item 12 collapsed 500-row
> HTTP inserts 718 ms → 35 ms (**20.5×**) by batching commits, with no B-tree
> change.

1. **HTTP overhead (dominant)** — TCP + header parsing + JSON serialize/
   deserialize: ~2 ms per call. One `/sql` call per row means this is paid
   per row.
2. **Per-statement auto-commit / WAL fsync** — each auto-commit row is its own
   durable transaction (group-committed ~4 ms fsync amortized across the
   batch). Batching thousands of rows into **one** transaction removes almost
   all of this.
3. **B-tree index maintenance** — real but **small**: it is already included in
   the ~30 µs/row batched figure, so it is *not* what creates the 50× HTTP-path
   gap. A bulk-sort-then-append for sequential PK ranges is a possible *further*
   micro-optimization, not the primary fix.
4. **In-transaction inserts don't help via `/sql`** — wrapping per-row `/sql`
   calls in `/txn/begin … /txn/commit` still pays per-call HTTP overhead, so it
   does not close the gap; the win comes from **one server-side loop over a
   streamed body**, which this endpoint provides.

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

### Expected performance gain (target)

Eliminating per-call HTTP overhead and processing rows in a tight server loop
was *targeted* at **50 k–200 k rows/second**, consistent with PostgreSQL's
`COPY` path.

### Measured result (2026-07-14) — honest correction, target not reached

The shipped endpoint achieves **~12 k–31 k rows/sec**, reproducibly measured
(release; the `#[ignore]`d `tests/server_bulk.rs::bulk_throughput_measurement`,
server-reported `elapsed_ms`):

| Rows | No secondary index | With a B-tree index |
|-----:|-------------------:|--------------------:|
| 100 k | 17.2 k rows/sec | 16.6 k rows/sec |
| 200 k | **30.6 k** (amortizes toward the ~33 k engine batched ceiling) | **12.5 k** (degrades — B-tree cost grows with the tree) |

That is a **~20–50× win over the ~640 rows/sec per-row `/sql` path**, but
**below the 50 k–200 k target**. Why: each row still pays JSON parse +
type-coercion + a `execute_prepared` call on top of the engine's ~30 µs/row
insert, and a B-tree index's per-insert cost rises as the table grows. The
engine's own batched insert ceiling (~31 k–34 k rows/sec, single-threaded, with
one index) *bounds* the SQL-path approach — reaching 50 k+ needs a lower-level
path. **Follow-up filed:** channel-streamed body → a lower-level bulk-insert
loop (bypassing per-row SQL parse/coercion) and/or parallel apply; an optional
`?chunk=N` commit mode to bound the whole-body undo/horizon footprint.

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
