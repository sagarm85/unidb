# Subscription CDC — canonical envelope (before/after), format adapters, lag observability

**Type:** Improvement
**Status:** NOT STARTED

> **V1 of "Supabase-Realtime-grade table subscription."** The subscription
> *mechanism* already ships (items 20 + 26): `enable_events(table)` captures CRUD
> atomically with the commit; `GET /events/subscribe` (SSE, JSON) serves durable
> consumers (offset-resumed, at-least-once, zero-loss-across-crash — proven) and
> ephemeral browser live-tail; `unidb-dispatch` fans out with retry/DLQ. This
> item closes the **payload + observability** gaps that separate it from
> Debezium/Supabase parity — NOT the transport, which exists.
>
> Grounding (verified on `main` @ item 26): events are rows in `__events__`
> `{seq, xid, table_name, op, payload:json}`; capture is `send_event_capture`
> (`src/sql/executor.rs`). The payload is a JSON **column** → enriching its shape
> is **not** an on-disk format change (no `FORMAT_VERSION` bump). The UPDATE
> path already holds the pre-mutation `row` before `set_column` overwrites it;
> DELETE already passes the old row; INSERT has only the new row.

## The three gaps this closes

1. **`before`/`after` row images.** Today one `payload`. Debezium/Supabase both
   need before+after for UPDATE (and before for DELETE, after for INSERT).
2. **No canonical envelope + format adapters.** Downstream tools expect either
   Debezium (`op`/`before`/`after`/`source`, single-char op) or Supabase
   (`eventType`/`new`/`old`, flat). Betting on one vendor format is wrong.
3. **Lag is not observable.** `unidb-dispatch` has a per-poll `backlogged` bool;
   there is no *queryable* per-consumer lag ("consumer X is N events / T seconds
   behind"). This is the "find the lag/offset for tracking" ask.

## Scope (V1)

### C1 — before/after capture (MUST, engine, small)
- Capture the old image: UPDATE clones the pre-mutation `row` before
  `set_column`; DELETE uses the row it already has; INSERT has after only.
- Enrich the captured payload to carry both images. **Back-compat landmine:**
  existing consumers (item-20 `unidb-dispatch` tests, the SSE `data`) read the
  flat `payload` today — preserve them. Options: keep `payload` = `after`
  alongside new `before`/`after`, OR add a documented `envelope_version`. Pick
  one with a dated note; **item-20 dispatch tests must stay green.**
- Add `ts_ms` (capture wall-clock). `source.lsn`: the commit LSN is assigned at
  commit, not at per-statement capture — `seq` is the authoritative ordering
  cursor, so `source` carries `{seq, txId=xid, table, schema}` in V1 and `lsn`
  is a documented follow-up if commit-time wiring is wanted (don't force it).
- No `FORMAT_VERSION` bump (payload is a JSON column). Crash harness unchanged.

### C2 — canonical envelope + adapters (MUST native+debezium; SHOULD supabase)
- **Native canonical envelope** (default) — the source of truth, rich enough to
  derive both externals:
  ```json
  { "seq": 42, "op": "u", "table": "orders", "schema": "public",
    "ts_ms": 1752000000000, "xid": 1017, "before": {…}, "after": {…} }
  ```
- **`?format=debezium`** (MUST) → `{payload:{op:"u",ts_ms,before,after,
  source:{seq,txId,table,schema}}}` (single-char op c/u/d).
- **`?format=supabase`** (SHOULD) → flat `{eventType:"UPDATE",new,old,schema,
  table,commit_timestamp}`.
- Adapters are pure serialization over C1's fields (~40 lines each); all read the
  same captured event. `seq` stays the offset/lag cursor in every format.
- Applies to `GET /events/subscribe` and is documented for the `unidb-dispatch`
  consumer path too (a `Filter`/format option, or a projection helper).

### C3 — lag observability (MUST)
- **`unidb_catalog.subscription_lag`** relation (item-18 virtual-relation shape):
  per consumer → `consumer, offset, max_seq, lag_events (= max_seq - offset),
  oldest_unconsumed_ts, lag_seconds`. Queryable over the normal SQL surface.
- **`/stats` gauges** per consumer (item-21 shape) so Prometheus/alerts can watch
  lag; the studio Observability/Events tab renders from it. Reuse the existing
  `__consumers__` offsets + `__events__` max seq — no new bookkeeping.

### C4 — docs (MUST)
- `engine_access_guide.md` §8 (event stream): the subscription contract
  (enable → subscribe → ack = offset commit), at-least-once semantics, the three
  formats with example frames, and **how to detect/bound lag** (query
  `subscription_lag`, watch the gauge, the retention-pin warning from item 26).

## Non-goals (V1)
- Not Kafka transport (SSE + dispatch cover it; a Kafka bridge stays item-20's
  optional follow-up).
- Not subscription-level RLS (row filtering by the subscriber's policy) — depends
  on item 24 (authz v2); note as a follow-up.
- Not `source.lsn` if it needs commit-time capture wiring (follow-up).

## Acceptance
- [ ] UPDATE event carries correct `before` AND `after`; INSERT after-only;
      DELETE before-only — unit + a round-trip test.
- [ ] `format=debezium` and `format=supabase` frames match documented shapes for
      all three ops; `format=native` default; existing flat consumers unbroken
      (item-20 dispatch tests green).
- [ ] `SELECT * FROM unidb_catalog.subscription_lag` returns correct
      `lag_events`/`lag_seconds` for a consumer deliberately held behind;
      `/stats` gauge matches.
- [ ] Crash harness unchanged (no format/recovery change); clippy/fmt; workspace.
- [ ] Guide §8 documents the contract + formats + lag detection.

## Depends on / builds on
- Item 20 (dispatcher, offsets, DLQ), item 26 (seq index, push, retention
  contract), item 18 (virtual catalog relations), item 21 (`/stats` surface).
- Lightly touches the queue/server surface — **coordinate with in-flight #27
  (vacuum) / #28 (replication) only on doc files;** its code (event payload +
  a catalog relation + `/stats` + server subscribe) is otherwise disjoint.
