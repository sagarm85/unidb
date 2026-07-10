# Index & heap write concurrency — latch-coupled B-tree + spread insertion targets

## Status as of 2026-07-10: **SHIPPED** (Core-lane, branch `index-write-concurrency`) — the first landed unit **0a + 0c + Item A** is implemented behind the default-off `UNIDB_CONCURRENT_SQL_WRITES` toggle, validated per the strategy below (structural validator + split-injection + loom + generation tripwire; TSan is the documented CI hook), and accepted on Table C (indexed 8-writer **768 → 1058 commits/s** with the toggle on, toggle-off reproducing the baseline). See `PROGRESS.md`'s "Index & heap write concurrency (0a + 0c + Item A)" entry for the full before/after and `docs/design/engine_design.md` §5.4. **0b (per-table lock registry) remains deferred/optional; Item B (heap-tail spread) not landed here.** Original PLAN retained below for the record.

> **Correction (2026-07-10, high-scale concurrency experiment — millions of rows,
> unidb vs Postgres head-to-head; `docs/performance/high_scale_concurrency.md`).**
> The experiment re-ran this workload at 100 k → 2 M rows *with a matched-durability
> Postgres column*, and two statements below need adjusting (evidence-based, per
> §9 — the mechanism analysis in Items A/B is otherwise confirmed):
>
> 1. **The Non-goals claim "neither item beats the fsync floor in the
>    per-commit-durable regime; their win shows [only] in deferred-sync/batched
>    workloads" is too strong for *indexed* tables.** Table C measured, in the
>    ordinary per-commit-durable regime (one coalesced fsync/commit), 8-writer
>    indexed INSERT: **unidb 904 (3.32×) vs Postgres 1243 (4.21×)** — while
>    *unindexed* the two are identical (1292 vs 1291). Postgres reaches 1243 in the
>    *same* durability regime, so a **~1.4× win for indexed concurrent writes is
>    available without removing the fsync floor.** The floor (~1290) is not beaten;
>    rather, unidb's indexed writes currently fall *below* it (to 904) and Item A
>    recovers them toward it. Root cause: unidb serializes all index maintenance
>    (B-tree descent + `WAL_INDEX` full-node-page append) under the global catalog
>    write lock, so 8 writers' index work is fully serial; Postgres overlaps it.
>    ⇒ The Item A acceptance test does **not** need a fsync-removed variant —
>    `benches/decompose.rs` Table C (`UNIDB_BENCH=hiconc`, `HICONC_ONLY=c`) is a
>    ready per-commit-durable acceptance test as-is.
>
> 2. **Item A has an unlisted prerequisite: split the catalog write lock (new
>    Item 0 below).** While `execute_sql_inner` holds `cat_write(&self.catalog)`
>    across the whole executor body, no two writers descend the tree concurrently
>    at all — so crabbing *alone* changes nothing. Crabbing only pays *after* the
>    catalog lock is split into per-table write locks. Sequencing: **Item 0 →
>    Item A**. (Item B / heap-tail spread is independent of the catalog lock and
>    can land any time; the raw-path regression it fixes — 8-writer raw INSERT
>    collapsing to 1.35× / 427 commits/s on a 2 M cold heap — was also reproduced.)
>
> The Q1 finding is unchanged and now directly measured: concurrent *unindexed*
> SQL writes are flat ~1,290 commits/s at 8 writers, 100 k → 2 M rows, at parity
> with Postgres (1291 vs 1290), fsync-bound.

Raising the **concurrent SQL-write ceiling** past the current group-commit-fsync
floor. The durable-FSM milestone (2026-07-10) confirmed concurrent SQL-insert
throughput is **flat ~1,250 commits/s at 8 writers, at every table size** (0 →
7,500 pages) — at parity with Postgres, bounded by the coalesced commit fsync,
**not** by anything the FSM change touched. To go *faster* per commit, the
serialization has to come out of the page/index layer. Two independent items,
both additive, neither reopens a §3 decision.

## Item 0 — Split the per-statement catalog write lock (prerequisite for Item A)

**Problem.** `execute_sql_inner` (`lib.rs`) takes `cat_write(&self.catalog)` — a
single engine-wide exclusive lock — for the *entire* executor body of *every* SQL
statement, including a plain INSERT that changes no schema. So all SQL writers are
serialized before they ever reach the heap or an index; the coarse lock, not the
tree, is what one-at-a-times them. Confirmed by the high-scale experiment: indexed
8-writer throughput (904) trails Postgres (1243) precisely because the index work
is serial under this lock (unindexed, they match — the lock's serial section is
tiny until index maintenance enlarges it).

**Fix.** Separate schema access (read-mostly, shared) from row-write access
(per-table exclusive). A plain INSERT should take a *shared* catalog read lock
(schema is stable) plus a **per-table** write lock, so writers to different tables
never serialize and writers to the same table serialize only as much as the
storage/index layer actually requires. This is what lets Item A matter at all.

**Acceptance:** concurrent INSERTs into *different* tables scale with cores in
per-commit-durable mode; same-table indexed INSERT throughput moves toward the
unindexed/Postgres ceiling once combined with Item A.

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

**Acceptance:** with Item 0 in place, concurrent inserts into an *indexed* table
recover from ~904 toward the ~1290 unindexed/Postgres ceiling in the ordinary
per-commit-durable regime (the ~1.4× gap the high-scale experiment measured —
`benches/decompose.rs` Table C, `UNIDB_BENCH=hiconc HICONC_ONLY=c`, is the
ready-made test; no fsync-removed variant is required). A deferred-sync /
batched-commit variant additionally shows the win once the fsync floor is removed
and the index is the sole serializer.

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

- The **group-commit fsync floor** (~1290 commits/s here) is fundamental — neither
  item lets a durable single-table commit rate *exceed* it, and *unindexed* writes
  are already at that floor and at Postgres parity. What the items recover is the
  **indexed** shortfall *below* the floor (904 → ~1290; see the Correction) plus
  cross-table and deferred-sync/batched scaling. The project's real
  durable-throughput edge remains the **one-commit multi-model write**
  (row+vector+edge+event in a single fsync vs four systems), not a faster
  single-table commit rate.
- Batching the **SERIAL** counter persistence (`alloc_serial` persists per
  allocation under the catalog write lock — `docs/backlog/phase2_data_model.md`
  P2.d) is a related, smaller concurrency win filed there; it is folded into
  Checkpoint 0c below because the lock split forces it.
- Stays within §1 (single-primary) and all §3 storage decisions.

---

# Implementation plan (2026-07-10)

**B is independent** and may land in parallel (it touches only
`Heap::find_or_alloc_page`, not the catalog). One PR per checkpoint; each is
green-on-`main` (crash harness + clippy/fmt + concurrency stress) before the next.

### Scope refinement (2026-07-10) — 0-core and A are ONE landed unit; 0b is OPTIONAL

Two clarifications from the risk review, both of which *shrink* the change:

1. **0-core (`cat_write → cat_read` for DML) + A (crabbing) must land together, and
   they reduce to a single local invariant.** Splitting them buys nothing and is
   unsafe halfway:
   - *Unindexed* tables: `cat_read` is safe **but pointless** — the executor body
     is µs and group commit already coalesces the dominant fsync (that is exactly
     why Table A/B already scale ~4× under today's `cat_write`). Nothing moves.
   - *Indexed* tables: `cat_read` is **unsafe without A** — two writers would race
     the un-latched `DiskBTree` — and A is inert without `cat_read` (nothing
     descends concurrently under `cat_write`). So the win (Table C, 904 → ~1290)
     only appears when **both** ship, and the entire new correctness question
     collapses to **"is the `DiskBTree` crabbing protocol correct under concurrent
     insert/split?"** — one small, local, *validatable* invariant (see
     §"Validation strategy").
2. **0b (the per-table lock registry) is a later, OPTIONAL refinement — not
   required for correctness.** The existing catalog `RwLock` *already* gives the
   load-bearing semantics: many DML hold `cat_read` and overlap; DDL holds
   `cat_write`, drains readers, and runs quiescent. 0b only makes DDL-on-table-X
   stop blocking DML-on-table-Y — a throughput nicety. **Deferring 0b removes the
   only new lock tier (and the only new lock-ordering-cycle risk) from the first
   landed unit.** Do it later, on its own, if cross-table DDL blocking ever
   matters.

**So the first (and only strictly-needed) landed unit = 0a + 0c + A**, validated on
the concurrent-indexed-insert workload. 0b is filed but deferred.

## Grounding facts (verified in-tree, 2026-07-10)

- `execute_sql_inner` (`src/lib.rs`) takes `cat_write(&self.catalog)` for the
  **whole executor body of every statement**, including a plain INSERT. `ExecCtx`
  carries `catalog: &mut Catalog`, so the executor is written assuming exclusive
  access.
- **A modern (`fsm_meta = Some`) non-SERIAL table's INSERT/UPDATE/DELETE does not
  mutate the catalog**: `persist_pages_if_changed` is a no-op for FSM-backed tables
  (`Heap::is_fsm_backed`), and there is no other catalog write on that path. The
  only DML catalog mutations that remain are (i) `alloc_serial` (SERIAL columns —
  bumps + persists `TableDef.serial_next`) and (ii) the legacy `set_pages`
  (pre-FSM tables, `fsm_meta = None`). DDL (`CREATE/ALTER/DROP/TRUNCATE/CREATE
  INDEX`) mutates and persists the catalog.
- `DiskBTree` (`src/btree_index.rs`) descent (`find_leaf`, `insert_into`) is
  `fetch_page → deserialize → unpin` with **no latch held across levels**; splits
  propagate bottom-up (`insert_into` returns `Some((sep_key, new_page))`); leaves
  are right-linked (`Node::Leaf { next }`). The per-page S/X latch table exists:
  `BufferPool::latch_shared` / `latch_exclusive` (RAII guards). Each insert is one
  redo-only `WAL_INDEX` full-node-page mini-txn.
- Lock-ordering invariant already in force (P5.e, `heap.rs`): the heap FSM `Mutex`
  is **never** held across a page-latch acquisition or WAL I/O. Row locks go
  through `LockManager` (S/X modes, `Condvar` wait queues, **wait-for-graph
  deadlock detection** → `DbError::Deadlock`).

## Checkpoint 0 — split the per-statement catalog write lock

### 0a — DML takes a shared catalog lock; DDL keeps exclusive
- Route `execute_sql_inner` by statement kind: DDL → `cat_write` (unchanged);
  row-DML (`INSERT/UPDATE/DELETE/SELECT`) → `cat_read`. This needs the executor's
  read paths to take `&Catalog` rather than `&mut Catalog`; they already
  `lookup(table)?.clone()` the `TableDef`, so the body works off an owned clone —
  the `&mut` is only *nominally* required. Split `ExecCtx` into the shared fields +
  a narrow "catalog-mutation" capability used solely by 0c and DDL.
- **Escalation:** a DML statement that *does* need a catalog write (SERIAL bump on
  a table with an identity column, or a legacy non-FSM table's `set_pages`) drops
  the read guard and re-acquires `cat_write` for that mutation only (or routes
  through 0c's narrower lock). Legacy `fsm_meta = None` tables may simply keep the
  whole-statement `cat_write` path — they are a documented pre-FSM minority.

### 0b — per-table lock registry (RowExclusive-equivalent) — **OPTIONAL, DEFERRED**
*Not part of the first landed unit (see Scope refinement). File separately; build
only if DDL-on-X-blocks-DML-on-Y becomes a measured problem. The existing catalog
`RwLock` already gives DML-overlap + DDL-exclusion without it.*
- New `table_locks: RwLock<HashMap<TableId, Arc<RwLock<()>>>>` on `Engine` (lazy
  insert; `TableId` = the table's stable catalog id/name). DML takes the table's
  lock **shared**; DDL takes it **exclusive** so DDL-on-X stops blocking DML-on-Y.
- **Global lock order (deadlock-free by construction):** `catalog schema
  (RwLock)` → `per-table (RwLock)` → `row lock (LockManager)` → `page latch` →
  `heap FSM mutex`. Never acquire upward. The existing wait-for-graph detector
  covers the row-lock tier; the two `RwLock` tiers are always taken top-down and
  released in reverse, so they add no cycle.

### 0c — concurrency-safe SERIAL + drop `write_serial` from the SQL DML path
- Make `TableDef.serial_next` increments not require the global catalog write
  lock: hold the value in an `AtomicI64` per identity column (or a tiny per-table
  mutex), hand out ids lock-free, and **persist the high-water mark lazily/batched**
  (fold in the P2.d filed item) rather than once per row under `cat_write`.
- Audit the `write_serial` sites (`src/lib.rs`, 10 call sites): the SQL DML path
  must no longer depend on it for catalog/index safety once 0a/0c + Item A land.
  Edges/LOBs/events keep `write_serial` for now (their multi-page RMWs are out of
  scope — noted as a follow-up once crabbing generalizes to `__edges__`).

**0-core (0a + 0c) acceptance:** with the catalog `RwLock` alone (no 0b), same-
table *unindexed* concurrent INSERT stays correct and unchanged in throughput
(already fsync-bound); single-writer throughput unchanged; crash harness green;
RC/RR/SSI isolation tests unchanged. The *indexed* win is validated jointly with
Checkpoint A (below) — 0-core does not ship for indexed tables without A.

## Checkpoint A — latch-coupled ("crabbing") B-tree descent

Lands **together with 0-core** as one unit (see Scope refinement) — 0-core is what
first lets two writers reach the same tree concurrently, and A is what makes that
safe. Neither is useful alone.

### A1 — latch-coupled descent
- `find_leaf` / the read path: acquire the child's **shared** latch before
  releasing the parent's (`latch_shared`), so a concurrent split can't make the
  descent follow a stale child pointer.
- `insert_into`: descend under shared latches; **exclusive**-latch the target leaf
  for the actual entry write. Keep the mini-txn/`WAL_INDEX` write exactly as today
  (D5 preserved by `write_node`).

### A2 — safe splits
- **Leaf split (no new format):** the leaf is already right-linked; a descender
  or `search_eq` that lands on a just-split leaf follows `next_leaf` to find a
  migrated key (Lehman-Yao leaf behaviour the code half-implements already).
- **Internal split:** start with **optimistic coupling + pessimistic restart** —
  descend shared; if the leaf must split (rare), release and re-descend taking
  exclusive latches along the path that may change. No format change.
- **Follow-up (separate PR, format-bump-gated):** add right-links to *internal*
  nodes for a full Lehman-Yao B-link tree (splits then latch only the splitting
  node, never the parent). This changes the internal-node body layout →
  `FORMAT_VERSION` bump (D9) → **needs the §3 sign-off ritual**; keep it optional.

### A3 — recovery unchanged
- Nodes stay full-page redo-only `WAL_INDEX` images; each insert is still one
  mini-txn. Concurrency changes *who* writes a node, not *how* it recovers.
  Invariants: D5 (WAL-before-page), P5.e (no page latch under the heap FSM mutex),
  index latches acquired strictly root→leaf.

**Checkpoint A acceptance:** with 0 in place, 8-writer **indexed** INSERT in the
per-commit-durable regime recovers from ~904 toward the ~1290 unindexed/Postgres
ceiling — measured by `benches/decompose.rs` Table C (`UNIDB_BENCH=hiconc
HICONC_ONLY=c`). A deferred-sync variant shows the index-bound scaling once the
fsync floor is removed.

## Validation strategy (the "can we validate a concurrency change before commit?" question)

Ordinary unit tests are necessary but **not sufficient** — a race can pass tests
thousands of times and still exist. No pre-commit test *proves* absence of races.
But the risk here is bounded and validatable because, per the Scope refinement, the
only genuinely-new hazard is **one local invariant: the `DiskBTree` crabbing
protocol under concurrent insert/split**. That is small enough to attack directly:

1. **Structural validator (loudest, cheapest).** After any concurrent-stress run,
   walk the tree: every inserted `(key,rid)` reachable via `search_eq`; leaf chain
   sorted and fully linked (`next_leaf`); internal separators consistent; no lost
   or duplicated entries. Turns silent index corruption into a hard failure. Run it
   as the assertion at the end of every concurrency test.
2. **Deterministic interleaving injection.** Reuse the crash-harness mindset (D7
   defined points): add debug-only pause points in `insert_into`/split, force the
   dangerous schedules (writer B paused mid-descent while writer A splits the same
   node; two writers splitting sibling leaves), resume, run validator (1). Converts
   a heisenbug into a deterministic, checked-in test.
3. **`loom` on the extracted protocol.** Model the crabbing latch-acquire/release +
   split logic with `cfg(loom)` types and let loom exhaustively enumerate
   interleavings for a bounded tree. This is genuine *proof* for the bounded model
   (not the whole engine, but the part that carries the risk).
4. **ThreadSanitizer in CI.** Run the indexed `concurrent_writers` stress under
   `-Zsanitizer=thread` (Linux CI target `x86_64-unknown-linux-gnu`, well-supported)
   — catches unsynchronized shared access even on interleavings that didn't visibly
   fail.
5. **Schema-generation tripwire (validate-by-construction).** A `u64` generation on
   each `TableDef`, bumped by DDL under `cat_write`; a DML captures it at clone and
   `debug_assert!`s it still matches at write time. If the `cat_read`/`cat_write`
   discipline is correct this never fires — so it is a cheap guard that converts the
   one "stale clone" window into a panic in tests/stress rather than a silent bug.
6. **Randomized concurrency property test.** Random schedules of {INSERT, UPDATE,
   DELETE, CREATE INDEX, DROP, TRUNCATE} across N threads, each iteration ending in
   validator (1), run many iterations under (4). High coverage, not a proof.

**Safety net that bounds the commit itself.** Gate the `cat_read` DML path behind a
config/env toggle (`UNIDB_CONCURRENT_SQL_WRITES`, **default off**). Ship dark, soak
under (4)+(6), flip default-on only after soak. If a surviving race ever appears in
the field, flip back to the known-safe `cat_write` serialization **without reverting
code** — the old path stays compiled in. This is what makes the change committable
despite the impossibility of *proving* race-freedom: the downside is one env var.

**Verdict:** we cannot *prove* it pre-commit, but we can reach high confidence via
(1)–(6) — because the risk is localized to the crabbing protocol — and the toggle
bounds the residual. "Can't validate before commit" is too strong; "can't prove,
but can validate to high confidence and bound the downside" is accurate.

## Test matrix

| Area | Test | Gate |
|------|------|------|
| Correctness (concurrent index writes) | extend `tests/concurrent_writers.rs`: N threads INSERT into one **indexed** table, overlapping + distinct keys → every row present, every `search_eq` resolves, no lost/dup index entries | must pass |
| MVCC aliasing (M10.c) | vacuum interleaved with concurrent index writes — stale-slot-reuse gate still holds | must pass |
| Isolation | existing RC / RR / SSI tests (`lib.rs`, `btree_mvcc.rs`) unchanged | no regression |
| Tree structural validator | after every concurrency test: all keys reachable, leaf chain sorted+linked, no lost/dup entries (Validation §1) | must pass |
| Split interleaving injection | deterministic paused-mid-split schedules (Validation §2) | must pass |
| `loom` on crabbing protocol | bounded exhaustive interleaving (Validation §3) | must pass |
| ThreadSanitizer | indexed `concurrent_writers` under TSan, Linux CI (Validation §4) | clean |
| Same-table indexed scaling (A) | Table C, `HICONC_ONLY=c` → 904 → ~1290, toggle **on** | acceptance |
| Toggle-off regression | `UNIDB_CONCURRENT_SQL_WRITES=0` reproduces today's numbers exactly (old `cat_write` path intact) | must pass |
| Deadlock | 2-thread cross-index ordering → detector fires cleanly, no hang | must pass |
| Crash recovery | crash harness P13/P14/P15 (durable-index) still green; optional new point: kill mid-concurrent-split → reopen → tree valid, all committed keys present | no regression (+1 optional) |
| Raw regression (B, if landed) | Table A `unidb_raw` at 2 M rows recovers from 1.35× | acceptance |

## Sequencing, risk, effort

- **0-core (0a+0c) + A land as one unit** (see Scope refinement) — splitting them
  buys nothing and is unsafe halfway. The executor catalog-borrow refactor (0a) is
  the load-bearing part; the crabbing protocol (A) carries the concurrency risk.
- **The risk is localized to the `DiskBTree` crabbing protocol** and validatable to
  high confidence via the Validation strategy (structural validator + injection +
  loom + TSan + generation tripwire). We cannot *prove* race-freedom pre-commit; the
  **default-off `UNIDB_CONCURRENT_SQL_WRITES` toggle bounds the residual** — ship
  dark, soak, flip on after soak; revert-in-the-field is one env var.
- **0b (per-table locks) is deferred/optional** — removing it takes the only new
  lock tier (and the only new lock-ordering-cycle risk) out of the first unit.
- **Deadlock** is only a live hazard for the deferred 0b; the fixed global lock
  order + existing wait-for-graph detector contain it. Still add the 2-thread test.
- **No `FORMAT_VERSION` bump** for 0-core, A1–A3 (A2 optimistic path). Only the
  optional full-B-link follow-up (internal right-links) bumps the format and needs
  §3 sign-off.
- **§3 untouched** otherwise; single-primary (§1) unchanged.

## Definition of done

Per §9: feature works end-to-end · crash harness green · **structural validator +
loom + TSan clean; toggle-off reproduces baseline exactly** · Table C acceptance
number recorded in `PROGRESS.md` with before/after · `docs/performance/high_scale_concurrency.md`
updated with the post-fix Table C · this doc's status flipped to SHIPPED pointing
at the `PROGRESS.md` entry · `engine_design.md` §5.4 ("latch-coupled B-tree writes
are future work") and the Phase 5 known-limitations line updated · no §3 decision
reopened without recorded sign-off. **Ship with `UNIDB_CONCURRENT_SQL_WRITES`
default-off; a follow-up commit flips the default on after a soak period, recorded
in `PROGRESS.md`.**
