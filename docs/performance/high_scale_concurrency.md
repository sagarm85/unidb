# High-scale concurrency experiment — millions of records

**Date:** 2026-07-10 · **Machine:** Apple M5 Pro, 18 logical cores, 48 GiB, macOS
26.4, native · **Build:** `--release`, group-commit (deferred-sync) on · **Peak
RSS:** ~280 MiB for Table A at 2 M rows (two 2 M-row unidb engines — SQL *and*
raw — plus the Postgres client, all resident at once); ~92 MiB for the
A+B+C sequential sweep (engines built and dropped one at a time).

Reproduce: `UNIDB_BENCH=hiconc cargo bench --bench decompose` (knobs:
`HICONC_PREGROW`, `HICONC_PER`, `HICONC_IDX_PREGROW`, `HICONC_SIZES`,
`HICONC_ONLY=a|b|c`). Harness added in `benches/decompose.rs::bench_hiconc`. The
Postgres column in **all three tables** is gated on `PG_URL` (a superuser conn
string, e.g. `PG_URL="host=/tmp port=5432 user=<you> dbname=postgres"`); unset →
the columns are skipped and unidb runs alone.

Each measured phase = **N writer threads over one `Arc<Engine>`, each committing
`per` durable single-row INSERTs** (begin → insert → commit; one group-coalesced
fsync per commit). Tables are pre-grown **once** (not timed) so page count / tree
depth are realistic.

---

## Results

### A. Writer-count scaling — unidb (SQL & raw) vs Postgres, at 2 M rows

2 000 commits/writer, all three engines pre-grown to **2 000 000 rows**, one
clean single pass. Postgres is under the **matched-durability lens**
(`wal_sync_method = fsync_writethrough` = `F_FULLFSYNC`, the only macOS setting
that truly flushes to platter — the same guarantee as unidb's `File::sync_all`).
The lens is set via `ALTER SYSTEM` and **reset to the default afterward**, so the
dev server is left as found.

| writers | unidb_sql    | unidb_raw   | postgres     |
|--------:|-------------:|------------:|-------------:|
| 1       | 322 (1.00×)  | 315 (1.00×) | 311 (1.00×)  |
| 2       | 321 (1.00×)  | 296 (0.94×) | 317 (1.02×)  |
| 4       | 632 (1.96×)  | 346 (1.10×) | 656 (2.11×)  |
| 8       | **1291 (4.01×)** | 427 (1.35×) | **1290 (4.15×)** |

**Headline: unidb's concurrent SQL writes track PostgreSQL almost exactly at
every writer count and scale identically (4.01× vs 4.15×; 1291 vs 1290 commits/s
at 8 writers) — at 2 M rows, under matched flush-to-platter durability.** On the
workload both engines actually share (durable single-row INSERT under
concurrency), unidb is at parity with the default Postgres engine. This confirms
the pg-baseline finding (PR #25) and refutes the older "the catalog `RwLock`
serializes SQL writers" prediction. (A prior two-part run gave 1273 / 1249 — same
story; run-to-run noise is a few percent, the parity and ~4× shape are robust.)

**One real regression — the raw path on a large cold heap.** `unidb_raw` scales
~3.9× on a *small/fresh* heap (see the 100 k run below) but here collapses to
1.35× (427 commits/s) at 8 writers on the 2 M pre-grown heap. All raw writers
target the same heap **tail page(s)** handed out by the free-space map, so at high
writer counts they ping-pong on one page latch + FSM state instead of spreading
out. The SQL path does *not* show this (its coarse per-statement catalog lock
serializes the heap touch, so there is no page-latch ping-pong; group commit then
coalesces the fsync). Postgres does not show it either (heap-page free-space is
spread across writers). **So raw-path write scaling is heap-state-dependent;
SQL-path scaling is not.**

For reference, the clean small-heap run (100 k rows, where raw does not yet hit
tail-page contention):

| writers | unidb_sql   | unidb_raw   |
|--------:|------------:|------------:|
| 1       | 320 (1.00×) | 318 (1.00×) |
| 4       | 642 (2.01×) | 642 (2.02×) |
| 8       | 1238 (3.87×)| 1261 (3.97×)|

### B. Size-independence (8 writers) — unidb vs Postgres

| table rows | unidb_sql | postgres |
|-----------:|----------:|---------:|
| 100 000    | 1281      | 1290     |
| 500 000    | 1285      | 1269     |
| 1 000 000  | 1279      | 1254     |
| 2 000 000  | 1286      | 1288     |

**Both engines are flat from 100 k to 2 M rows, and neck-and-neck.** The durable
on-disk FSM (PR #29) holds: `Heap::open` is O(1) and page growth is an O(log n)
FSM write, so nothing bends with table size. Postgres — with far more mature
storage management — is likewise flat. The ceiling is *not* the table for either.

### C. Indexed vs unindexed insert (300 k-row table) — unidb vs Postgres

| schema   | writers | unidb_sql   | postgres     |
|----------|--------:|------------:|-------------:|
| no-index | 1       | 318 (1.00×) | 309 (1.00×)  |
| no-index | 8       | 1292 (4.06×)| 1291 (4.17×) |
| indexed  | 1       | 272 (1.00×) | 295 (1.00×)  |
| indexed  | 8       | **904 (3.32×)** | **1243 (4.21×)** |

**This is the one place unidb materially trails Postgres — and it is exactly the
concurrency signal.** Unindexed, they match (1292 vs 1291, both ~4×). Add a
secondary B-tree and Postgres barely notices (1291 → 1243, still 4.21×), while
unidb drops to 904 (3.32×) — **~27 % slower and visibly worse scaling.** The
reason is structural: Postgres maintains its index **concurrently** (fine-grained
B-tree latching / no global lock), so 8 indexed writers still coalesce their
fsyncs and scale. unidb maintains the index **inside the serialized catalog-lock
section**, so the extra per-insert B-tree work enlarges the serial fraction and
eats into the group-commit win. **This gap (904 → 1243, ~1.4×) is the concrete
prize for the catalog-lock split + latch-coupled B-tree work — see Q2.**

> **Post-fix update (2026-07-10, index-write-concurrency SHIPPED).** The
> catalog-lock split (0a/0c) + latch-coupled ("crabbing") `DiskBTree` descent
> (Item A) landed behind the default-off `UNIDB_CONCURRENT_SQL_WRITES` toggle. A
> re-run of Table C (native Apple silicon, `HICONC_IDX_PREGROW=200000`, per-commit
> durable) shows the indexed 8-writer prize **realized**:
>
> | schema   | writers | toggle OFF | toggle ON |
> |----------|--------:|-----------:|----------:|
> | no-index | 8       | 1263 (3.86×) | 1260 (3.97×) |
> | indexed  | 8       | **768 (2.57×)** | **1058 (3.74×)** |
>
> Indexed 8-writer recovers **768 → 1058 commits/s (+38%)**, from ~61% to ~84% of
> the unindexed floor; unindexed is unchanged (already fsync-bound); toggle off
> reproduces the serialized baseline. The residual gap to the floor is
> `WAL_INDEX` full-node-page-image append contention (WAL-format-inherent), not
> tree-latch serialization. (Absolute numbers here are lower than the 904/1243
> above because this re-run is on a different machine; the *mechanism* and
> *direction* match.) Full detail: `PROGRESS.md`'s "Index & heap write
> concurrency" entry.

---

## Interpretation — where the ceiling actually is

Per-commit cost splits into (a) serialized executor CPU work under the
per-statement catalog write lock (`execute_sql_inner` holds
`cat_write(&self.catalog)` across parse→plan→heap-RMW→WAL-append) and (b) the
commit-time group-commit **fsync**, which happens in `Engine::commit`, **outside**
that lock.

At today's ~3 ms/fsync durability floor, (b) dominates and (a) is tiny. So:

- Adding writers deepens the group-commit batch → throughput rises ~linearly to
  the **fsync-throughput ceiling (~1250 commits/s)**, independent of table size.
- The catalog write lock is **not** the binding constraint yet — the serial CPU
  section is a few µs against a ~3 ms fsync. That is *why* SQL scales like raw.
- **Postgres hits the same ceiling (~1290 commits/s, 4.15×).** The wall is the
  storage's flush-to-platter latency, not a unidb-specific defect — the default
  Postgres engine, purpose-built for exactly this, lands in the same place under
  matched durability. This reframes "can we improve at scale?": we are not behind
  a mature incumbent that we can catch up to; we are both against the same
  physical floor.

---

## Q1 — Concurrent SQL writes: where would improving them help, and can we at scale?

**They already scale ~3.9×/8 cores and hold flat to 2 M rows.** The binding
constraint is group-commit fsync throughput, not concurrency and not table size.
So "improve concurrent writes" only pays off along these axes, in order:

1. **Raise the durability ceiling itself** (biggest lever): larger/adaptive
   group-commit batching, or an async/pipelined commit path, or faster storage.
   This lifts the ~1250/s ceiling that *both* paths hit.
2. **Shrink the serialized section** — matters only *after* (1). The catalog
   write lock is held for the whole executor body even for a plain INSERT that
   changes no schema. Splitting it into (i) a read-mostly schema lock and (ii)
   **per-table** write locks removes cross-table false serialization. On fast
   storage (µs-scale fsync) this becomes the dominant win; today it is nearly
   invisible.
3. **Reduce per-commit work** (e.g. index maintenance, table C): lowers the
   serial fraction, helping most once (1)+(2) expose it.
4. **Spread heap inserts across pages** — a *raw-path* fix, orthogonal to the
   above. The raw path's collapse at 2 M rows (485 commits/s, 1.55×) is hot
   tail-page latch contention, not durability. Handing concurrent inserters
   *different* target pages (per-writer insertion cursors / multiple open tail
   pages, as Postgres does) restores raw scaling on large heaps. The SQL path
   doesn't need this today (its catalog lock masks it), but it becomes relevant
   the moment the catalog lock is split in (2).

**Bottom line:** at high scale we do *not* have a concurrency problem to fix on
the SQL write path today — we are at **Postgres parity against a shared fsync
floor**. Concurrency improvements are real but **second-order until that floor is
lowered.** The one concrete concurrency defect is raw-path tail-page contention
on large heaps (lever 4), which the SQL path currently hides.

## Q2 — Is latch-coupled ("crabbing") B-tree descent worth doing alongside this?

**Not yet, and not alone.** Crabbing makes *concurrent* descent of the same
`DiskBTree` safe and non-serializing. But today:

- `DiskBTree` is a stateless struct over the shared pool with **no** latch
  coupling, and its `insert` runs inside the executor **under the global catalog
  write lock** — so **no two threads ever descend the tree at the same time.**
  Crabbing would protect concurrency that structurally cannot occur.
- Even the pool's existing per-page latches can't help here: one writer is in the
  executor at a time.
- Table C shows index maintenance is fully serialized (indexed scales *less* than
  unindexed precisely because its work sits in the serial section) — crabbing
  would change none of that while the catalog lock is above it.

**But Table C also proves crabbing eventually pays — and quantifies it against
Postgres.** Indexed, 8 writers: unidb 904 vs **Postgres 1243** (both engines
identical when *unindexed*). Postgres keeps ~4× scaling on indexed writes
precisely *because* it does concurrent, latch-coupled B-tree maintenance instead
of serializing it. So the crabbing win is real and worth **~1.4×** on
concurrent indexed writes — it is simply **gated behind the catalog-lock split**,
without which no two writers reach the tree together.

**Correct sequencing:** (1) lower the fsync/durability ceiling → (2) split the
catalog lock into per-table write locks → *then* multiple writers actually
descend the same index concurrently → **(3) crabbing becomes the fix that keeps
that descent correct and non-serializing, closing the indexed-write gap to
Postgres.** Doing crabbing before (2) is optimizing a path no workload can
currently take. It belongs on the roadmap **after** the catalog-lock split, not
bundled with generic "concurrent writes" work.

---

## Caveats

- Single-node, single NVMe, macOS `F_FULLFSYNC`. The ~1250/s ceiling is a
  property of this storage's fsync latency; on hardware with faster durable sync
  the ordering in Q1/Q2 shifts *toward* the lock-splitting and crabbing work
  sooner. A Linux/enterprise-SSD re-run is the natural follow-up.
- Bursts are modest (2 k commits/writer); numbers are throughput estimates, not
  criterion-grade distributions. The *shape* (scaling, flatness, ceiling) is
  robust across repeats; absolute values carry ±few-percent noise.
