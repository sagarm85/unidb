# unidb Operations Runbook (Phase 6)

Operating a single **primary** + read **replicas**. Everything here is the
`unidb-server` (`--features server`) surface plus the embedded API. Config is by
environment variable (no config file in v1).

> Scale charter: strong single primary + read replicas (100s of GB). Distributed
> / sharded writes are parked.

---

## 1. Start / stop

```bash
UNIDB_DATA_DIR=/var/lib/unidb \
UNIDB_BIND_ADDR=0.0.0.0:8080 \
UNIDB_JWT_SECRET=<hmac-secret> \
  unidb-server            # HTTP

# HTTPS (P6.f): also set cert + key (PEM)
UNIDB_TLS_CERT=/etc/unidb/server.crt \
UNIDB_TLS_KEY=/etc/unidb/server.key \
  unidb-server            # HTTPS (rustls)
```

- `UNIDB_JWT_SECRET` is **required** (verify-only JWT; no default).
- Stop with `SIGINT`/`Ctrl-C` — graceful drain of in-flight requests (HTTP mode).
- The data directory holds `control`, `data.db`, `db.wal/` (segment dir),
  `slots.json`, `roles.json`, `audit.log`.

## 2. WAL & checkpoints (P6.a)

- The WAL is a directory of fixed-size **16 MiB segments** (`db.wal/seg-*.wal`);
  override with `UNIDB_WAL_SEGMENT_BYTES`.
- Auto-checkpoint is on by default (`UNIDB_AUTO_CHECKPOINT`,
  `UNIDB_CHECKPOINT_TIMEOUT_SECS`, `UNIDB_MAX_WAL_SIZE_BYTES`), or force one:
  `POST /checkpoint`.
- Truncation deletes whole consumed segments, held back by replication slots.

## 3. Backups + PITR (P6.d)

```rust
// Embedded API
let lsn = engine.base_backup(Path::new("/backups/base"))?; // checkpoints, copies
engine.archive_wal(Path::new("/backups/wal"))?;            // re-run periodically
// Restore to latest, or to a point-in-time LSN:
unidb::backup::restore(base, archive, dest, None)?;        // latest
unidb::backup::restore(base, archive, dest, Some(lsn))?;   // PITR by LSN
```

- **Drill:** `base_backup` → keep archiving the WAL → to restore, `restore(...)`
  into a fresh dir, then `Engine::open` it. PITR is **by LSN** in v1 (time-based
  PITR is a follow-up — commit timestamps aren't in the WAL yet).
- Take base backups regularly (roll-forward reconstructs pages present in the
  base; see the P6.c/P6.d fresh-page note).

## 4. Add a read replica + failover (P6.b / P6.c)

Primary:
```
POST /replication/slots        {"name":"replica1"[,"sync":true]}   # create slot
GET  /replication/slots                                            # list / lag
GET  /replication/stream?from_lsn=<n>                              # ship WAL
POST /replication/slots/replica1/advance  {"lsn":<n>}             # consumer ack
DELETE /replication/slots/replica1                                # drop slot
```

Replica (embedded): seed from a base backup, then stream + apply:
```rust
let mut replica = unidb::replication::Replica::init_from_base(dir, base)?;
loop {
    let stream = /* GET /replication/stream?from_lsn=replica.applied_lsn() */;
    replica.apply_stream(&stream, primary_control)?;   // control shipped alongside
    // POST .../advance {"lsn": replica.applied_lsn()}
}
let engine = replica.promote()?;   // FAILOVER → read-write primary
```

- **Sync option:** create the slot with `"sync": true`; the primary can call
  `Engine::wait_for_sync_replicas(lsn, timeout)` after a commit so a failover
  loses no acknowledged commit. Async (default) may lose the last un-shipped
  commits on failover — the documented tradeoff.
- **Stuck slot** pins the WAL and grows `db.wal/`: watch `GET /stats`
  `max_replication_lag` and drop abandoned slots.

## 5. Users / roles / access control (P6.e)

Bootstrap (open mode until the first user exists — create a SUPERUSER first):
```sql
CREATE USER admin SUPERUSER;        -- run auth DDL as a superuser thereafter
CREATE USER analyst;
CREATE ROLE reader;
GRANT SELECT ON accounts TO reader;
GRANT reader TO analyst;            -- role membership (transitive)
REVOKE reader FROM analyst;
```
- The JWT `sub` claim = the unidb username; a token with no `sub` is the implicit
  superuser (backward compatible). Auth DDL + schema DDL require superuser.
- All auth DDL and named-user access decisions are written to `audit.log`
  (one JSON line each).

## 6. Security (P6.f)

- **TLS:** set `UNIDB_TLS_CERT`/`UNIDB_TLS_KEY` (rustls).
- **Audit:** `audit.log` in the data dir — ship it to your SIEM.
- **Encryption at rest:** not provided by the engine (mmap page store; would
  change the D9 format — sign-off-gated follow-up). Use full-disk / volume
  encryption (LUKS/FileVault) underneath.

## 7. Observability (P6.g)

- `GET /metrics` — Prometheus (request rates/latencies, JWT verify time).
- `GET /stats` — `pg_stat_*`-style: `commits`, `aborts`, `checkpoints`,
  `active_transactions`, `wal_bytes`, `replication_slots`,
  `max_replication_lag`, `data_pages`, `recent_slow_queries`.
- **Slow-query log:** `Engine::set_slow_query_threshold(Duration)`; slower
  statements are `tracing::warn`ed (tagged with `txn_id`/`request_id`) and kept
  in the bounded ring shown by `/stats`.
- `EXPLAIN` / `EXPLAIN ANALYZE <query>` for plan diagnosis (P4.e).

## 8. Logs — JSON lines, correlation ids, shipping (item 22)

**Format (L1).** The server logs **JSON lines** to both stdout and the rolling
daily files under `UNIDB_LOG_DIR` (`unidb.log.YYYY-MM-DD`). Each line carries
`timestamp` (RFC3339 UTC), `level`, `target`, the `message`, and — inside a
request — the correlation ids. Set `UNIDB_LOG_FORMAT=text` for the older
human-readable console format when developing locally; leave it unset (JSON) in
production.

**Correlation ids (L2).** Every HTTP request is stamped with a `request_id`
(returned in the `x-request-id` response header). The SQL path additionally
tags `txn_id` (the xid). Both flow through to the app log, the slow-query log,
and `audit.log`, so **one request's lines join across all three by
`request_id`** (and audit/slow-query lines also carry `txn_id`). To trace a
request end-to-end: grep the `request_id` from `x-request-id` across the JSON
log and `audit.log`.

**Built-in tail (L3).** `GET /logs?level=&since=&until=&q=&cursor=&limit=`
(superuser-gated) is a bounded, cursor-paged reverse read of those JSON files —
the local/single-node "Logs tab" experience. It is **not** a log store: page
size is capped at 500 and each request examines at most 50 000 lines, so it
never OOMs or stalls on a multi-GB directory (it returns a `next_cursor` to
resume). Use it for quick incident triage on one node; use a real platform for
fleet-wide search and retention.

**Shipping to CloudWatch / Datadog / Loki (L5) — the production contract.** The
JSON files under `UNIDB_LOG_DIR` *are* the shipping interface; point your log
agent at them (they need no parser — one JSON object per line):

```bash
# CloudWatch agent (logs section)
{ "logs": { "logs_collected": { "files": { "collect_list": [
  { "file_path": "/var/lib/unidb/logs/unidb.log.*",
    "log_group_name": "unidb", "log_stream_name": "{hostname}" } ] } } } }

# Datadog agent (conf.d/unidb.d/conf.yaml)
logs:
  - type: file
    path: /var/lib/unidb/logs/unidb.log.*
    service: unidb
    source: unidb            # JSON is auto-parsed; request_id/txn_id become facets

# Promtail → Loki
scrape_configs:
  - job_name: unidb
    static_configs: [{ targets: [localhost], labels: { job: unidb, __path__: /var/lib/unidb/logs/unidb.log.* } }]
    pipeline_stages: [{ json: { expressions: { level: level, request_id: request_id, txn_id: txn_id } } }]
```

- **Rotation & retention** are `tracing-appender`'s daily files; unidb does not
  prune them — let the log agent (or logrotate) own deletion after shipping.
- Put `UNIDB_LOG_DIR` on its own volume if log volume competes with data I/O.
- Both stdout and the files are JSON, so a `docker logs`/journald → agent path
  works identically to tailing the files.
