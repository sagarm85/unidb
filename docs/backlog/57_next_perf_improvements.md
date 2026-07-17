# Post-Step-4 performance gap: architectural review and next-item recommendations

**Type:** Performance
**Status:** Section B (parallel DELETE scan) SHIPPED 2026-07-17, PR on `57-parallel-delete-scan`.
A4 gate NOT MET: parallel scan runs but `delete_many` is the bottleneck post Step 4; DELETE stayed at 0.04× PG. See PROGRESS.md "Item 57" for analysis.
Sections A (HOT) and C (W4/W0) and D (ROI order) remain NOT STARTED.

> Senior DB-internals architectural review produced 2026-07-17 under the §0.6
> expert lens. Calibrated against `docs/performance/benchmark_20260716_232744.md`
> (Step 1 baseline, 100k rows, aarch64 · 18 cores) and
> `docs/performance/benchmark_20260717_074259.md` (Step 3 baseline, 100k rows).
> Current measured state (post Steps 1–4):
>   INSERT 0.54× (655 B/row), SELECT grouped 1.14×, SELECT filtered 0.57×,
>   DELETE selected 0.07× (72 B/row), UPDATE bulk 0.04×, SELECT COUNT(*) 6.64×,
>   W4/W0 at 100k 1.70×.

---

## A — D4 HOT sign-off analysis

### Finding

UPDATE bulk is at 0.04× PG and has been confirmed as an **architectural ceiling**
by direct benchmarking (PROGRESS.md "Item 56 Step 3 — Step 2 investigation").
Batching heap writes did not help because:

1. **B-tree per-row insert dominates** (`src/btree_index.rs` via
   `apply_durable_index_writes` → `flush_index_batches` → `DiskBTree::insert_many`,
   called once per row in `exec_update`). At 50k rows this is ~500 ms of the ~1400 ms
   total (35%), but there is no way to skip it in insert-new-version MVCC because the
   B-tree is the only forward resolver (CLAUDE.md §0.6.2 — confirmed via the
   `patch_many` bug in item 50: skipping maintenance makes live rows unfindable).

2. **Insert-new-version MVCC requires two page cycles per row** (old page xmax-stamp
   + new page insert), both with WAL framing. This is intrinsic to D4.

3. **60 MB staging alloc thrashes L3 cache** at 50k rows when batching is attempted
   (Step 2 bench: 35,547 → 16,919 rec/s, −52%), worse than per-row. No staging
   window solves this without HOT.

Postgres serves `UPDATE t SET body=... WHERE k<N/2` via **HOT** (Heap Only Tuple):
when no indexed column changes and a free slot exists on the same page, the new
version is written in-place; the old index entry stays pointing at the old slot,
which carries a forward pointer to the new slot. Zero index maintenance. This is
why PG is 23× faster here and why it is not a "fair" CRUD workload for any
insert-new-version engine without HOT.

### What HOT requires in unidb

**Tuple header (src/page.rs:23–29)** already has 2 spare bytes at offset [22..24]:
```
[0..8]   xmin     u64
[8..16]  xmax     u64
[16..20] prev_page u32
[20..22] prev_slot u16
[22..24] _pad     u16   ← available for next_slot
```
TUPLE_HEADER_SIZE (currently 24) does NOT increase. The spare bytes carry the
forward pointer from old → new version when HOT conditions are met.

**Semantics change:** old slot gets `xmax = xid` (stamped as superseded, same as
today) and `next_slot = new_slot_id` (forward pointer set). New slot has `xmin =
xid`, written in the same page. The B-tree entry is **left unchanged** — it still
points to the old slot. Resolution follows: B-tree returns (page, old_slot); heap
resolution checks `next_slot ≠ 0` → follow chain to (page, new_slot) → apply MVCC
visibility; if new_slot's xmin is not yet committed, re-read the original slot.

**Conditions for HOT eligibility (in `exec_update`):**
- No indexed column changes (verify by comparing SET columns against all B-tree
  index column sets before choosing the path).
- The new version fits on the same page as the old version (check free space after
  acquiring the old page latch — fall back to regular insert-new-version if full).
- No unique constraint columns in SET (unique enforcement inserts new index entries
  that must point at the new slot, not the old one — HOT would leave them dangling).

**Files and estimated scope:**

| File | Change | Lines estimate |
|------|--------|---------------|
| `src/page.rs` | Add `next_slot: u16` to TupleHeader; getter/setter | ~30 |
| `src/heap.rs` | `hot_update()`: same-page xmax-stamp + in-place new slot; `scan()`: skip redirect slots (next_slot ≠ 0, xmax committed) | ~120 |
| `src/btree_index.rs` | Resolution: follow HOT chain when B-tree entry's target has next_slot ≠ 0 | ~60 |
| `src/sql/executor.rs` | HOT eligibility gate in `exec_update`; fall back for unique/FK tables | ~80 |
| `src/wal.rs` | `WAL_HOT_UPDATE` record type: old_slot, new_slot, new_payload (same page); log_hot_update() | ~60 |
| `src/recovery.rs` | Redo (stamp old xmax + next_slot, write new slot) + undo (clear new slot, clear next_slot + xmax in old slot) | ~80 |
| `src/format.rs` | FORMAT_VERSION bump (6→7) | ~5 |
| `tests/` | New crash tests: P57a (WAL-durable before flush), P57b (undo of incomplete HOT), P57c (page-full fallback) | ~150 |
| Total | — | **~585 lines** |

**Recovery implications:** undo of a HOT update must be two-phase: (1) zero the new
slot's data and reset it to Unused; (2) clear next_slot and clear xmax in the old
slot — restoring it to live. This is order-sensitive: if crash occurs between (1)
and (2), the old slot has xmax set but no forward pointer → appears as a
deleted-but-not-redone row. A crash test at this point is mandatory (P57b above).
LSN-gated redo on the HOT record is identical in structure to the existing
WAL_XMAX_BATCH redo: re-apply only if page.LSN < record.LSN.

**Vacuum (pruning) interaction:** when the old HOT slot is safe to reclaim (xmax
committed and visible to all active snapshots), vacuum must clear next_slot before
marking the old slot Dead — otherwise a reader following next_slot after vacuum
would be directed to a reused slot. Add a MVCC check in `autovacuum.rs`'s heap
scan that handles HOT redirect slots correctly.

### Expected improvement

From the Step 2 bench (PROGRESS.md "Step 2 investigation"):
- Per-row total = 28µs at 35k rec/s
- B-tree insert ≈ 10–12µs (35–43% of total)
- Second page cycle (insert-new-version) ≈ 3–5µs

HOT eliminates B-tree insert (10–12µs) and reduces two-page cycle to one-page
in-place write (saves ~3µs). Estimated savings: 13–15µs/row.

New throughput estimate: ~(28−14)µs = 14µs/row → ~71k rec/s.
vs PG 893k rec/s (074259 baseline) → **~0.08×**.

The original A3 acceptance target (≥0.12×) is **not achievable** with HOT alone
because the remaining 14µs/row of non-B-tree work (exec_update compute pass,
mini-txn bracket, lock acquire, WAL framing, CDC capture) is structural. Honest
ceiling with HOT: **0.07–0.09× PG**.

### Recommendation: DEFER (not never)

HOT's improvement is real (~2× on UPDATE), but:

1. The bench case (`UPDATE t SET body=... WHERE k<N/2`) is the maximally favorable
   case for PG's HOT (unindexed column, plenty of same-page space). Production
   workloads that update indexed columns or that have dense pages will see a smaller
   PG advantage; the 23× gap is not representative.

2. The D4 sign-off process is correct and should not be bypassed for a gain that
   still leaves UPDATE at ~0.08× (still 11× behind PG). Honest ceiling ≠ target.

3. Parallel DELETE (Section B) and item 55 (Section D) are lower-effort, lower-risk,
   and address larger relative gaps or the thesis story.

**Sign-off condition:** sign off on D4 HOT when (a) parallel DELETE and item 55 are
shipped and the remaining customer-visible gap is shown to be UPDATE-dominated in
a realistic workload, AND (b) the implementation is reviewed by a second pair of eyes
against the MVCC resolution and recovery invariants listed above. Record the sign-off
in PROGRESS.md as a D4 superseding note.

**Do not** implement HOT opportunistically — the FORMAT_VERSION bump, the new
crash injection points, and the vacuum interaction make this a meaningful release
with its own correctness surface. File it as a separate backlog item.

---

## B — Parallel DELETE scan design

### Finding

DELETE selected is at 0.07× PG (387k rec/s vs 5.4M rec/s, 14× gap) after Steps 3+4.
The WAL bottleneck is eliminated (72 B/row, Step 3 shipped). PROGRESS.md §A4
honest-miss: "Remaining gap: PG's parallel delete + lock scheduling vs unidb's
sequential scan."

The scan phase in `exec_delete` (executor.rs:2498) calls `matching_rows`
(executor.rs:2706) which at line 2754 does a serial `heap.scan(...)` over all pages:

```rust
// executor.rs:2754 — the bottleneck
for (row_id, bytes) in heap.scan(snapshot, ctx.xid, ctx.pool)? {
    ...
}
```

At 100k rows with `k >= N/2` predicate (matching ~50k rows, non-index-eligible
because A3 gate correctly routes 50%-selective to the scan path):
- ~1250 heap pages at 8 KiB each to touch sequentially
- ~60–70% of total DELETE time is in this scan loop (post Step 3)
- `delete_many` (Step 3's WAL batch) is the other 30–40%

The pre-spawned parallel worker pool (items 45/P, `parallel_scan.rs`) is already
wired for COUNT(*), filtered SELECT, and GROUP BY COUNT. It is **not** used for
DELETE or UPDATE scan phases.

### Design

**Add `parallel_collect_matching` to `src/sql/parallel_scan.rs`** (~80 new lines):

Pattern mirrors `parallel_count_matching` (parallel_scan.rs:540) with
`Arc<Mutex<Vec<(RowId, Vec<u8>)>>>` as the accumulator instead of an `AtomicUsize`:

```
pub fn parallel_collect_matching<F>(
    pages: &[PageId],
    reader: &SharedPageReader,
    snapshot: &Snapshot,
    self_xid: Xid,
    degree: usize,
    matches: &F,        // returns Ok(true) to keep, Ok(false) to drop
) -> Result<Vec<(RowId, Vec<u8>)>>
where
    F: Fn(RowId, &[u8]) -> Result<bool> + Sync
```

Each worker: work-steal pages via the existing `AtomicUsize` cursor; call
`scan_page_into` (already used by all parallel paths); append matching
`(RowId, raw_bytes)` pairs to a worker-local `Vec`. After pool completion: concat
all worker Vecs, then sort by `(page_id, slot)` (required for `delete_many`'s
page-group grouping).

**Modify `exec_delete` (executor.rs:2516) to use the parallel path** (~30 lines):

Replace the `matching_rows` call with:
```rust
let pages = heap.scan_pages(ctx.pool)?;
let matching = if pages.len() >= PARALLEL_CANDIDATE_MIN {
    if let Some(lease) = parallel_scan::acquire(pages.len()) {
        parallel_scan::parallel_collect_matching(
            &pages, &reader, &snapshot, ctx.xid, lease.degree(), &predicate_fn
        )?
        // parallel_collect_matching returns unsorted; sort before delete_many
        .sorted_by_page()
    } else {
        matching_rows(...)? // serial fallback unchanged
    }
} else {
    matching_rows(...)? // small tables: serial is cheaper
};
```

The `predicate_fn` closure is the same B2 decode-and-filter already in
`matching_rows`'s serial loop (executor.rs:2756): deform predicate columns, eval,
keep if match.

**Correctness analysis:**

| Risk | Verdict |
|------|---------|
| MVCC visibility per worker | Safe: each worker calls `scan_page_into` with the shared `Snapshot` (read-only, `Arc`-cloned, same object as the serial path). MVCC is per-page; a row whose old and new versions land in different workers is counted once by the snapshot. |
| Lock acquisition ordering | Safe: `delete_many` calls `try_acquire_write_many` (Step 3, lockmgr.rs:474) on the full RowId set after the scan. RowIds are sorted by `page_id` before `delete_many`, so lock acquisition proceeds in page order — no ordering conflict with other concurrent statements (which also acquire page-order). |
| Write conflict detection | Safe: `try_acquire_write_many` acquires all locks in one mutex pass; any xmax≠0 conflict (Step 3's in-latch check) rejects the whole statement. The parallel collect only reads pages; no writes happen until `delete_many`. |
| CDC ordering (events_enabled) | Safe: the pre-check loop (executor.rs:2536–2560) that calls `send_event_capture` runs **after** the parallel collect, serially over the sorted RowIds. CDC order is by `(page_id, slot)` which is deterministic and matches the serial path's order. |
| FK RESTRICT | Safe: same as CDC — pre-check runs serially after collect. |
| A3 gate interaction | Safe: the parallel path applies only to the full-scan fallback (non-index-eligible predicates). Index-eligible predicates already take the `index_matching_rows` path and produce a sorted RowId list — unchanged. |
| `PARALLEL_CANDIDATE_MIN` gate | Necessary: at small page counts (< 64 pages), parallel overhead exceeds savings. Use the same 64-page threshold as `parallel_count_matching`. |

**Files touched:**

| File | Change |
|------|--------|
| `src/sql/parallel_scan.rs` | Add `parallel_collect_matching` (~80 lines) |
| `src/sql/executor.rs` | Replace `matching_rows` in `exec_delete`'s full-scan path (~30 lines changed) |
| `tests/` | Regression test: parallel-scanned DELETE produces same result as serial; CDC order test |

**Total scope estimate:** ~130–160 lines.

No WAL format change, no FORMAT_VERSION bump, no locked-decision touch. Crash
harness: the existing P56a/P56b tests cover `delete_many` correctness. Add one new
test that forces the parallel path and asserts the committed delete set is identical
to the serial path result.

### Expected improvement

At 100k rows (1250 pages, 18 workers):
- Scan is ~65% of total DELETE time (estimated from before/after Step 3 improvement)
- delete_many (per-page sequential, 50k rows / ~40 rows/page = 1250 groups) is ~35%
- Parallel scan speedup: 18 workers on 1250 pages = ~10–12× for scan portion
  (accounting for pool dispatch overhead ~1–2ms)
- New total time ≈ 0.65T/10 + 0.35T = 0.065T + 0.35T = 0.415T
- Throughput multiplier: T/0.415T ≈ 2.4×
- New DELETE throughput: 388k × 2.4 ≈ **930k rec/s**
- vs PG 5.4M rec/s → **~0.17×** (from 0.07×)

This hits the A4 acceptance target (≥0.15×) that Step 3 alone missed.

Honest caveat: the 10× scan speedup assumes uniform page distribution across
workers and minimal memory contention. In practice, the `Arc<Mutex<Vec>>` merge
overhead at 50k matching rows adds ~1–2ms. At 100k rows this is within the noise
budget. If the merge dominates, switch to per-worker `Vec` + post-pool concat
(same pattern as `parallel_filter_project` — avoids mutex inside the hot loop).

---

## C — W4/W0 multi-model overhead analysis

### Finding

From `benchmark_20260716_232744.md` Table 2 (the most reliable W4 baseline;
the Step 3 benchmark's Table 1 at 100k shows physically impossible W4 < W0,
indicating measurement noise — do not use it for W4 analysis):

| rows | W4/W0 | Δ btree | Δ vector | Δ edge | Δ event | dominant (100k) |
|-----:|------:|--------:|---------:|-------:|--------:|:------|
| 1000 | 4.50× | +0.10ms | +0.00ms | +0.08ms | +0.69ms | **event** (+0.69ms = 79%) |
| 10000 | 1.98× | +0.07ms | +0.07ms | +0.02ms | +0.06ms | btree + vector tied |
| 100000 | **1.70×** | +0.05ms | **+0.06ms** | +0.02ms | +0.03ms | **vector** (+0.06ms = 38%) |

At 100k rows, **Δ vector (W2−W1) = +0.06ms = 38% of the total W4−W0 tax** is the
dominant multi-model overhead.

### Root cause for Δ vector at 100k rows

The `CREATE INDEX ... USING HNSW` path builds a `DiskVectorIndex` (despite the
HNSW name, this is an IVF-flat index — see `disk_vector.rs:10–11`).

Per-INSERT code path (`src/disk_vector.rs:324–333`):

```rust
pub fn insert(&self, rid: RowId, vector: &[f32], pool: &BufferPool, wal: &Wal) {
    let hdr = self.load_header(pool)?;          // 1 page read (pool-cached)
    let centroids = self.load_centroids(&hdr, pool)?;  // nlist pages (pool-cached)
    let cell = nearest_centroid(&centroids, hdr.metric, vector);  // O(nlist×dim)
    DiskBTree::new(hdr.postings_meta, self.page_size).insert(
        OrderedValue::Int(cell as i64), rid, pool, wal,  // ← THE BOTTLENECK
    )
}
```

At 100k rows, `nlist = sqrt(100000).clamp(1,256) = 256`
(`src/sql/executor.rs:47`). Nearest-centroid cost: 256×128 = 32k f32 ops ≈ 0.3µs
on this machine — trivial.

**The `DiskBTree::insert` call** is the bottleneck: it appends a full ~8 KiB leaf
page image WAL record per row (WAL_INDEX, `src/wal.rs:586`). At 100k rows this is
one leaf image per insert into the postings B-tree, accounting for ~6 KiB of the
Δ vector WAL overhead per row.

The same structural issue drives **every W-rung's overhead:**
- W1−W0: +0.05ms ← secondary B-tree leaf image per row
- W2−W1: +0.06ms ← IVF postings B-tree leaf image per row
- W3−W2: +0.02ms ← edge adjacency B-tree leaf image per row + `__edges__` heap FPI
- W4−W3: +0.03ms ← event seq B-tree leaf image per row + `__events__` heap insert + serde_json

### Impact of Step 4 on W4/W0

Step 4 (logical B-tree WAL records, item 56) reduced INSERT per-row WAL from
8837 to 655 B/row. If Step 4's `WAL_INDEX_INSERT` logical record applies to
ALL DiskBTree instances (including the IVF postings tree, edge adjacency tree,
and event seq tree — all using the same `DiskBTree::insert` codepath), then the
multi-model B-tree WAL overhead collapses to ~70 B per rung per row.

**Post-Step-4 W4/W0 estimate (100k rows):**
- Δ btree ≈ +0.001ms (was +0.05ms; purely CPU cost of key encoding + B-tree traversal)
- Δ vector ≈ +0.002ms (was +0.06ms; centroid compute + postings B-tree traversal)
- Δ edge ≈ +0.003ms (was +0.02ms; B-tree traversal + `__edges__` heap insert)
- Δ event ≈ +0.005ms (was +0.03ms; B-tree traversal + `__events__` heap insert + serde_json)
- W4−W0 ≈ 0.011ms (was 0.16ms, 14.5× improvement)
- W4/W0 ≈ (0.23 + 0.011)/0.23 ≈ **1.05×** (from 1.70×)

**However**, the `benchmark_20260716_232744.md` W4/W0 = 1.70× is cited as the
current state post Steps 1–4. This means **no new multi-model benchmark was run
after Step 4 shipped**. Action required: re-run `scripts/report.sh`
(multi-model bench) and report the post-Step-4 W4/W0 before any W4-specific
optimization is proposed. The analysis above predicts the W4/W0 improvement is
already captured by Step 4 if Step 4 applies to all DiskBTree users.

### Remaining W4/W0 overhead AFTER Step 4

If Step 4 ships cleanly for all B-trees, the residual overhead per rung is:
- **Δ event** (dominant): serde_json envelope construction in `send_event_capture`
  (executor.rs:1029–1061) — one `row_to_json` call + one `serde_json::json!` macro
  call + one `std::time::SystemTime::now()` syscall per captured row.
  At 1k rows: this is the 4.50× W4/W0 anomaly investigated in **item 55**.
- **Δ edge**: `__edges__` heap insert mini-txn (structural — cannot be avoided)
- **Δ vector**: centroid distance computation (256×128 f32 ops, trivially fast)

### Concrete W4/W0 improvement (item 55 priority)

The 1k-row W4/W0 = 4.50× (Δ event = +0.69ms) is the **thesis story's visible
weakness**: a customer whose use-case has small tables sees 4.5× overhead per
model, not the 1.05× claimed at scale. This is item 55's investigation target.

Before proposing any new W4/W0 optimization beyond what Step 4 already delivers,
the required action is:

1. Re-run the multi-model bench post-Step-4 to confirm W4/W0 at 100k rows.
2. Profile `send_event_capture` at 1k-row workload (item 55) to identify whether
   the +0.69ms anomaly is in serde_json allocation, the `__events__` heap insert
   (possible buffer-pool thrash on a small DB), or the B-tree seq index insert.
3. If the serde_json path is the bottleneck: replace the `serde_json::json!` macro
   with a fixed-schema append-only builder that reuses a single `String` buffer
   per statement. Estimated saving: 2–5µs per row. Estimated impact on 1k W4/W0:
   +0.69ms → +0.10ms (if serde_json is 80% of the anomaly).

### Is W4/W0 worth prioritising over DELETE/UPDATE improvements?

At 100k rows, W4/W0 = 1.70× already passes the A7 guard (≤2.3×). If Step 4
collapses it to ~1.05×, it becomes a non-issue at scale. The 1k-row anomaly
(item 55) is the only remaining W4/W0 concern — and it is cheap to investigate.

DELETE (0.07×) and the thesis comparison table (Table 4: unidb W4 = 0.19× of PG
relational floor) are more customer-visible gaps. **Prioritise DELETE scan and item
55 over W4/W0 work**, and re-run the multi-model bench first to confirm Step 4's
already-delivered improvement.

---

## D — ROI-ordered next 3 items

Ranking formula: `(addressable_gap_fraction × confidence) ÷ (effort_days + risk_units)`
where risk = 0 (read-only/additive), 1 (new WAL type), 2 (FORMAT_VERSION + recovery
+ D4-sign-off).

### Re-derived ROI from current numbers

| item | current ratio | estimated post-fix ratio | Δ ratio | confidence | effort (days) | risk | ROI score |
|------|:-------------:|:------------------------:|:-------:|:----------:|:-------------:|:----:|:---------:|
| Parallel DELETE scan | 0.07× | 0.15–0.20× | +0.10–0.13 | HIGH | 2 | 0 | **HIGH** |
| Item 55 (event queue 1k investigation + fix) | 4.50× (1k W4/W0) | 1.5–2.0× (1k) | −2.5–3.0 | HIGH | 1–2 | 0 | **HIGH** |
| HOT UPDATE (D4 sign-off) | 0.04× | 0.07–0.09× | +0.03–0.05 | MEDIUM | 8–12 | 2 | MEDIUM |

### Item 1 (highest ROI): Parallel DELETE scan

**What:** Add `parallel_collect_matching` to `src/sql/parallel_scan.rs` and wire
it into `exec_delete`'s full-scan path. Scope ~130–160 lines. See Section B for
the full design.

**Why it wins ROI:**
- DELETE is 0.07× — the worst remaining gap after Step 4 (UPDATE's ceiling is
  architectural; this is not).
- The parallel scan pattern is proven in three existing functions (parallel_count,
  parallel_count_matching, parallel_filter_project, parallel_group_count). Code
  reuse is high; novel logic is low.
- Risk is near-zero: scan is read-only, sort-before-delete_many preserves the
  existing correctness invariant, CDC pre-check loop is unchanged.
- Expected outcome: DELETE 0.07× → **0.15–0.20×**, meeting the A4 target that
  Step 3 alone missed.

**Acceptance bar:** run `scripts/report.sh` post-ship; DELETE selected ≥0.15× at
100k rows. Regression guards: SELECT COUNT(*) ≥5×, SELECT filtered ≥0.55×, INSERT
≥0.50×, W4/W0 ≤2.3×. Concurrency matrix 32/32 PASS.

### Item 2: Item 55 — Event queue small-table investigation and fix

**What:** Profile `send_event_capture` at `MM_SIZES=1000` to root-cause the
W4/W0 = 4.50× anomaly at 1k rows (Δ event = +0.69ms per commit, vs +0.03ms at
100k rows — a 23× size-sensitivity for an operation that is O(1) logically).

**Why it wins ROI:**
- Lowest effort of any W4/W0 improvement (1–2 days: profiling + likely a simple
  fix).
- The 1k-row W4/W0 = 4.50× is the thesis story's most visible weakness for demos
  on small datasets.
- Investigation-first: do not optimize before measuring. The root cause could be
  any of: (a) serde_json allocation per row (likely — `json!` macro creates a full
  Value tree per row), (b) `__events__` heap insert contending on a very small
  buffer pool (small DB = frequent evictions), (c) B-tree seq index splitting
  frequently on a new (small) index.

**Evidence already in code:** `send_event_capture` (`src/sql/executor.rs:1027`)
has per-sub-step timing instrumentation under `RUST_LOG=unidb=debug`:
```
// Item 55 investigation: sub-step timing active under RUST_LOG=unidb=debug.
```
Run `MM_SIZES=1000 RUST_LOG=unidb=debug scripts/report.sh` and read the
`json_us` / `heap_us` / `btree_us` / `persist_us` fields to isolate the bottleneck
before writing any code.

**Expected improvement (if serde_json is the bottleneck):**
Replace the `serde_json::json!` macro (which allocates a full in-memory Value tree)
with a manual `String` buffer that appends the JSON fields directly, avoiding the
intermediate AST. This is a ~30-line change in `src/sql/executor.rs:1029–1061` and
`src/queue/payload.rs`. Estimated saving: 3–8µs per row. At 1k rows (W4-W3 =
+0.69ms): if serde_json is responsible for 0.4ms = 58% of the delta, fixing it
brings the delta to ~0.29ms, and W4/W0 at 1k rows drops from 4.50× to ~2.5×.
This is still above 1×, but the remaining cost is structural (two heap+B-tree
inserts for the event row + seq index entry).

**Acceptance bar:** W4/W0 at 1k rows ≤2.5× (from 4.50×); regression guards
unchanged.

### Item 3: HOT UPDATE (D4 sign-off, deferred)

**What:** See Section A for the full analysis. Implement same-page HOT update chain
using the 2 spare bytes already in TupleHeader. ~585 lines across 7 files. Requires
D4 sign-off in PROGRESS.md before implementation starts.

**Why it is #3 (not higher):**
- Honest ceiling is ~0.07–0.09× PG (from 0.04×), not the original A3 target of
  0.12×. A 2× improvement on the worst gap is real but not decisive.
- Effort (8–12 days) is 4–6× higher than parallel DELETE, with 5× more risk
  (FORMAT_VERSION bump, recovery change, new crash points).
- D4 sign-off process correctly creates a pause point: the improvement must justify
  reopening the locked decision. **It does justify it** — 2× improvement on the
  worst-performing operation — but only AFTER items 1 and 2 clear, so the gap
  context is honest and not masked by easy wins.

**Acceptance bar (when eventually shipped):** UPDATE bulk ≥0.08× at 100k rows
(honest revision of A3 from ≥0.12×); crash harness 42/42+3 new HOT tests; conc
matrix 32/32 PASS; FORMAT_VERSION bump recorded in PROGRESS.md with D4 sign-off
note.

---

## Non-goals / do-not-touch for items in this doc

- No re-litigation of D2, D3, D5, D6, D8/D9.
- Parallel DELETE does not touch `Heap::update_many` or the UPDATE path.
- Item 55 is investigation + targeted fix only — do not restructure the event
  queue's WAL model (that is the "WAL-derived streams" design rejected as the
  unidb moat; see memory file `unidb-moat-and-wal-model.md`).
- HOT (item 3) is gate-blocked on D4 sign-off — do not start implementation until
  sign-off is recorded in PROGRESS.md.
- Further INSERT optimization (0.54→0.60×+): residual gap is fsync-floor
  differential and PG parallel insert. Not addressable without distributed
  group-commit changes (out of scope §1). Do not pursue.
- SELECT filtered (0.57→0.65×+): residual gap is Postgres 18-core parallel worker
  advantage. Architectural. Do not pursue beyond what item 54 already delivered.
