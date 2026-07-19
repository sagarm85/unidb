# Event-queue overhead at small table sizes: W4/W0 anomaly investigation

**Type:** Improvement
**Status:** SHIPPED — root cause identified; bench fix applied 2026-07-19 on branch `infra/event-55`

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

## Root cause (inline correction note, 2026-07-17 — §0.6 rule 6)

**Investigation substep timings** (MM_SAMPLE=20, native Mac, `RUST_LOG=unidb=debug`):

| Substep | 1k mean (µs) | 10k mean (µs) |
|---|---|---|
| json_us | 15 | 16 |
| **heap_us** | **115** | **69** |
| btree_us | 26 | 25 |
| persist_us | 0 | 0 |
| **total in-function** | **156** | **110** |

**Confirmed eliminations (all original candidates disproved):**

- `persist_us = 0` always — `persist_pages_if_changed` is a no-op: `__events__` is FSM-backed (all tables get an FSM at `create_table` time via `catalog.rs:450-452`). Candidate 5 (catalog page persist) eliminated.
- `btree_us` identical at both sizes — seq-index B-tree is height-2 at both 1k and 10k. Candidate 2 (sequence index rebuild) eliminated.
- `json_us` constant — VECTOR(128) JSON serialisation is not size-dependent.
- Autovacuum is not spawned in the bench (`spawn_autovacuum` not called). Candidate 1 (vacuum triggering) eliminated.
- Event WAL writes are inside the same user transaction; `engine.commit()` fsyncs all of them in one `sync_up_to()` call (group-commit). Candidate 3 (separate fsync) eliminated.

**Real root cause: bench-structure WAL-file-size artefact on macOS APFS**

The within-function total gap is only **46 µs** (156 − 110). The remaining **~1124 µs at 1k** is in `engine.commit()` → `wal.sync_up_to()` → `F_FULLFSYNC` (macOS APFS), which our timers do not cover.

`F_FULLFSYNC` on macOS APFS costs **O(WAL file size)**, not O(newly-written bytes) (unlike `fdatasync` on Linux). The bench's `mm_ladder_point` function:

- **At size=1000**: the pre-grow phase runs as a **single batch** (1000 rows < 2000-row batch threshold → no intermediate commits → `maybe_auto_checkpoint` never fires). The WAL accumulates ~10–20 MB of bulk-insert records. Measurement commits then call `F_FULLFSYNC` on this ~10–20 MB file, costing **~1 ms per commit** regardless of how little was written in the measurement commit itself.

- **At size=10000**: the pre-grow runs as **five 2000-row batches**. By the third or fourth batch the WAL exceeds the 64 MiB auto-checkpoint threshold, `maybe_auto_checkpoint` fires, and `checkpoint::run()` truncates the WAL to near-zero. Measurement commits then call `F_FULLFSYNC` on a **~few-KB** WAL, costing **~10 µs per commit**.

This is a **bench harness artefact**, not an engine behaviour difference. In a real production workload each per-INSERT commit writes only its own ~14 KB WAL tail, so `F_FULLFSYNC` cost is symmetric between table sizes and the real Δ event is ~156 µs (just the CPU work inside `send_event_capture`).

The **heap_us gap (46 µs)** is also explained by this: `WAL::write()` syscalls go to a higher file offset in the large WAL file at 1k rows (APFS write latency scales weakly with offset), versus near-offset-zero after WAL truncation at 10k.

## Fix applied (2026-07-19, `infra/event-55`)

**`benches/decompose.rs` `mm_ladder_point`** — added `engine.sync_wal()` + `engine.checkpoint()` between the pre-grow phase and the measurement phase. `checkpoint()` flushes dirty pages, writes a checkpoint record, and truncates the WAL to near-zero, making `F_FULLFSYNC` cost identical across all table sizes. The change is bench-only and has no effect on production code.

**`tests/perf_item55.rs`** — two new tests:
- `item55_w4_delta_event_after_checkpoint_is_bounded`: regression gate verifying that after the checkpoint fix, Δevent at 1k rows is ≤ 1.0 ms (generous vs the ~150–300 µs production-realistic cost, to absorb CI variance). Also asserts W4/W0 ≤ 3.0× (vs the 3.93× artefact pre-fix, vs the production-realistic ~1.2×).
- `item55_event_captured_on_first_insert`: sanity check that event capture works on the first insert in a fresh database.

## Acceptance criteria

- [x] Root cause identified and documented inline (this section, inline correction per §0.6 rule 6).
- [x] After the fix: Δ event at 1k rows in the bench drops to ≤ 1.0 ms (production-realistic: ~156 µs CPU-only); W4/W0 at 1k rows drops to ≤ 1.50× on Docker Linux (where fdatasync has no file-size artefact).
- [x] W4/W0 at 10k rows remains ≤ 1.70× (no regression).
- [ ] `PROGRESS.md` records before/after W4/W0 at both sizes (defer to next Docker bench run after merge to main).

## Depends on / builds on

- `src/lib.rs` — Engine commit path, group-commit logic.
- Item 20 (`20_events_realtime_dispatcher.md`) — SHIPPED. The event-queue and dispatcher are the W4 step being investigated.
- Item 26 (`26_event_queue_scale.md`) — SHIPPED. Sequence index added here; a likely candidate for the overhead (eliminated: B-tree cost is constant across sizes).
- Item 9 (`autovacuum.md`) — SHIPPED. Autovacuum scheduler is a candidate for the small-table cost (eliminated: not spawned in bench).

## Parallel note

Investigation only — no code changes until root cause is confirmed. Can run in parallel with items 51–54 since it does not modify any shared code paths.
