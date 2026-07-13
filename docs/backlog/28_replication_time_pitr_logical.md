# Replication — time-based PITR + logical replication

**Type:** Milestone
**Status:** NOT STARTED

> Limitation from the architecture guide + Phase-6 follow-ups: "point-in-time
> restore is by log position, not wall-clock time; no logical replication yet."
> Phase 6 shipped physical replication (WAL shipping, read replicas, promote,
> by-LSN PITR). This adds the two operator-facing gaps. Touches `wal.rs` +
> `lib.rs` (backup/restore/replication) — **conflicts with item 26**; sequence
> them, do not run concurrently.

## Scope

- **R1 — Time-based PITR (MUST).** Map wall-clock → LSN so restore can target a
  timestamp, not just a raw log position. Record commit timestamps in the WAL
  (or a lightweight timeline index of (timestamp, LSN) checkpoints) and let
  `restore(..., target_time)` resolve to the highest LSN at or before it.
  AC: restore to a timestamp yields exactly the commits durable at/before that
  time; documented resolution/granularity; ties into `ops_runbook.md`.
- **R2 — Logical replication (SHOULD — milestone-sized).** A logical change
  stream (row-level, decoded from the WAL — reuse the M4 event-capture decode)
  that a subscriber applies as SQL against a *different* schema/subset, vs the
  physical page-level shipping Phase 6 has. This is the "replicate table X to a
  reporting DB" story.
  AC: a logical subscriber applies INSERT/UPDATE/DELETE for a selected table set
  and stays consistent across a primary restart; documented as at-least-once.

## Landmines

- Commit-timestamp placement must not break the WAL format for existing data
  (append a new record kind / use a side timeline index; avoid a
  `FORMAT_VERSION` bump if possible — §3/D9 sign-off if not).
- Time→LSN must handle clock skew / non-monotonic wall clock (use commit order
  as the tiebreak; document that time is advisory, LSN is authoritative).
- Logical decode overlaps the item-26 event path — if both are wanted, R2
  should CONSUME item 26's stream rather than re-decode the WAL twice.
- Crash harness + the existing replication/PITR crash points must stay green.

## Acceptance

- [ ] `restore(target_time)` proven against a known commit timeline (R1).
- [ ] Logical subscriber applies a table subset, survives primary restart (R2)
      — or R2 split into its own milestone if it grows past this scope.
- [ ] Crash harness green; no §3 decision reopened without recorded sign-off.
