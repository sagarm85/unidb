# Item 28 — Design Decisions: Time-based PITR (R1) + Logical Replication (R2)

> Committed before code. Last updated 2026-07-13.

---

## R1 — Time-based PITR

### Decision: side timeline index, NOT a WAL record-format change

**What we chose:** A `timeline.bin` file (16-byte binary records, each
`u64 ts_micros || u64 lsn`, little-endian) appended atomically in
`Engine::commit` after WAL sync. `backup::restore_to_time(target_ts_micros)`
loads the marks, finds the highest LSN where `mark.ts ≤ target`, and calls the
existing `backup::restore(..., Some(lsn))`.

**Why not stamp `WAL_TXN_COMMIT` with a timestamp:** The WAL record layout is
governed by §3/D9. Adding a field would require a `FORMAT_VERSION` bump and
explicit human sign-off. The side-file approach avoids all of that: timeline
marks are advisory metadata outside the page/WAL hot path; they are not required
for correctness (crash recovery is LSN-based and unchanged); and they are never
replayed by the redo engine. The WAL format is untouched.

**Precision / granularity:** One mark per committed user transaction → per-commit
resolution. Operators can restore to any moment that falls between two commits;
the result is the state as of the commit immediately at or before the target
time. This is exactly the documented "checkpoint granularity" PITR that Postgres
physical PITR provides.

**Clock skew:** `resolve()` scans all marks and picks `max(lsn)` where
`mark.ts ≤ target_ts`. Since LSN is strictly monotonic (the WAL ordering), this
handles non-monotonic wall clocks without ambiguity. Time is advisory; LSN is
authoritative.

**Timeline file lifecycle:**
- Created in the data directory on `Engine::open`.
- Included in a base backup (extended `base_backup_dir`).
- Archived alongside WAL segments by `Engine::archive_wal`.
- Restored alongside WAL segments into the destination directory.
- A torn last record (crash mid-append) is silently skipped (file size mod 16).

**Crash point P31:** A torn timeline mark does not affect database consistency
(the WAL is the source of truth); it reduces PITR resolution to the previous
valid mark. Tested in `tests/crash/main.rs` as a non-fatal advisory degradation.

---

## R2 — Logical Replication

### Decision: app-layer crate `unidb-logical`, consuming item-26 events

**What we chose:** A new workspace member `unidb-logical` that wraps
`unidb-dispatch`'s `Dispatcher` with a `LogicalApplySink`. The sink translates
each `Event` (table, op, payload JSON) into INSERT / DELETE / upsert SQL
executed on a target `Engine`. At-least-once delivery, offset-durable, resumes
across primary restarts — all inherited from the existing M4 + item-20 contract.

**Why not re-decode the WAL:** Item 26 already ship a WAL-derived event stream
captured synchronously into `__events__`. Re-decoding the WAL for logical
replication would be a parallel mechanism with no additional benefit: the event
table has `table_name`, `op`, and a full JSON row image, which is exactly what
logical replication needs. Using it avoids double-tracking and keeps the WAL
format unextended.

**Event payload sufficiency check:**
- INSERT: payload = new row image → sufficient for `INSERT INTO target (...) VALUES (...)`.
- DELETE: payload = old row image (captured before `heap.delete`) → sufficient if
  a key column is declared.
- UPDATE: payload = **new** row image only (old key not present) →
  reconstruction via `DELETE WHERE key = new_key + INSERT new_row`. This is
  correct when the key column is immutable (standard practice). If the key
  itself is updated, the logical subscriber may not find the old row.
  **Filed as item-26 follow-up:** capturing `(old_key, new_row)` in the update
  event payload would make key-update replication lossless without a WAL change.

**Key-column requirement:** `TableSpec { table, key_column }` must be provided
per replicated table for UPDATE/DELETE to work. INSERTs are applied without a key.
If no matching table spec is found for an event, the event is skipped (not an error).

**Schema requirement:** The target schema must be pre-created before replication
starts. The `LogicalReplicator` does no DDL. This is the standard logical
replication model (Postgres also requires the target schema to exist).

**Delivery semantics:** At-least-once (inherited from item-20 Dispatcher).
Consumers must deduplicate on event `seq` if exact-once is required.

**Topology:** One primary `Arc<Engine>` (event source) + one target `Arc<Engine>`
(write destination). Multi-target fanout is handled by running multiple
`LogicalReplicator` instances with different consumer names and targets.

---

## Honest limitations (documented, not silent)

| Limitation | Status |
|---|---|
| PITR resolution = per-commit mark granularity | Documented in ops_runbook §9 |
| Clock skew handled by LSN tiebreak, time is advisory | Documented |
| UPDATE events carry new row only (no old key) | Item-26 follow-up filed |
| Target schema must be pre-created | Documented in engine_access_guide |
| No schema-mapping DSL (column rename, type cast) | Out of R2 scope, follow-up |
| Multi-primary / conflict resolution | Out of R2 scope (single-primary only) |
