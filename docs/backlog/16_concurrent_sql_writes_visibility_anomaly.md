# MVCC visibility anomaly under concurrent SQL writes (root-cause + fix)

**Type:** Improvement
**Status:** ✅ SHIPPED (2026-07-12, branch `16-visibility-fix`) — root cause below;
metrics in `PROGRESS.md` "MVCC visibility anomaly under concurrent SQL writes".

> **Naming note (why "#16" appears in several places).** This is backlog item
> **16** in [`backlog_index.md`](backlog_index.md). It is **unrelated to PR
> #45** — that PR shipped item **17** (`17_mm_replaced_stack_headline.md`);
> its PR *body* and its `item16_*` crash-test names are stale labels from
> before that work was renumbered 16 → 17. Everything about the anomaly
> itself lives here and in the cross-references below.

## Symptom (what a user would see)

Under concurrent load, a SQL scan that overlaps a committing cross-row
UPDATE transaction can return a **wrong result set**:

- an **extra row version** — a superseded version visible alongside its
  successor (the original sighting: `SELECT` returning **3 rows instead
  of 2**; also observed as `COUNT(*)` = 9 on an 8-row table), or
- a **missing live row** — a short scan / torn read (`COUNT(*)` = 7 on an
  8-row table; a bank-transfer reader seeing 7 accounts summing 699 instead
  of 8 summing 800), and
- after vacuum races cross-row churn, **duplicate visible ids that persist
  at quiescence** (still there after writers stop and a final `vacuum()`) —
  corruption, not just a stale read.

## Discovery & evidence timeline

1. **2026-07-11** (item-12 verification): `tests/concurrent_writers.rs::
   cross_row_update_deadlock_resolves_no_hang` (toggle **on**) intermittently
   ended with 3 visible rows instead of 2 — only under CPU contention (6
   parallel test-binary instances), never in isolation. Reproduced on
   unmodified `main` @ `dc93931`. Initially believed confined to the
   `UNIDB_CONCURRENT_SQL_WRITES` toggle path.
2. **2026-07-12** (concurrency correctness matrix, PR #46,
   `benches/conc_matrix.rs`, run via `scripts/report.sh --conc`): the anomaly
   family is **NOT gated on the toggle** — the production default (off) is
   affected. Release build, native macOS (M5 Pro, 18 cores), `main` @
   `0c09a70`:

   | shape | toggle | observed |
   |---|---|---|
   | readers-during-churn, RC reader | **off** | FAIL 2/3 — `COUNT(*)`=7, expected 8 |
   | transfer-sum, RC reader | **off** | FAIL 1/3 (7/10 focused) — 7 accounts / sum 699 |
   | vacuum × cross-row churn | **off** | 3/10 focused — duplicates persist post-quiescence |
   | cross-row churn 8w×8rows | **off** | 1/6 focused — post-quiescence duplicate ids |
   | cross-row churn 2w×2rows (original geometry) | on | FAIL 2/3 with spinners (6/6 PASS without — why the shipped test looked reliable) |
   | cross-row churn 8w×8rows, indexed **and unindexed** | on | FAIL 3/3 — duplicate ids |
   | readers-during-churn RC/RR/SER | on | FAIL 2–3/3 — `COUNT(*)`=9 or a live row missing |
   | parallel-scan-path reader | on | FAIL 1/3 — 1 row missing from a 3000-row scan |
   | 8w indexed churn (one focused run) | on | `Recovery("D5 violation on flush: page LSN > durable WAL LSN")` at commit |
   | 8w indexed churn (one full-matrix run) | on | hang > 120 s (deadlock/livelock) |

   Official full-matrix run: **17 PASS · 11 FAIL of 28 cells** (3
   repeats/cell, 18 contention spinners).

## Three symptom classes to explain (may be 1–3 distinct bugs)

1. **MVCC visibility around commit-during-scan** (the family core): a scan
   concurrent with a multi-statement cross-row UPDATE commit either sees the
   superseded version too, or misses the live row. Fails **unindexed** as
   well ⇒ the Item-A crabbing B-tree is not the (only) culprit. The
   persistent post-vacuum duplicates suggest the wrong version can also
   survive reclamation.
2. **D5 flush-ordering violation** at commit (locked invariant, §3 D5) —
   possibly a separate bug in the eviction-forced-sync path under the
   concurrent-writes toggle.
3. **Deadlock/livelock hang** (> 120 s) under heavy contention with the
   toggle on — the wait-for-graph detector failed to break or a livelock
   formed.

## Repro (deterministic enough to bisect)

```bash
scripts/report.sh --conc                                   # full 28-cell matrix (~5–10 min)
CONC_ONLY=readers-during CONC_REPEATS=10 scripts/report.sh --conc   # fastest loop, toggle-on 3/3
CONC_ONLY=transfer      CONC_REPEATS=10 scripts/report.sh --conc    # toggle-OFF 7/10
CONC_ONLY=vacuum-churn  CONC_REPEATS=10 scripts/report.sh --conc    # persistent-corruption case
```

## Root cause (2026-07-12) — abort dropped the xid from `active` before undo

All three symptom classes share **one** cause: `TransactionManager::abort`
(`src/txn.rs`) removed the aborting xid from the `active` set **before** it
physically reversed the transaction's heap mutations (and before releasing its
row locks):

```
let txn = self.lock().active.remove(&xid)…;   // (1) xid no longer "active"
for action in txn.undo_log.iter().rev() { heap.undo_* }   // (2) heap still holds its writes
…
lock_mgr.release_all(xid);                     // (last) locks freed
```

Visibility has **no "aborted" state** by design: `mvcc::is_committed_at_snapshot`
treats any xid that is *not in `active`* and *below `next_xid`* as **committed**
(the `mvcc.rs` header states this is sound only because an aborted txn's writes
are physically undone). Step (1) breaks that premise: between (1) and the end of
(2) the aborting xid is neither active nor undone, so a concurrent snapshot
classifies its still-present writes as committed —

- its new UPDATE version (`xmin` = aborting xid) becomes **visible**, and
- the old version it superseded (`xmax` = aborting xid) becomes **invisible**.

A concurrent reader therefore sees the doomed version — a wrong `COUNT(*)`, an
extra id, or (when it sees the hidden old + not-yet-inserted new) a missing id.
Worse for durability: the new version's `RowId` is **not** locked (`heap.update`
only locks the *old* version), so a concurrent writer can supersede it and build
a fresh version chain on top — after which undo reverts the old version to live,
leaving **two live versions of one logical row** (the persistent post-vacuum
duplicate) or, symmetrically, **none** (the persistent missing row).

- **Symptom class 1** (visibility / duplicate / missing) is this directly.
- **Symptom class 2** (D5 "page LSN > durable WAL LSN" on flush) and
  **class 3** (>120 s hang) were **downstream** of the corruption, not separate
  bugs: they did **not** reproduce across the full matrix at `CONC_REPEATS=10`
  once the ordering was fixed (28/28 PASS, toggle off *and* on, 18 spinners).
  **No §3 locked decision (D5 especially) was reopened.**

**Fix (single-site, `src/txn.rs::abort`).** Read the undo actions while the xid
is still in `active`, run the physical undo + WAL abort, and only *then* remove
the xid from `active` / mark it aborted / `release_all`. The whole rollback is
now atomic to every other snapshot: they see the pre-abort committed state
throughout, then the restored state — never the half-undone middle. Toggle-off
behavior is unchanged except for this ordering; no on-disk format change; the
crash harness is untouched (recovery's single-threaded undo was never exposed).

**Evidence (the failing interleaving, not a story).**
- `src/txn.rs::aborting_txn_new_version_never_visible_to_concurrent_snapshot` —
  a deterministic unit test that pins an observer scan to the exact abort
  midpoint via a barrier. Pre-fix it reads the doomed `"v2"`; post-fix `"v1"`.
- `tests/concurrent_writers.rs::item16_readers_during_cross_row_churn_{off,on}`
  — the 8w×8rows + 2-reader geometry. Fails pre-fix **without** external CPU
  load (observed `reader snapshot lost/gained a live row`, `COUNT(*)
  disagrees`, and a >90 s hang); passes post-fix, standalone, repeatedly.
- `benches/conc_matrix.rs` via `scripts/report.sh --conc`: **17 PASS/11 FAIL →
  28 PASS/0 FAIL** at `CONC_REPEATS=10`.

## Definition of done

- Root cause identified and written up here (dated, inline — per
  `CONVENTIONS.md` corrections rule) for **each** of the three symptom
  classes (or evidence they share one cause).
- Fix lands with the full concurrency matrix **green at
  `CONC_REPEATS≥10` with spinners**, toggle off AND on; crash harness green;
  no §3 decision reopened without recorded sign-off (D5 especially).
- `tests/concurrent_writers.rs` gains a geometry that fails pre-fix without
  external load (the matrix showed 8w×8rows + readers is that geometry).
- Only then may item 11's planned `UNIDB_CONCURRENT_SQL_WRITES` default-ON
  flip proceed (it stays **default-off** until this ships). The toggle-off
  findings mean the fix is a production-correctness item, not just a
  flip-blocker. ✅ **This fix SHIPPED (PR #50); item 11's default-ON flip then
  proceeded and SHIPPED 2026-07-13** — 28/28 matrix at `CONC_REPEATS=10`, Table C
  811 → 1016 commits/s (see `index_write_concurrency.md` flip note + `PROGRESS.md`).

## Cross-references

- `index_write_concurrency.md` — "Known issue found post-ship" section
  (original sighting + matrix results tables; the toggle's design and
  validation strategy).
- `benches/conc_matrix.rs` + `scripts/report.sh --conc` (PR #46) — the
  reproducer/regression harness; report lands in
  `docs/performance/conc_matrix_<ts>.md` (committed — the dated reports under
  `docs/performance/` are the durable measurement record).
- `PROGRESS.md` "Index & heap write concurrency" — the item-11 unit whose
  toggle path first exposed the family.
