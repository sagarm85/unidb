# 11. Server, Replication & Operations

**Modules:** `server/*` (feature-gated), `replication/*`, `backup/*`, `authz/*`,
`audit/*`. The embedded crate is primary; everything here is optional and the
default build carries **zero async dependencies**.

---

## 11.1 Server architecture

```mermaid
%%{init: {"theme":"base","themeVariables":{"primaryColor":"#eef3fc","primaryTextColor":"#1f2a37","primaryBorderColor":"#3b6fd4","lineColor":"#7f8c9b","secondaryColor":"#eef7f1","secondaryBorderColor":"#2f9e5f","tertiaryColor":"#fdf5e8","tertiaryBorderColor":"#d98b1f","fontFamily":"Segoe UI, Arial, sans-serif","fontSize":"14px","clusterBkg":"#f7fafc","clusterBorder":"#dce4ec"}}}%%
flowchart LR
    C[Clients] -->|HTTPS optional rustls| AX["axum router"]
    AX --> JWT["JWT layer (HS256, verify-only)<br/>every route except /metrics"]
    JWT --> H["handlers: begin → execute → commit/abort"]
    H --> EH["EngineHandle = Arc&lt;Engine&gt;<br/>spawn_blocking per call"]
    EH --> ENG["Engine (Send + Sync)<br/>group commit coalesces fsyncs"]
    H --> RH["ReadHandle fast path<br/>(reads: no begin/commit)"]
    ENG --> AZ["authz roles.json"]
    ENG --> AU["audit.log (JSON lines)"]
```

No engine mutex behind the handlers: since P5.e the `Engine` is `Send + Sync`,
handlers run blocking calls on tokio's blocking pool, and concurrency is
coordinated by the engine's own latches/locks/WAL — more concurrent committers
means *fewer* collective fsyncs (group commit). The HTTP layer adds only ~6 %
over a direct `Engine::insert` call.

### Routes

| Area | Routes |
|---|---|
| SQL / Cypher | `POST /sql`, `POST /cypher`, `POST /batch-sql` (item 99) |
| Raw rows | `POST /rows`, `GET/PUT/DELETE /rows/{page_id}/{slot}` |
| Bulk load | `POST /tables/{name}/bulk` — NDJSON (item 32) |
| Graph | `POST /edges`, `DELETE /edges/{page_id}/{slot}`, `GET /edges/from/{from_id}` |
| Indexes | `POST /indexes`, `GET /indexes/{table}/{column}/status` |
| Tables / events | `GET /tables`, `POST /tables/{t}/events`, `GET/DELETE /tables/{t}/events` (item 33), `GET /events/subscribe` (SSE), `POST /events/ack`, `GET /events/head` |
| Storage | `GET/POST /storage/*` — 7 routes (item 31, StorageApi trait) |
| Config | `PUT /config/slow_query_threshold_ms` (item 34) |
| Ops | `POST /checkpoint`, `GET /stats`, `GET /stats/history` (300-point ring, item 34), `GET /metrics` (public) |
| Replication | `POST/GET /replication/slots`, `DELETE /replication/slots/{name}`, `POST /replication/slots/{name}/advance`, `GET /replication/stream` |

`POST /sql` dispatch order: auth DDL → superuser-gated `execute_sql_as`;
privilege pre-check (`authorize_sql`); parameterized statements bind `$n` as
data (injection-safe); read-only SELECT → the concurrent read path; everything
else → writer path with commit-or-abort wrapping.

### POST /batch-sql (item 99)

`POST /batch-sql` accepts a JSON array of up to 256 SQL statements and executes
them in a single HTTP request, each in its own transaction:

```mermaid
%%{init: {"theme":"base","themeVariables":{"primaryColor":"#eef3fc","primaryTextColor":"#1f2a37","primaryBorderColor":"#3b6fd4","lineColor":"#7f8c9b","secondaryColor":"#eef7f1","secondaryBorderColor":"#2f9e5f","tertiaryColor":"#fdf5e8","tertiaryBorderColor":"#d98b1f","fontFamily":"Segoe UI, Arial, sans-serif","fontSize":"14px","clusterBkg":"#f7fafc","clusterBorder":"#dce4ec"}}}%%
sequenceDiagram
    participant C as Client
    participant H as batch_sql handler
    participant E as Engine
    C->>H: POST /batch-sql [{sql:"INSERT..."},{sql:"SELECT..."}]
    loop per statement (up to 256)
        H->>E: execute_sql(stmt)
        E-->>H: result rows or error
    end
    H-->>C: [{rows:[...]},{rows:[...]}]
```

Each statement is independent — a failure in statement N does not roll back
statement N-1. This is the primary API for `compare.py`-style workloads;
measured reduction: 109 ms → ~15 ms by eliminating per-statement HTTP
round-trips.

### POST /tables/{name}/bulk — NDJSON bulk load (item 32)

Accepts newline-delimited JSON rows (NDJSON). Each line is one JSON object
matching the table schema. The handler streams rows in chunks (default 1,000
per chunk) through `exec_insert`, using `InsertAccum` for WAL batching.

**Measured throughput: 60,000–87,000 rows/sec** (vs ~640/s per-row HTTP).
Body buffered up to 512 MiB. JWT grant enforcement: `check_table_grant(user,
table, Insert)` runs before reading any body bytes.

### CDC management API (item 33)

- `GET /tables/{name}/events` — returns CDC status (enabled/disabled, offset
  of last event, approximate lag).
- `DELETE /tables/{name}/events` — disables CDC for the table. Idempotent:
  returns 200 even if already disabled.
- `GET /events/head` — returns the current head sequence number across all
  event-enabled tables.

### Slow query log (item 34)

`UNIDB_SLOW_QUERY_MS` env var (or `PUT /config/slow_query_threshold_ms`) sets
a threshold. Queries exceeding it are emitted as `tracing::warn` events and
stored in a 300-point ring buffer accessible via `GET /stats/history`. The
ring holds `(timestamp, sql_fingerprint, duration_ms)` tuples.

### SQL authz DDL (item 24 Z1+Z3+Z5)

- **Z1 — SQL DDL for roles/grants/policies:**
  `CREATE ROLE <name> [SUPERUSER]`, `GRANT SELECT|INSERT|UPDATE|DELETE|ALL ON
  <table> TO <role>`, `CREATE POLICY <name> ON <table> FOR <op> USING (...)`.
  All persisted in the catalog.
- **Z3 — JWT grant enforcement on every relevant route:** `authorize_sql`
  called before `execute_sql`; `check_table_grant` called in bulk handler.
  `apply_rls` applied at both `execute_sql_inner` call sites.
- **Z5 — Catalog virtual relations:** `SELECT * FROM unidb_catalog.roles`,
  `unidb_catalog.grants`, `unidb_catalog.policies`.

## 11.2 Security

- **JWT** — stateless verify-only HS256 (`UNIDB_JWT_SECRET`); the `sub` claim is
  the unidb username; a token without `sub` is the implicit superuser
  (backward compatible). `/metrics` is deliberately outside the JWT wall
  (Prometheus scrapers carry no bearer token — firewall it at the network
  layer).
- **Roles/GRANT (`roles.json`)** — users (optional SUPERUSER), roles, transitive
  membership, per-table SELECT/INSERT/UPDATE/DELETE/ALL. Auth DDL (`CREATE
  USER/ROLE`, `GRANT/REVOKE …`) has its own small parser. Atomic
  write-tmp+rename persistence. **Bootstrap mode:** until the first
  `CREATE USER … SUPERUSER`, everyone is an effective superuser — pre-P6.e
  behavior preserved.
- **Audit (`audit.log`)** — append-only JSON lines: all auth DDL, plus every
  named-user access decision (allowed *and* denied). The implicit embedded
  superuser's data ops are not audited (trusted operator); fsync-free append off
  the hot path.
- **TLS** — native rustls (aws-lc-rs) when `UNIDB_TLS_CERT`/`UNIDB_TLS_KEY` are
  set; no reverse-proxy assumption. Encryption-at-rest is deferred (D9
  sign-off-gated).
- Error contract: typed `DbError → (HTTP status, code)` — e.g. 409
  `WRITE_CONFLICT`/`SERIALIZATION_FAILURE`/`DEADLOCK`, 403 `PERMISSION_DENIED`,
  **503 `DURABILITY_FAILURE`** (fsyncgate — session poisoned; restart is the
  remedy).

## 11.3 Replication & HA (Phase 6)

```mermaid
%%{init: {"theme":"base","themeVariables":{"primaryColor":"#eef3fc","primaryTextColor":"#1f2a37","primaryBorderColor":"#3b6fd4","lineColor":"#7f8c9b","secondaryColor":"#eef7f1","secondaryBorderColor":"#2f9e5f","tertiaryColor":"#fdf5e8","tertiaryBorderColor":"#d98b1f","fontFamily":"Segoe UI, Arial, sans-serif","fontSize":"14px","clusterBkg":"#f7fafc","clusterBorder":"#dce4ec"}}}%%
flowchart LR
    subgraph Primary
        W["Segmented WAL db.wal/"] --> SR["SlotRegistry slots.json<br/>restart_lsn = truncation floor"]
        W -->|"ship_wal — CAPPED at durable LSN"| ST["/replication/stream"]
    end
    ST --> R1["Replica<br/>base snapshot + incremental apply<br/>(normal recovery redo, LSN-gated ⇒ idempotent)"]
    R1 -->|"promote()"| P2["New primary (RW Engine)"]
    W --> BB["base_backup + archive_wal"] --> PITR["restore(…, target_lsn) — PITR by LSN"]
```

The load-bearing correctness rules:

- **Never ship past the durable frontier.** Records physically in the WAL file
  but not yet fsynced must not reach a replica — otherwise a promoted replica
  could hold commits the crashed primary never made durable (divergence). Proven
  by a dedicated test: after a primary crash drops its unsynced tail, the
  replica is still a strict prefix.
- **Ship at commit boundaries** — a replica only ever applies complete committed
  transactions and never needs undo.
- **Slots hold the truncation floor**: checkpoint truncates to
  `min(checkpoint_lsn, min slot restart_lsn)`; `advance` is monotonic (a stale
  confirmation never rewinds retention). A stuck slot pins the WAL — the classic
  footgun, surfaced as `/stats max_replication_lag`.
- **Sync replication option**: a `{"sync": true}` slot plus
  `wait_for_sync_replicas(lsn, timeout)` after commit ⇒ failover loses no
  acknowledged commit; the async default may lose the last un-shipped commits
  (documented trade-off).
- **Backup/PITR**: `base_backup` (consistent, post-checkpoint, directly
  openable) + `archive_wal` (sealed segments are append-only ⇒ plain copies) +
  `restore(base, archive, dest, target_lsn)` — rebuilds the WAL up to the target
  and runs ordinary crash recovery. PITR is **by LSN**; time-based PITR needs
  commit timestamps in the WAL (filed).

**Documented limitation (both replica and PITR):** freshly *allocated* pages are
not FPI-covered, so incremental redo can only reconstruct pages present in the
base — steady-state updates are fine, but a workload that keeps allocating new
pages needs periodic re-basing. The full fix (FPI-covering fresh pages) is a
tracked follow-up.

## 11.4 Observability

- **`/metrics`** (Prometheus): HTTP metrics + engine gauges refreshed per scrape
  — `unidb_autovacuum_runs_total`, `unidb_dead_tuple_estimate`,
  `unidb_live_tuple_estimate`, `unidb_autovacuum_last_run_epoch_secs`,
  `unidb_jwt_verify_seconds` (histogram), `unidb_sse_poll_cycles_total`,
  `unidb_sse_events_delivered_total`.
- **`GET /stats`** (pg_stat-style): commits, aborts, checkpoints,
  active_transactions, wal_bytes, replication_slots, max_replication_lag,
  data_pages, recent_slow_queries.
- **Slow-query log**: `set_slow_query_threshold` → `tracing::warn` + a bounded
  ring exposed via `/stats`.
- **Structured logging (D13)** from day one for WAL writes, checkpoints, and
  recovery replay — "did recovery work" is answerable from logs.
- `docs/ops_runbook.md` covers deployment, replication setup, backup schedules,
  and failure drills.

## 11.5 Border cases

| Case | Handling |
|---|---|
| Replica applies the same stream twice | LSN-gated redo — idempotent (tested) |
| Primary crashes mid-ship | durable-frontier cap ⇒ replica remains a prefix |
| Stale slot confirmation | monotonic advance — retention never rewinds |
| JWT with no `sub` | implicit superuser (compatibility); first CREATE USER closes bootstrap mode |
| Denied access | audited with decision + reason |
| fsync failure under load | 503 `DURABILITY_FAILURE`; session poisoned by design |
| SSE subscriber backlog | poll-loop model — no unbounded server buffering; acks only via REST |
