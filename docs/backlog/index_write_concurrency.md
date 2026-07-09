# Index & heap write concurrency — latch-coupled B-tree + spread insertion targets

## Status as of 2026-07-10: NOT STARTED (backlog, Core-lane). Filed from the durable-FSM concurrency experiment.

Raising the **concurrent SQL-write ceiling** past the current group-commit-fsync
floor. The durable-FSM milestone (2026-07-10) confirmed concurrent SQL-insert
throughput is **flat ~1,250 commits/s at 8 writers, at every table size** (0 →
7,500 pages) — at parity with Postgres, bounded by the coalesced commit fsync,
**not** by anything the FSM change touched. To go *faster* per commit, the
serialization has to come out of the page/index layer. Two independent items,
both additive, neither reopens a §3 decision.

## Item A — Latch-coupled ("crabbing") B-tree descent

**Problem.** `DiskBTree` (`btree_index.rs`) has **no intra-tree concurrency
control**. Every `insert`/`remove` is one WAL mini-txn over the node pages it
touches, and correctness under concurrent writers currently rests on the coarse
outer serialization (the SQL path's catalog `RwLock` + `write_serial` +
group-commit ordering), not on node-level latching. So two writers updating the
same index can only proceed one-at-a-time through the coarse lock; the tree
itself cannot be descended concurrently. This is called out as future work in
`engine_design.md` §5.4 ("latch-coupled B-tree writes are future work") and in
the Phase 5 known-limitations ("finer-grained index concurrency is future work").

**Fix (standard).** Latch coupling / "crabbing": acquire the child's page latch
before releasing the parent's during descent, so concurrent descents interleave
safely; hold only the minimal set of latches across a split (or use B-link-tree
right-links so a split needs no parent latch — the leaves are *already*
right-linked here, `next_leaf`, which is half of a Lehman-Yao B-link tree). The
per-page S/X latch table already exists (`BufferPool::latch_exclusive`, P5.a) —
this is about *using* it for tree descent, not building new machinery.

**Invariant to preserve:** the P5.e latch-ordering rule (no page latch acquired
while the heap FSM `Mutex` is held) and D5 (WAL-before-page). Recovery is
unchanged — nodes are still full-page `WAL_INDEX` images, redo-only.

**Acceptance:** concurrent inserts into an indexed table scale past the current
flat ceiling on a workload where index contention (not fsync) is the bottleneck
— e.g. deferred-sync / batched-commit mode, where the fsync floor is removed and
the index becomes the serializer. Measure with a variant of `benches/
decompose.rs`'s B3 in one-fsync mode.

## Item B — Spread the heap insertion target across concurrent writers

**Problem.** `Heap::find_or_alloc_page` sends every writer to the **same** append
tail (`DiskBTree::max_entry`), so N concurrent inserters pile onto one page's
exclusive latch and fill/grow it in lockstep — the classic single-hot-page
relation-extension contention Postgres avoids by giving each backend a different
target block.

**Fix.** Hand concurrent writers **different** pages-with-space. The durable FSM
now makes this cheap: return several candidate pages (or a per-writer-thread
sticky target) instead of only the single max-entry tail, so inserts fan out
across pages and their latches. Bounded by the same "never over-report free
space" invariant (the insert retry loop already corrects a miss).

**Acceptance:** at high writer counts, insert throughput on an unindexed table in
one-fsync mode rises versus the single-tail baseline; no regression to the crash
harness (grow/orphan invariants unchanged — this only changes *which* existing/
new page a writer targets).

## Non-goals / notes

- The **group-commit fsync floor** is fundamental and already at Postgres parity;
  neither item beats it in the per-commit-durable regime. Their win shows in
  deferred-sync / batched-commit workloads and in reducing latch stalls. The
  project's real durable-throughput edge remains the **one-commit multi-model
  write** (row+vector+edge+event in a single fsync vs four systems), not a faster
  single-table commit rate.
- Batching the **SERIAL** counter persistence (`alloc_serial` persists per
  allocation under the catalog write lock — `docs/backlog/phase2_data_model.md`
  P2.d) is a related, smaller concurrency win filed there, not here.
- Stays within §1 (single-primary) and all §3 storage decisions.
