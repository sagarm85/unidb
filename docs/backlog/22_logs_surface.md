# Logs surface — JSON structured logs, correlation ids, bounded /logs tail

**Type:** Improvement
**Status:** NOT STARTED

> CloudWatch/Datadog-*like* local experience without rebuilding either: the
> engine already logs structured `tracing` events (D13) plus `audit.log` and a
> slow-query log. This item makes them queryable enough for a studio Logs tab,
> and ships production logs in a form any real log platform ingests directly.

## Scope

- **L1 — JSON-lines output.** Server logging switches to `tracing-subscriber`
  JSON formatting (rotation via existing `tracing-appender`). Fields: ts,
  level, target/module, message, and the L2 correlation ids.
- **L2 — Correlation ids.** Server middleware assigns a `request_id`; the SQL
  path tags `txn_id` (xid) into spans, so a log line ↔ slow-query entry ↔
  audit entry join on one id. Small, highest-leverage piece.
- **L3 — Bounded query endpoint.** `GET /logs?level=&since=&until=&q=&cursor=`
  — filtered tail over the rotated JSON files, cursor-paged, hard result cap;
  superuser-gated. Explicitly NOT a log database — filtered file reads only.
- **L4 — Studio Logs tab.** Level/time/module filters, live tail (SSE reusing
  item-20 framing), click-through from a slow query to its correlated lines.
- **L5 — Production guidance (docs).** `ops_runbook.md`: the JSON files are the
  shipping contract — point the CW/Datadog agent at them; the built-in tab is
  the local/single-node experience.

## Acceptance

- [ ] One request's lines are retrievable by `request_id` across app log,
      slow-query log, and audit log.
- [ ] `/logs` returns bounded, cursor-paged results; a multi-GB log directory
      cannot OOM or stall the server (cap + reverse-seek proof test).
- [ ] Log volume/overhead measured: ladder within noise with JSON logging on.
