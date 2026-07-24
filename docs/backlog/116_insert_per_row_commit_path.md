# Item 116 — Per-row INSERT commit path: attribution + software-cost reduction

**Type:** Performance
**Status:** 🔄 IN PROGRESS — Step-0 + small levers + durability hardening shipped
2026-07-24 (PR #210); Docker cert: **0.50× unchanged** (4,093 rec/s, WAL 584 B/row,
canary quiet — the µs-levers were hygiene as measured natively, and the durability
fix cost nothing). Target ≥0.75× rides on the structural unit below (statement-
scoped mini-txn bracket merge), designed and NOT yet implemented.

**Target:** Table 3 `INSERT (per-row commit)` ratio ≥ **0.75×** vs PG (user-set
2026-07-24; 0.50× in the 07-23 baseline, unidb ~230 µs/row vs PG ~114). Honest
escalation clause: if Docker fsync share proves the floor makes 0.75
unreachable without batching, present the evidence and revise (§0.6 rule 6).

## Step-0 attribution (probe: `tests/perf_item116.rs`, native, 100k pre-loaded)

**Fsync-count finding first:** the engine DEFAULT is commit-time fsync
(`wal.set_deferred_sync(true)` at open — ONE group-coalesced fsync per commit
via `Engine::commit`'s `sync_up_to`). An earlier draft of this analysis flipped
the probe to the legacy per-statement mode (`set_deferred_sync(false)`) and
"discovered" 3 fsyncs/row — that mode is harness-only; nothing ships with it.
The probe now measures the default and confirms **exactly 1 fsync per commit**.

Software split per row (macOS F_FULLFSYNC subtracted via the WAL fsync
histogram; 2,000-row sample, WAL 578 B/row):

| phase | µs |
|---|---:|
| begin (snapshot + WAL_TXN_BEGIN) | 1.5 |
| execute (heap insert + B-tree + their WAL) | ~70 |
| commit: txn_mgr (undo drop + TXN_COMMIT append + locks) | 1.3 |
| commit: sync_up_to software (leader coordination, above raw fsync) | ~24-45 |
| commit: post (timeline mark, row-count deltas, wake) | 0.8 |
| **software total** | **~117** |

Permanent `Q116_*` commit-phase timers added (txn_mgr / sync / post).

## Shipped in this PR (small, safe levers)

- `Heap::find_or_alloc_page`: stop collecting the ENTIRE page list as a Vec on
  every first-insert-of-statement (free_map starts empty for a per-statement
  heap → the old `filter().collect()` allocated+copied ~11 KB/row at 100k);
  only the single newest unknown page — the only one ever probed — is found.
- `Wal::group_fsync`: cache the duplicate FD per segment instead of a
  `try_clone` (dup syscall + alloc) per commit; invalidated on rotation.
- **Durability hardening (catalog):** `Catalog::persist` now explicitly
  `sync_up_to(commit_lsn)` BEFORE flipping `catalog_root` in the control file.
  Under the commit-time-fsync default, the persist mini-txn was not durable at
  flip time — the control file could reference pages whose log could vanish in
  a crash. Closes a latent hole; no-op cost when already durable.

Native effect of the two µs-levers: within noise on macOS (software ~117
before/after) — kept for their allocation/syscall hygiene; the honest gain
must come from the structural unit. The Docker cert records where the ratio
stands now.

## Next unit (designed, NOT yet implemented): statement-scoped mini-txn bracket

Today one INSERT statement opens TWO WAL mini-txn brackets (heap via
`insert_accumulating`, index via `apply_durable_index_writes`) ≈ 8-10 WAL
appends/row (578 B), each paying append-mutex + CRC + framing. D2 defines the
statement as the atomic unit — heap and index writes of one statement can
share ONE bracket:

- Thread the statement's `(txn_id, last_lsn)` from the heap accum into the
  index batch apply instead of opening a fresh bracket there.
- Recovery semantics IMPROVE: heap row + index entry redo/undo become atomic
  together (today they are two brackets that recovery treats independently —
  correct but subtle).
- Est. −2 records and one bracket's bookkeeping per row (~10-15 µs software),
  → software ~100 µs; with Linux fsync ~40-70 µs → **~140-170 µs/row ≈
  0.67-0.81×**. The cert after THAT unit decides whether 0.75 is met or the
  escalation clause fires.
- Gate: full crash harness (its p-points cover exactly this bracket
  structure) + item-105 selective Table-3 re-cert. Touches WAL/heap/executor —
  no carry-forward stitching allowed for its bench.

## Context for readers

Per-row single-writer INSERT is the engine's documented worst case: the
design point is group-commit coalescing (many writers share one fsync) and
batched statements (Table 3.1 bulk INSERT is at PG parity). This item narrows
the worst case; it does not redefine it.
