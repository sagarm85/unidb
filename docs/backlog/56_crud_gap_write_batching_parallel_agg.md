# CRUD gap closure — write-path batching + parallel aggregation

**Type:** Performance
**Status:** IN PROGRESS — Step 1 SHIPPED 2026-07-16 (1.14× PG); Steps 2+3 SHIPPED 2026-07-17 (branch `56-step3-delete-wal-batch`, benchmark_20260717_074259.md); A6 PASS (DELETE WAL 72 B/row); A3/A4/A5 honest-miss — see PROGRESS.md for root-cause analysis; Step 4 gated

> Design/planning story produced 2026-07-16 under the §0.6 expert lens
> (senior DB-internals review, code paths verified in `src/sql/executor.rs`,
> `src/heap.rs`, `src/wal.rs`, `src/bufferpool.rs`, `src/sql/query_exec.rs`,
> `benches/decompose.rs` — not assumed).
> Calibrated against `docs/performance/benchmark_20260716_182226.md`
> (aarch64 · 18 cores · fsync-matched · `MM_CRUD_ROWS=100000`).
> Verified via `scripts/compare_bench.py <new_report>
> docs/performance/benchmark_20260716_182226.md`.

---

## 0. Baseline calibration — read this first (it is not what the headline says)

Two facts about the named benchmark file, established by direct comparison,
that change how the targets below must be read:

1. **`benchmark_20260716_182226.md` is a byte-identical copy of
   `multi_model_report_20260716_052432.md`** (verified with `cmp`). It is a
   100k-row sweep taken **before item 52 merged** — its `DELETE selected` row
   (0.05×, WAL 133 B/row, cols/row 6.00) predates the decode pushdown.
2. **Item 52's post-fix numbers (0.16×, WAL 114 B/row, cols/row 2.00) were
   measured only at `MM_CRUD_ROWS=10000`** (`multi_model_report_20260716_095901.md`).
   There is **no post-51/52 measurement at 100k rows anywhere in
   `docs/performance/`**. The 10k and 100k regimes differ materially
   (DELETE 0.16× vs 0.05×; grouped SELECT 0.46× vs 0.23×) because Postgres's
   throughput *rises* with scale on these ops while unidb's falls.

Hence **Step 0 (re-baseline) is mandatory** before any code change, and every
ratio target below is stated against the 100k-row regime of the named file.

### The losses, with root cause verified in code

| operation | ratio (100k file) | measured proof | root cause (verified, this engine's storage model) |
|-----------|------:|------|------|
| UPDATE bulk (k<N/2) | **0.05×** | WAL 530 B/row · dec/row 1.00 · cols/row 8.00 | Per-row write path: `exec_update` (`executor.rs:2366`) calls `heap.update` (`heap.rs:321`) once per row → one WAL mini-txn bracket (begin+commit ≈ 90 B), two page latch/fetch/`write_page` cycles, one FSM probe, one `lock_mgr` acquire, two `record_undo` calls **per row**. Insert-new-version MVCC makes the second page write intrinsic (D4), but nothing batches it. DELETE got exactly this batching in item 44; UPDATE never did. |
| DELETE selected (k>=N) | **0.05×** (0.16× at 10k post-52) | WAL 114 B/row at 10k · dec/row 0 | Item 52 removed the decode; item 44 batched mini-txns per page. What remains per row: one `log_update` WAL record = 41 B header + 8 B redo (xid) + 8 B undo (old xmax) + 4 B CRC = **61 B of framing for an 8-byte stamp** (`heap.rs:504-517`), plus one `lock_mgr.try_acquire_write`, one `ssi_note_write`, one `record_undo`. The old xmax is *provably 0* for every stamped row (the conflict check rejects otherwise), so the undo payload is pure ballast. FPI floor ≈ 62 B/row (8 KiB / ~133 rows/page) is the true residual. |
| SELECT grouped (GROUP BY g) | **0.23×** | cols/row 1.00 (item 46 mask works) | The item-46 path (`query_exec.rs:336-405`) is a **serial** `heap.scan` that additionally **materializes a `Vec<Vec<Literal>>` of all 200k rows** before calling `aggregate()`. The pre-spawned parallel worker pool (items 45/P, `parallel_scan.rs`) is never used for GROUP BY — only for `COUNT(*)` and filtered scans. Postgres runs this at 25.6M rows/s; our serial scan+alloc does 5.9M. |
| INSERT (per-row commit) | **0.43×** | WAL **8837 B/row** | NOT the fsync floor. `apply_durable_index_writes` → `DiskBTree::insert` → `log_index` (`wal.rs:586`, called from `btree_index.rs:1674`) logs a **full ~8 KiB leaf page image per statement** (WAL_INDEX redo = restore image, `recovery.rs:258`). With per-row statements that is one leaf image per row. 8237 B of the 8837 is this image. The backlog's standing note "per-row INSERT gap is structural WAL FPI, per §1 expected to lose" is only half-true: the heap FPI amortizes (~62 B/row), the **index image does not**. |
| SELECT filtered (k<N) | 0.53× (0.62× in 075853 run) | cols/row 4.00 | Owned by **item 54** (arena alloc) — explicitly **out of scope here**; do not double-plan it. |

Also out of scope: FK UPDATE 0.06× (**item 53**), event-queue small-table
anomaly (**item 55**), JOIN Phase B (**item 51**), HOT-equivalent version
chains (item 47 Phase C — requires reopening locked decision D4, per the
index's standing note).

---

## 1. Re-derived ROI order (first principles, not the draft order)

Ranking = (addressable fraction of the gap) × (confidence) ÷ (effort+risk).

1. **Step 1 — Parallel GROUP BY partial aggregation.** Read-only (lowest
   correctness risk in an MVCC engine), reuses two already-shipped mechanisms
   (worker pool + partial-aggregate pattern of `parallel_count_matching`),
   and the gap is 4.3×. Nothing about insert-new-version MVCC resists this —
   visibility filtering is already per-page in workers. Highest
   confidence-per-effort of the four.
2. **Step 2 — UPDATE write-path batching (`update_many`).** Largest write
   loss (20×). Mirrors item 44's shipped, crash-tested design. Bounded upside
   (insert-new-version + full index maintenance is structurally dearer than
   PG's HOT path — §1 honesty), but 3–5× is realistically addressable.
3. **Step 3 — DELETE batched xmax-stamp WAL record.** Item 52's own finding
   names WAL stamp framing as the floor. Smaller absolute win than Step 2 and
   it touches the recovery format, so it ranks below despite looking
   "obvious" — the WAL bytes are buffered (one fsync per statement), so the
   gain is CPU (record encode + mutex + per-row lock/undo), roughly 2×, not
   the 8× the byte count suggests. Do it in the same branch as Step 2 (shared
   design, shared crash points).
4. **Step 4 (gated) — logical B-tree index WAL records.** Biggest conceptual
   win (INSERT 8837→~700 B/row; also shrinks the Table-1 Δbtree ladder rung
   and every index-touching op), but it changes redo semantics — the riskiest
   change class this engine has (see item 50's history: the last critical bug
   lived exactly in batched-leaf code). Gated on a measurement step proving
   the index-image share, and explicitly NOT a prerequisite for Steps 1–3.

Draft plans that put "UPDATE first because the ratio is worst" get the ROI
wrong the same way Phase B's draft did (per CLAUDE.md §0.6.1): Step 1 has a
larger *expected shipped delta per unit of risk* than Step 2.

---

## 2. Steps

### Step 0 — Re-baseline at 100k (no code change)

`main` now has items 51 (PR #129/#130) and 52 (PR #131) merged; the named
benchmark predates both at 100k. Run `scripts/report.sh` (Docker, reachable
Postgres, defaults → `MM_CRUD_ROWS=100000`), promote via
`scripts/promote_bench.sh`, and record the post-51/52 100k ratios for
UPDATE/DELETE/grouped/INSERT. **Every target below is then re-checked against
this run** — if DELETE at 100k already reads ≥0.12× from item 52 alone,
Step 3's target tightens accordingly; state that in the PR rather than
banking item 52's win as this item's.

*Expected impact: none (measurement). Gate: one bench process only (`pkill`
strays first); compare with `scripts/compare_bench.py`.*

### Step 1 — Parallel GROUP BY partial aggregation

**Change** (`src/sql/query_exec.rs`, item-46 block at `query_exec.rs:336`;
`src/sql/parallel_scan.rs`):

- Extend the item-46 fast path (COUNT(*) GROUP BY simple column refs over a
  base `Scan`): acquire a worker lease (`parallel_scan::acquire`, existing
  governance — items 15/21), partition pages exactly as `parallel_count`
  does, and give each worker its own hash table keyed by the group column
  (`i64` fast path first, mirroring item 51's integer-key specialisation);
  merge the per-worker partials at the end.
- **Stream, don't materialize**: fold `deform_row`'s group-key directly into
  the worker-local table. The current `Vec<Vec<Literal>>` of every visible
  row (one heap alloc per row, `query_exec.rs:385-391`) disappears on this
  path even in the serial fallback (no lease → serial streaming fold).
- Fall through unchanged for: non-Column group exprs, non-COUNT aggregates,
  virtual tables, subqueries — same guards the block already has. (A
  follow-on can widen to SUM/MIN/MAX with per-worker accumulators; keep this
  step COUNT-only to match the benchmarked op.)

**Expected impact:** SELECT grouped 0.23× → **0.45–0.8×** (serial does 5.9M
rows/s; the parallel filtered path sustains ~9.5M rows/s *scanned* with a
4-column deform + predicate, and this path deforms 1 column with no
predicate; 18 workers, merge cost ≈ #groups × degree = 1800 entries —
negligible).

**Correctness constraints:**
- All workers share the statement's one `Snapshot` (same object the serial
  path uses). Per-version visibility (`xmin`/`xmax` vs snapshot) decides
  membership — a logical row whose old and new versions land in different
  workers' page ranges is counted once *by construction* (only one version
  is visible to one snapshot); do not add any cross-worker dedup, it would
  mask a visibility bug instead of preventing double-counting.
- D11 `on_read` seam / SSI read tracking: match whatever
  `parallel_count_matching` does today for reads on this path — parity with
  the serial path is the acceptance bar, verified by running the readers-
  during-churn + transfer-sum matrix cells (RC/RR/SER) unchanged.
- Group keys must compare identically to the serial path for NULL and
  negative values (NULL group = its own group; don't let an `i64` fast path
  fold NULL into 0 — the `Option<i64>` key or a sentinel-checked path).

**Pitfalls:** the worker pool is a shared singleton (`OnceLock`) — a panic
in a worker closure must not poison it for later queries (follow lever-2's
existing panic discipline). The lease can be denied (governor) — the serial
streaming fold must be the fallback, not the old materializing path.

### Step 2 — UPDATE write-path batching (`Heap::update_many`)

**Change** (`src/heap.rs`, `src/sql/executor.rs::exec_update`):

- New `Heap::update_many(rows: &[(RowId, Vec<u8>)], …) -> Vec<(RowId, RowId)>`
  mirroring `delete_many`'s shipped shape (`heap.rs:458`):
  - **Phase A (stamp):** group matched RowIds by `page_id` (already sorted by
    `matching_rows`' B5 sort). Per page: one mini-txn, one exclusive latch,
    conflict-check **all** slots (`xmax != 0` → abort + `WriteConflict`,
    unchanged semantics), one `maybe_log_fpi`, one `log_update` per row (Step
    3 replaces this with the batched record), one `write_page`, one
    `commit_mini_txn`.
  - **Phase B (insert new versions):** pack encoded new versions into fill
    pages via one `acquire_page_for_insert` per fill page — per fill page:
    one mini-txn, one FPI check, per-row `log_insert` (keeps per-row redo,
    and each carries its `prev` back-pointer to the old version), one
    `write_page`, one FSM note.
- `exec_update` restructure: run the whole per-row *compute* pass first
  (decode, eval assignments, coerce, NOT NULL, CHECK, CDC before/after
  capture, staging of index batches/patches), collect `(old_rid,
  encoded_new)`, then call `update_many`, then record per-row undo
  (`XmaxStamp` + `Insert`) and `ssi_note_write` from the returned pairs, then
  flush patch/index batches exactly as today.
- **Gate (mandatory, per §0.6.5):** take the batched path **only when**
  `!has_unique && !has_fk_refs && !has_fk_children` — otherwise keep the
  existing per-row loop verbatim. Reason below (in-statement uniqueness
  visibility). The Table-3 bench table (`t(id,k,g,body)`, secondary index
  only) qualifies; Table 5's FK table intentionally does not (that gap is
  item 53's).

**Expected impact:** UPDATE bulk 0.05× → **0.12–0.22×** (removes per-row
mini-txn brackets ≈ 90 WAL B + two of the two page-cycle costs become
per-page; WAL 530 → ~300 B/row before Step 3, ~250 after). Honest ceiling:
Postgres serves this op with HOT (body is unindexed → no index maintenance,
same-page tuple) — reaching parity requires item 47 Phase C (D4 sign-off),
out of scope. If measurement shows <0.10× after batching, escalate per
§0.6.6 with the profile rather than grinding.

**Correctness constraints (insert-new-version MVCC — the sharp edges):**
- **Never defer heap writes past a unique/FK check.** With the per-row loop,
  `enforce_unique` on row *n* can see row *n−1*'s freshly inserted new
  version (own-xid visible); batching heap writes after all checks would let
  `UPDATE t SET u = 5` pass two rows to the same unique key. That is why the
  gate above is a correctness gate, not a tuning knob — do not "widen" it to
  unique tables without an in-statement key-set check, and if that is ever
  added, it must also cover the FK RESTRICT re-check ordering
  (phantom lock → fresh snapshot → check; `executor.rs:2380-2443`).
- **The B-tree is the only forward resolver** (locked lesson, CLAUDE.md
  §0.6.2): every new version must get its index entries — changed keys via
  `flush_index_batches` inserts, unchanged keys via item 47's `patch_many`
  RowId patch. `update_many` must not create any path where a new RowId
  reaches the heap without reaching `stage_row_index_writes_update`.
- **Conflict-check under the latch, immediately before the stamp** — the
  current `heap.update` checks `xmax != 0` after latching (`heap.rs:340-348`);
  the page-group version must conflict-check all slots *after* acquiring the
  page latch (as `delete_many` does), never from the pre-pass (TOCTOU).
- **One physical page latch at a time** (deadlock discipline,
  `heap.rs:369-371`): Phase A holds only the old page's latch; Phase B only
  the fill page's. Never interleave.
- **Statement atomicity across mini-txns is unchanged in kind**: today a
  50k-row UPDATE is already 50k mini-txns; a crash mid-statement is healed by
  recovery's incomplete-user-txn undo walking the per-row undo records. The
  two-phase shape (all stamps, then all inserts) must record undo actions in
  an order the abort path can replay: `XmaxStamp` undos for Phase A rows must
  be recorded even if Phase B never ran (record Phase A undos as soon as
  `update_many` returns per-page progress — return partial results on error,
  or record undo inside the loop via a callback; pick one and crash-test it).
- **CDC ordering** (item 29): before-images are captured in the compute pass
  (pre-mutation) — preserved by construction; events must still be emitted
  once per row with both images.
- **D5** on every batched page write: FPI + all row records precede
  `write_page` within the mini-txn — same bracket `delete_many` uses.

**Pitfalls:** item 50 is the cautionary tale — the last critical bug was in
exactly this kind of batched page-group loop (`patch_many`'s bounds check
never advancing `i`). Any grouping loop here needs a
guaranteed-progress argument in review and a regression test that crosses a
page-split / page-boundary condition, with a hang deadline
(`mpsc::recv_timeout` pattern from `tests/patch_many_leaf_bounds_regression.rs`).

### Step 3 — DELETE (and UPDATE Phase A) batched xmax-stamp WAL record

**Change** (`src/wal.rs`, `src/format.rs`, `src/heap.rs::delete_many`,
`src/recovery.rs`, `unidb-logical`):

- New additive record type `WAL_XMAX_BATCH`: one record per (page,
  mini-txn) carrying `xid` once + a slot array. Redo: stamp `xmax = xid` on
  every listed slot (LSN-gated, idempotent). Undo: reset the listed slots'
  xmax to 0 — valid because the conflict check guarantees every stamped
  slot's prior xmax was exactly 0 (assert this at build time in debug; the
  per-slot old-xmax payload of today's records is provably dead weight).
- `delete_many` emits one `WAL_XMAX_BATCH` per page group instead of N
  `log_update`s; `update_many` Phase A (Step 2) uses the same record.
- Batch the bookkeeping around it: one `lock_mgr` pass that takes the mutex
  once for the whole statement (add a `try_acquire_write_many`), and a
  vectored `record_undo`/`ssi_note_write` (`Vec`-append instead of per-call
  overhead) — the WAL bytes are buffered ahead of one statement fsync, so
  the *CPU* around each record is the real cost being removed.
- Recovery: redo + undo arms for the new type in `recovery.rs` (both the
  committed-mini-txn redo pass and the incomplete-user-txn undo pass at
  `recovery.rs:140/193` must classify it alongside `WAL_UPDATE`).
- **`unidb-logical` (item 28 R2) and CDC replay decode the WAL stream** —
  the new record must decode into per-row delete/update events there too, or
  be explicitly rejected with a clear error. Grep every `rec_type` match
  arm; a silently-skipped record type is a replication data-loss bug, not a
  perf bug.

**Expected impact:** DELETE selected WAL 114 → ~70 B/row (FPI floor);
throughput +50–100% at 10k (0.16× → **0.25–0.35×**); at the named 100k
baseline 0.05× → **≥0.15×** combined with item 52 (already merged) and the
lock/undo batching. UPDATE gains ride Step 2's line.

**Correctness constraints:**
- WAL format is additive (no page-format change, no D8/D9 impact) but **an
  older binary cannot replay a newer WAL**: `decode_record` must fail loudly
  (not skip) on unknown `rec_type`, and the change note goes in
  `docs/design/engine_design.md`'s format section. No FORMAT_VERSION bump is
  strictly required (control-file format untouched) — but record the
  decision + sign-off in PROGRESS.md since it touches recovery truth (D3
  adjacency).
- Crash-injection (D7 — mandatory, storage touched): new points (a) after
  `WAL_XMAX_BATCH` append before `write_page`, (b) between two page groups
  of one statement, (c) during recovery undo of a batched record. Assert:
  committed statements' deletes all survive; an incomplete statement leaves
  **every** listed slot live again (a half-undone batch record = corruption).

### Step 4 (gated) — logical B-tree index insert WAL records

**Gate first, code second** (§0.6.4): add a one-run decomposition to the C1
counters (WAL bytes by record type per op) and prove on the Step-0 baseline
that WAL_INDEX images are ≥80% of per-row INSERT's 8837 B/row and a
measurable share of the Table-1 Δbtree rung (+0.14–0.22 ms). If yes, file
the design; implement only after Steps 1–3 ship.

**Design sketch** (for the follow-on doc; recorded here so the gate has a
shape to price): new `WAL_INDEX_INSERT` logical record (key + RowId +
meta_page, ~70 B); leaf torn-page safety via `maybe_log_fpi` on index pages
(the mechanism at `bufferpool.rs:454` is page-kind-agnostic already); redo
re-executes the leaf insert, LSN-gated; **splits fall back to full images of
every structurally-changed page** (rare, amortized); redo-only — no undo
arm, because index entries for aborted transactions are already tolerated
and filtered by heap visibility, then scrubbed by vacuum (`get_raw` scrub
window, `heap.rs:300-308`). `insert_many`/`patch_many` keep image logging
initially (they already amortize to ~1 image/leaf/statement).

**Expected impact:** INSERT per-row WAL 8837 → ~700 B/row; ratio 0.43× →
**0.60–0.80×** (both engines then ride the same fsync floor); Δbtree ladder
rung shrinks, directly improving the W1–W4 headline story (§1 — this is the
one step that serves the *thesis* benchmark, not just Table 3).

**Correctness constraints:** redo of a logical insert against a leaf whose
image predates the record requires the FPI-first-touch invariant to hold for
index pages exactly as for heap pages (D5 + P1.a extended); recovery must
replay index records in LSN order interleaved with heap records (today's
image-restore is order-tolerant; logical redo is not — this is the risk that
prices the step). Crash points: mid-split, between logical record and leaf
write. Honest fallback: if the split/interleave analysis exceeds scope,
ship "image-per-leaf-per-*user-txn*" (dedup images across the statement's
mini-txns) as a cheaper 80% variant with zero redo-semantics change.

---

## 3. Acceptance criteria

Verified by generating a full report (`scripts/report.sh`, Docker, reachable
Postgres, default 100k CRUD rows) and running
`scripts/compare_bench.py <new_report> docs/performance/benchmark_20260716_182226.md`.
All ratios are Table-3 `unidb ÷ PG` at 100k unless stated.

| # | criterion | target (accept) | stretch |
|---|-----------|-----------------|---------|
| A1 | Step 0 re-baseline promoted to `docs/performance/` and cited in the PR | done | — |
| A2 | SELECT grouped (GROUP BY g) | **≥0.45×** (from 0.23×) | 0.70× |
| A3 | UPDATE bulk (k<N/2) | **≥0.12×** (from 0.05×) | 0.20× |
| A4 | DELETE selected (k>=N) | **≥0.15×** (from 0.05×; ≥0.25× at 10k vs 095901's 0.16×) | 0.25× |
| A5 | UPDATE WAL B/row | **≤320** (from 530) | ≤260 |
| A6 | DELETE WAL B/row (10k lens) | **≤80** (from 114) | ≤70 |
| A7 | No regression: SELECT COUNT(*) ≥5.0×, DELETE all ≥5.0×, SELECT filtered ≥0.50×, INSERT ≥0.40×, W4/W0 at 100k ≤2.3× | all hold | — |
| A8 | Concurrency matrix 32/32 PASS; crash harness green incl. new Step-2/3 injection points | mandatory | — |
| A9 | Step 4 gate: WAL-by-record-type decomposition published in-report | done | INSERT ≥0.60× if Step 4 ships |

Per CONVENTIONS.md: if a target proves architecturally unreachable, revise
the acceptance line **honestly with a dated inline note + evidence** (as
`crud_performance.md` did) — never chase a lucky run.

## 4. Non-goals / do-not-touch

- No re-litigation of D4 (tuple format / HOT chains), D5, D2, D8/D9. Step 3's
  new WAL record is additive within the existing framing.
- No overlap with items 51 (JOIN), 53 (FK UPDATE), 54 (filtered arena),
  55 (event queue) — distinct code paths; this item can run in parallel with
  all of them except that Step 1 and item 54 both touch scan-adjacent code in
  `parallel_scan.rs`/`query_exec.rs` — coordinate branches if simultaneous.
- Per-row INSERT is **not** headlined as a competitive claim even if Step 4
  ships (§6: single-model vs specialized incumbent is not the thesis); it is
  justified here by the Δbtree ladder + UPDATE/bulk-load spillover.
