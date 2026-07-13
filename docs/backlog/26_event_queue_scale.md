# Event queue at scale — sequence index + push (vs poll-per-subscriber)

**Type:** Improvement
**Status:** NOT STARTED

> Limitation surfaced by the architecture guide + Milestone 20's known-limits:
> "polling cost grows with table size (no sequence index yet); realtime is
> poll-per-subscriber rather than push." The M4 event queue + item-20 dispatcher
> work correctly but scan work grows with the enabled table, and each subscriber
> polls independently. This item makes consumption O(new events) and adds a push
> path so the dispatcher/studio don't spin.

## Scope

- **Q1 — Sequence index on the event stream (MUST).** Give the per-table event
  capture a durable secondary index keyed by sequence/offset so
  `poll_events_after(offset)` resolves via an index range, not a full scan.
  Reuse the durable `DiskBTree` machinery (P3.a/P3.b) — no new index type.
  AC: `poll_events_after` cost is O(events returned + log n), independent of the
  enabled table's size; a bench shows flat poll latency as the table grows
  10k→1M while new-event count is held constant.
- **Q2 — Push notification for subscribers (SHOULD).** A commit that appends
  events wakes waiting subscribers (condvar/watch channel) instead of every
  subscriber polling on a timer. The dispatcher (item 20) and the server SSE
  route consume the wake; poll remains the fallback/catch-up path.
  AC: an idle dispatcher does zero polling work until a commit wakes it; latency
  from commit→delivery drops vs the fixed poll interval (measured).
- **Q3 — Retention/horizon interaction (MUST if Q1 lands).** The sequence index
  must be vacuum/horizon-correct (consumed-and-past-all-offsets events are
  reclaimable; the index entry goes with them). Ties to the M4 all-consumers
  vacuum contract — do not let the index pin retention.

## Landmines

- The event index must be crash-recovered like every other durable index
  (redo-only `WAL_INDEX`); crash harness must stay green (add a point if the
  index can be torn mid-append).
- Push must not hold a latch/lock across the wake (P5.e lock-ordering rule).
- Touches `lib.rs` (event capture/poll) + `wal.rs` — **conflicts with item 28**;
  do not run those two lanes concurrently.

## Acceptance

- [ ] Poll latency flat as enabled-table size grows (Q1 bench).
- [ ] Idle subscribers do no work until a commit wakes them (Q2).
- [ ] Crash harness green; event index recovers; retention still bounded.
- [ ] Item-20 dispatcher + server SSE consume the push path with poll fallback.
