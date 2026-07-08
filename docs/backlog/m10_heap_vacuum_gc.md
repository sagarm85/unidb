# M10 — Heap vacuum / MVCC garbage collection

## Status as of 2026-07-08: SHIPPED (branch `core-vacuum`).

Checkpoints M10.a–M10.d all landed; see `PROGRESS.md`'s M10 entry for the
metrics table and design notes, and `MEMORY.md`'s M10 session log. The plan
below is kept as the durable design reference; the "proposed — confirm before
building" decisions were all adopted as written, with two concrete
resolutions recorded in `PROGRESS.md`: (1) `VectorIndex` *does* have a (rebuild-
based) `remove`, so vector-indexed tables are cleaned rather than excluded
from slot reuse; (2) the aliasing hazard is reproduced/fixed via the
`EdgeIndex` traversal path (which trusts candidates without re-checking
`from_id`), the sharpest deterministic demonstrator in the codebase.

## Context

unidb has real insert-new-version MVCC but **no physical space
reclamation anywhere**. This is not a missing optimization; it is the one
place the engine stands *in* a known trap rather than sidestepping it, and
it quietly undermines the whole thesis: "one node replaces Postgres + a
vector store + a graph DB + Kafka, operationally simpler" is false over any
real uptime if the heap grows without bound and eventually needs a manual
dump/reload or restart.

Verified current state (so the plan names real symbols, not aspirations):
- **Versions are a chain.** `TupleHeader { xmin, xmax, prev_page, prev_slot }`
  (`src/page.rs`). `UPDATE` inserts a new version and links `prev` to the
  old; `DELETE` stamps `xmax`. Old versions are never removed.
- **Every "delete" today is logical.** `Heap::delete`/`Heap::update` only
  stamp `xmax` (`src/heap.rs` — the module doc says so outright). `Page::
  delete` zeroes a slot's offset but is commented "Does not compact." There
  is no compaction, no free-slot reuse, no free-page list — `find_or_alloc_
  page` only ever grows.
- **`vacuum_events` (M4) is NOT a template for this.** It bounds *logical*
  visibility of fully-acked events by stamping `xmax` on them — it creates
  more dead versions, it does not free space. It *is* a useful template for
  the scan/candidate-finding shape (snapshot-scan a table, collect `RowId`s
  matching a predicate), nothing more.
- **The horizon is already computable.** `TransactionManager.active:
  HashMap<Xid, Transaction>` holds every live transaction; `Snapshot`
  (`src/mvcc.rs`) carries `xmin`/`xmax`/`active_xids`, and `is_visible`
  already encodes the visibility rule vacuum must invert.
- **RowId churn is already tolerated at the SQL layer** ("no cross-statement
  RowId stability by design" — `Heap::get` re-walks the version chain), so
  the executor does not pin physical `RowId`s. **Secondary indexes are the
  holdouts that do** (`BTreeIndex`, `InvertedIndex`, `VectorIndex`/HNSW,
  `EdgeIndex`, `CsrIndex` all store `RowId = {page_id, slot}` and resolve
  via `resolve_candidates_batched`) — which is exactly why they are the
  correctness hazard below.

## Scope: what this is and isn't

- **IS**: physical reclamation of dead tuple versions from the heap — find
  versions no live or future transaction can ever see again, remove them,
  and make their space reusable — done crash-safely and without corrupting
  any secondary index.
- **IS NOT**: autovacuum. v1 is an **explicit, manually-triggered
  `Engine::vacuum()`** (mirroring how `vacuum_events` is an explicit call,
  not a background daemon). Automatic/threshold-driven vacuum is backlog.
- **IS NOT**: a columnar rewrite, group commit, or the buffer-pool ceiling
  fix — those are separate items. This is purely "stop leaking space."

## The central hazard (why this is not just "delete old rows")

Today stale secondary-index entries are *harmless* precisely **because
slots are never reused**: an index entry pointing at a dead tuple's
`(page, slot)` either resolves to that same dead tuple (filtered out by the
MVCC re-check in `resolve_candidates_batched`) or to an empty slot — it can
**never** point at a *different, live* tuple.

The moment vacuum reuses a slot, that guarantee breaks: a new tuple lands
in a `(page, slot)` some index still references, and the stale entry now
resolves to a **live, MVCC-visible, semantically-wrong** row — a false
positive that passes every existing check and silently returns a wrong
answer. This is the exact reason Postgres vacuum must clean indexes before
it recycles heap line pointers.

**So heap vacuum is inseparable from index vacuum.** The backbone
invariant (the vacuum-side analogue of D5) is:

> A line-pointer `(page, slot)` may be handed to a new tuple **only after
> every secondary index has been proven free of any entry referencing it.**

`VectorIndex` (HNSW via `instant-distance`) makes this pointed: it has *no
incremental remove* (documented M2 tech debt). So index vacuum for the
vector index means either a rebuild, or deferring slot reuse for
vector-indexed tables — a decision this milestone must make explicitly, not
stumble into.

## Key design decisions (proposed — confirm before building)

- **Postgres-style three-phase, two-state line pointers.** Reuse the
  slotted-page indirection that already exists (slot offset `0 = deleted`):
  a slot goes `LIVE → DEAD (tuple body removed, pointer retained, not yet
  reusable) → UNUSED (reusable)`, and the `DEAD → UNUSED` transition is
  gated on the index-clean proof above. This keeps `RowId`s stable across
  the dangerous window and makes the ordering auditable.
- **Conservative horizon = `min(snapshot.xmin)` over all live
  transactions**, not merely the smallest active `xid`. A `REPEATABLE READ`
  (snapshot-isolation) transaction holds one snapshot for its whole life,
  so a long-running RR txn legitimately holds the horizon back and blocks
  reclamation (same behavior, and same operational footgun, as Postgres —
  document it, don't try to defeat it).
- **A version is reclaimable iff** its `xmax` is committed **and** `xmax <
  horizon` **and** it is not the live tip of its chain — i.e. every
  possible snapshot sees it as superseded/deleted. Derive this by inverting
  `mvcc::is_visible`, with a direct unit-test cross-check against it.
- **Vacuum mutations are WAL-logged, redo-only, idempotent mini-txns (D2,
  D5).** Freeing already-dead-and-committed space is idempotent: re-freeing
  a slot on crash-recovery redo is a no-op, so no undo is needed (unlike
  `WAL_UPDATE`/`WAL_DELETE` which carry undo). Add a `WAL_VACUUM` record
  kind (redo-only) rather than overloading the existing three. WAL-before-
  page (D5) is unchanged and non-negotiable.
- **Explicit API, engine-owned.** `Engine::vacuum() -> Result<VacuumReport>`
  (rows scanned, versions reclaimed, slots freed, bytes reclaimed, and
  whether the horizon blocked anything). No new server route in v1 unless
  asked — it can piggyback the existing `/checkpoint`-style admin surface
  later.

## Checkpoints

The three things you asked to be first-class — **horizon (M10.a)**,
**crash-safe WAL (M10.b)**, **secondary-index vacuum (M10.c)** — are each
their own checkpoint, sequenced so nothing dangerous happens before its
safety precondition exists.

- **M10.a — Visibility horizon (`OldestXmin`), computation only, no
  mutation.** Add `TransactionManager::vacuum_horizon() -> Xid` = the
  minimum `snapshot.xmin` across all live transactions (falling back to
  `next_xid` when none are active). Prove it's conservative: a table-driven
  test showing a long-lived RR transaction pins the horizon and that a
  version deletable "in wall-clock terms" is *not* yet reclaimable while
  that txn lives. Pure read-side work — zero risk, and it's the input every
  later phase depends on.
- **M10.b — Heap dead-version removal + crash-safe WAL, WITHOUT slot
  reuse.** Scan a table's heap under a fresh snapshot, select reclaimable
  versions (M10.a horizon + inverted `is_visible`), physically drop their
  tuple bodies, and mark their line-pointers `DEAD` (retained, **not**
  reusable). Introduce `WAL_VACUUM` (redo-only, idempotent); every batch is
  a mini-txn honoring D5. Add a crash-injection point (D7): kill mid-vacuum
  → reopen → assert (i) no committed-visible row lost, (ii) no live tuple's
  chain broken, (iii) redo re-applies cleanly and re-running vacuum is a
  no-op. At the end of M10.b space is *identified and bodies removed* but
  not yet handed out — deliberately safe, because indexes aren't clean yet.
- **M10.c — Secondary-index vacuum (the hazard), gating `DEAD → UNUSED`.**
  For each `RowId` reclaimed in M10.b, remove the referencing entries from
  every secondary index before its line-pointer may become `UNUSED`:
  `BTreeIndex`/`InvertedIndex`/`EdgeIndex`/`CsrIndex` get a `remove_rowid`
  pass; `VectorIndex` (no incremental remove) is handled by the explicit
  decision above (rebuild, or exclude vector-indexed tables from slot reuse
  in v1 and document it). Only after an index-clean pass completes may the
  corresponding line-pointers be promoted to `UNUSED`. Test: reproduce the
  aliasing bug deliberately with index vacuum *disabled* (prove it returns a
  wrong-but-visible row), then show it enabled makes the wrong answer
  impossible — the M10 analogue of `graph_mvcc.rs`'s "single most important
  test."
- **M10.d — Space reuse (compaction + FSM), API, benchmarks, closeout.**
  Add intra-page compaction (`Page` currently can't compact) and a
  free-space map so `find_or_alloc_page` prefers vacuumed space over growth
  — the FSM today is just a per-page `free_space()` check, so this also
  retires that tech debt. Wire `Engine::vacuum()` + `VacuumReport`.
  Benchmark: an update-heavy / insert-delete-churn workload, heap file size
  and peak RSS **before vs. after** vacuum (the number that proves the leak
  is closed), plus vacuum's own throughput cost. Extend the crash harness,
  then `PROGRESS.md`/`MEMORY.md`/`README.md`/`docs/` closeout per CLAUDE.md
  §9.

## Known limitations to document (anticipated)

- **Manual, not automatic.** v1 reclaims only when `Engine::vacuum()` is
  called; no threshold trigger, no background autovacuum thread.
- **Long-running RR transactions hold the horizon back** and can stall
  reclamation indefinitely — inherent to snapshot isolation, same as
  Postgres; surfaced in the `VacuumReport`, not silently swallowed.
- **Vector-indexed tables pay extra** (HNSW rebuild on vacuum, or excluded
  from slot reuse in v1) until `VectorIndex` gains incremental remove — a
  dependency worth its own backlog note.
- **Whole-page reclamation, not cross-page defrag.** v1 compacts within a
  page and reuses freed slots/pages; it does not relocate tuples across
  pages to shrink the file's high-water mark (that's a `VACUUM FULL`-class
  operation — backlog).
- **No index bloat reclamation beyond entry removal** — B-Tree/inverted
  structures shrink logically but aren't physically rebuilt/compacted in v1.

## Backlog (explicitly deferred, not part of M10 v1)

- Autovacuum (threshold-driven, background-thread — likely reusing the
  `index_worker` thread pattern).
- `VACUUM FULL`-equivalent: cross-page compaction to lower the file's
  high-water mark and physically shrink the data file.
- `VectorIndex` incremental remove (removes the HNSW-rebuild penalty above)
  — a prerequisite for cheap vacuum on vector-indexed tables.
- Vacuum exposed over REST (an admin route alongside `/checkpoint`).
- Index bloat reclamation (physical B-Tree/inverted rebuild-on-vacuum).
