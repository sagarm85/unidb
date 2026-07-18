**Type:** Performance
**Status:** 📋 PLANNED 2026-07-18

# Item 67 — Async HNSW index build (decouple from commit critical path)

## Problem

W4/W0 at 10k rows is 21× and at 100k rows is 53× (with NodeCache regression) or ~40× (after
gate fix). The target from CLAUDE.md §1 is W4 ≈ W0 (one shared fsync ≈ all-or-nothing semantics).
The gap is entirely HNSW insert cost: W2−W1 ≈ 6-18ms/commit, while W0 ≈ 0.25-0.79ms.

**ef_construction reduction is ruled out (2026-07-18 probe):**
- ef=100 at 10k: recall@10 = 0.937 — FAILS ≥0.95 gate
- ef=50 at 10k: recall@10 = 0.926 — FAILS ≥0.95 gate
- ef=200 is required. No scalar parameter can fix the cost without breaking recall.

The only structural fix is to move HNSW index maintenance **off the commit critical path**.

## Approach

**Option A — Truly async (M2's original design):**
- SQL INSERT commits the row (WAL + heap + BTree) atomically in O(log n)
- HNSW insert is dispatched to a background worker queue after commit
- W4 ≈ W1 (no HNSW on critical path) → W4/W0 → ~1.1×
- Search returns approximate results; freshly inserted rows may not yet be in HNSW graph
- Consistency contract: NEAR query might miss the last ~N rows (async lag)
- This is the M2 design spec's "built asynchronously in a background worker"

**Option B — Deferred with durability fence:**
- SQL INSERT commits row + queues HNSW work unit to a durable WAL-backed queue
- HNSW worker processes queue and builds incrementally
- Stronger consistency: a `WAIT FOR HNSW` barrier makes query wait for outstanding work
- More complex recovery story: crash between row commit and HNSW apply needs replay

**Option C — Batched HNSW insert with write combining:**
- Instead of per-row insert, buffer N rows (e.g. 128) then insert as a batch
- Bulk-build amortizes ef_construction beam searches over N candidates
- Still synchronous but with lower amortized cost
- Drawback: INSERT waits until batch is full or timeout fires → latency spikes

## Recommended path

Option A first — the "NEAR may miss last N rows" contract is acceptable for our workload
(the thesis is about durable atomicity across models, not ANN real-time consistency). 
We already document that HNSW index build is synchronous in caveats; this would make it
explicit in the API contract.

## Expected impact

- W4/W0 at all table sizes → ~1.1-1.5× (only fsync + BTree + edge + event remain)
- Table 4 (thesis): unidb would beat the replaced stack (4× fsync → 1× fsync advantage fully realized)
- Table 3 INSERT: no change (per-row commit path unchanged)

## Dependencies

- Background worker infrastructure already exists (M2 async index worker for HNSW/FullText)
- HNSW index already has `insert_inner` function that can be called from worker context
- Recovery: crash after row commit but before HNSW apply → HNSW worker replays from WAL position

## Acceptance criteria

- W4/W0 < 3× at 1k/10k/100k rows
- Recall@10 ≥ 0.95 when queried after batch of 1000 inserts + HNSW flush
- Crash test: committed row survives; HNSW index converges after replay
- No regression in Table 3 CRUD numbers
