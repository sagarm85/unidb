# Event-queue overhead at small table sizes: W4/W0 anomaly investigation

**Type:** Improvement
**Status:** NOT STARTED

## Observed symptom (`030325`, Docker Linux fsync, 2026-07-16)

| rows | W4/W0 | Δ event (W4−W3 ms) | W0 (ms) |
|-----:|:-----:|-------------------:|--------:|
| 1000 | **3.93×** | **+1.29 ms** | 0.49 |
| 10000 | 1.66× | +0.12 ms | 0.45 |

The event-queue step (W4−W3) costs **1.29 ms per commit at 1k rows** but only **0.12 ms at 10k rows** — a 10× difference for a step that should be O(1) per commit (append one WAL record, update the sequence counter). This anomaly inflates the multi-model commit multiplier to 3.93× at small table sizes, which is the demo range for the §1 "eliminated multi-system dual-write tax" thesis.

Baseline context: the `005004` report (x86, Docker, pre-items 44/45/47) showed Δ event = +6.36 ms at 1k rows. The `030325` result (1.29 ms) is a large improvement, but the 10× ratio between 1k and 10k is still unexplained and must be root-caused before any optimization attempt.

## This is an investigation item first

**Do not optimize before profiling.** The §0.6 rule applies: "prove, don't assume." The 10× difference could be caused by:

1. **Event-queue vacuum triggering at low queue depth.** The vacuum scheduler may use a threshold that fires more often (or more expensively) at 1k table rows than 10k, where the vacuum cost is amortized differently.
2. **Sequence index rebuild cost.** Item 26 added a sequence index (`26_event_queue_scale.md`). If the index is rebuilt or compacted as part of event capture at small sizes, this could explain the size-dependent cost.
3. **WAL fsync not being group-committed for the event-queue step.** If the event capture issues a separate `fsync()` outside the group-commit window, it pays the full fsync latency per commit. At 18 cores, the fsync latency (~0.45 ms from W0) dominates.
4. **HNSW vector index growth at 1k rows.** Table 2 shows Δ vector (W2−W1) = +0.02 ms at 1k — very small, so this is unlikely to be HNSW.
5. **Event catalog page contention.** If the event catalog lives on a hot page that is being flushed frequently at 1k rows (e.g. due to a low buffer pool utilisation at small sizes), WAL-before-page enforcement could stall the event write.

## Investigation plan

### Step 1 — Add `tracing::Span` instrumentation
In the W4 commit path (the event-queue capture step in `src/lib.rs` or wherever `EventQueue::push()` / `Dispatcher::dispatch()` is called):
```rust
let _span = tracing::debug_span!("event_queue_capture").entered();
```
Run `MM_SIZES=1000,10000 scripts/report.sh --native` with `RUST_LOG=unidb=debug` and compare span durations between the two sizes.

### Step 2 — Isolate vacuum vs WAL vs catalog
Disable autovacuum temporarily (`UNIDB_AUTOVACUUM_ENABLED=0` if such a flag exists, or disable in the bench) and re-run. If Δ event at 1k drops significantly, vacuum is the driver. If not, the cost is in WAL/catalog.

### Step 3 — Measure group-commit coalescing
Add a counter for how many commits were coalesced per fsync in the event-queue step. If group-commit fires a separate `fsync()` for the event record at 1k rows (because there is only one writer and no other commit to coalesce with), that would explain the full fsync cost appearing in Δ event.

### Step 4 — Derive the fix from the evidence
Once the root cause is identified, file the appropriate fix:
- If vacuum: a size-aware vacuum throttle (similar to the small-candidate guard in item 46).
- If WAL non-group-commit: ensure the event-queue WAL write is always included in the current group-commit window.
- If catalog contention: cache the event catalog page reference to avoid repeated lookups.

## Acceptance criteria

- Root cause identified and documented in this file (inline correction note per §0.6 rule 6).
- After the fix: Δ event at 1k rows drops to ≤ 0.20 ms (from 1.29 ms), and W4/W0 at 1k rows drops to ≤ 1.50×.
- W4/W0 at 10k rows remains ≤ 1.70× (no regression).
- `PROGRESS.md` records before/after W4/W0 at both sizes.

## Depends on / builds on

- `src/lib.rs` — Engine commit path, group-commit logic.
- Item 20 (`20_events_realtime_dispatcher.md`) — SHIPPED. The event-queue and dispatcher are the W4 step being investigated.
- Item 26 (`26_event_queue_scale.md`) — SHIPPED. Sequence index added here; a likely candidate for the overhead.
- Item 9 (`autovacuum.md`) — SHIPPED. Autovacuum scheduler is a candidate for the small-table cost.

## Parallel note

Investigation only — no code changes until root cause is confirmed. Can run in parallel with items 51–54 since it does not modify any shared code paths.
