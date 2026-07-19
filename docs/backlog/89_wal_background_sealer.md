**Type:** Performance
**Status:** ⏳ NOT STARTED

# Item 89 — WAL segment seal off the append path

## Problem

When a WAL segment fills, `write_framed_locked` seals it inline —
`rotate_segment` flushes + fsyncs the old segment **while holding the WAL
append mutex, in the middle of whatever statement happened to write the
overflowing record** ([wal.rs](../../src/wal.rs) `write_framed_locked`).

Measured (native `sample`, `main` @ item 71): the seal fsync inside
`begin_mini_txn → write_framed_locked → File::sync_all` = **~8% of a bulk
UPDATE statement** (amplified by macOS F_FULLFSYNC; smaller but real on
Linux). Effect is a mid-statement latency spike, not sustained throughput.

## Fix (PG walwriter pattern)

- Pre-open segment N+1 when segment N passes a high-water mark (e.g. 80%).
- On rotation, hand the sealed segment to a background sealer thread for its
  flush+fsync; appends continue into the pre-opened segment immediately.
- Durability contract unchanged: `sync_up_to(lsn)` (group commit) must block
  until every segment containing records ≤ lsn is sealed-and-synced — the
  sealer completes before or as part of the commit fsync, never after.
- Checkpoint/truncation waits for the sealer queue to drain (existing
  truncation logic already fsyncs; it just stops doing it inline).

## Expected gain

- Removes the periodic mid-statement stall → flatter p99 on bulk DML and
  per-row INSERT; minor throughput gain.

## Acceptance criteria

- p99 of per-row INSERT and bulk UPDATE improves in the Docker report's
  latency columns; throughput rows non-regressed.
- Crash harness: kill during rotation (existing point d — WAL truncation) plus
  a new kill-between-rotate-and-seal case; recovery must treat an unsealed
  previous segment correctly.
