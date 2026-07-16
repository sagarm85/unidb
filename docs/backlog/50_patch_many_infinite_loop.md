# `DiskBTree::patch_many` infinite loop on unchanged-key UPDATE at scale

**Type:** Improvement
**Status:** SHIPPED — see `PROGRESS.md` ("Item 50 — patch_many infinite loop
fix") for measured numbers.
**Priority:** Critical — this is a real, single-threaded, CPU-burning infinite
loop reachable from ordinary SQL (`UPDATE ... WHERE <non-key-column-predicate>`
on any indexed column, at scale), not just a benchmark artifact.

## Problem

While re-verifying item 49's Postgres connect-timeout fix with a real,
reachable Postgres server, `scripts/report.sh`'s Table 3 (`UPDATE t SET body =
'updated' WHERE k < 5000` on a 10,000-row table with `CREATE INDEX t_k ON t
USING BTREE (k)`) hung for 29+ minutes at 100% CPU on a single thread — not
blocked/sleeping (a lock wait would show as `S` state), genuinely spinning.
`gdb -p <pid> -batch -ex bt` taken twice, seconds apart, showed the **identical
stack** both times:

```
#0  syscall ()
#1  <unidb::bufferpool::ExclusiveLatch as Drop>::drop ()
#2  unidb::btree_index::DiskBTree::patch_many ()
#3  unidb::sql::executor::flush_patch_batches ()
#4  unidb::sql::executor::execute ()
```

— stuck inside item 47's `patch_many` (`src/btree_index.rs`), repeatedly
acquiring and dropping the same leaf latch.

This is why Table 3 (and its UPDATE row) had never been exercised in this
session's earlier "successful" reports: `bench_mm_report`'s entire Table 3
block is gated on `pg_method.is_some()` (i.e. `PG_URL` set **and** a live
Postgres reachable) — every report generated without a working Postgres
skipped it silently, including every report generated in this project before
today that didn't have `PG_URL` pointed at a reachable server. Combined with
item 49 (no `connect_timeout`, so an unreachable `PG_URL` also hung), enabling
the Postgres comparison — the whole point of setting `PG_URL` — was reliably
going to hang one way or the other. This item is very likely the dominant
real-world cause of "`scripts/report.sh` running in indefinite mode."

## Root cause

`DiskBTree::patch_many` (`src/btree_index.rs`) groups a sorted batch of
`(key, old_rid, new_rid)` patches by leaf:

```rust
let mut j = i;
while j < sorted.len() {
    let (ref pk, pold, pnew) = sorted[j];
    if pk < &min_key || pk > &max_key {   // min/max = THIS leaf's *current* entries
        break;
    }
    match entries.iter_mut().find(|(k, r)| k == pk && *r == pold) {
        Some((_, rid)) => { *rid = pnew; modified = true; }
        None => fallbacks.push((pk.clone(), pnew)),
    }
    j += 1;
}
...
i = j;   // <-- if the break fired on the FIRST iteration (j == i), i == j: no progress
```

`min_key`/`max_key` are read from `entries.first()/last()` — the leaf's
**current, actual** entries — not the leaf's **structural** key range (the
span the parent's separator keys route to it). Those are not the same thing:
a B-tree leaf's live entries do not have to span its full structural range
(most visibly right after a split, but the general invariant is simply not
guaranteed). `find_leaf(&sorted[i].0)` correctly routes to the leaf that
*should* hold `sorted[i].0` per the tree structure, but if that leaf's live
entries happen to start after `sorted[i].0` (or end before it), the very first
loop iteration (`j == i`) hits the bounds check and `break`s **before `j` ever
increments**. `i = j` is then a no-op, and the outer `while i < sorted.len()`
loop repeats the exact same `find_leaf` → same leaf → same bounds miss →
`break` forever, pinned to one CPU, no lock contention (single-threaded),
zero output.

The bounds check itself is not wrong in intent (it's what lets *additional*
patches in the sorted batch piggyback onto the same leaf lookup instead of
each doing its own `find_leaf` — the whole point of batching) — it was simply
also (incorrectly) gating whether the entry that got us here (`j == i`) gets
processed at all.

## Fix

Restructured the loop so the bounds check can only terminate **additional**
(`j > i`) batching, never the first entry:

```rust
let mut j = i;
loop {
    let (ref pk, pold, pnew) = sorted[j];
    if j > i && (pk < &min_key || pk > &max_key) {
        break;
    }
    match entries.iter_mut().find(|(k, r)| k == pk && *r == pold) {
        Some((_, rid)) => { *rid = pnew; modified = true; }
        None => fallbacks.push((pk.clone(), pnew)),
    }
    j += 1;
    if j >= sorted.len() { break; }
}
```

`j == i` is now unconditionally processed on every outer-loop iteration
(falling back to the existing `insert_in_txn` path — exactly the same fallback
already used for any other not-found entry — if the exact `(key, old_rid)`
isn't in this leaf), so `j` always ends up `> i` and `i = j` always advances.
No behavior change for the case the bounds check was actually meant to serve
(deciding whether entry `j+1, j+2, ...` also belong to this leaf).

## Verification

- New permanent regression test,
  `tests/patch_many_leaf_bounds_regression.rs`: builds a 10,000-row table
  indexed on `k` (forcing B-tree leaf splits), runs `UPDATE t SET body = ...
  WHERE k < 5000` (the exact shape that hung) on a background thread with a
  30s deadline (`mpsc::Receiver::recv_timeout`, the same hang-detection
  pattern `benches/conc_matrix.rs` already uses) so a regression fails the
  test cleanly instead of hanging CI.
  - **Confirmed the test catches the bug**: temporarily reverted the fix
    (`git stash` on `src/btree_index.rs` alone) and re-ran — the test failed
    at exactly the 30s deadline with `HANG: ... DiskBTree::patch_many
    infinite-loop regression`. Restored the fix — test passes in ~1s.
- Full `scripts/report.sh` re-run (`MM_SIZES=1000,10000
  MM_BULK_SIZES=1000,10000 MM_CRUD_ROWS=10000 MM_FK_ORDERS=10000
  MM_TX_SWEEP=1000,10000`, real local Postgres 16, matched `fsync`/`fsync`
  durability lens): completes end to end, Table 3's UPDATE row included with
  real numbers, 32/32 concurrency-matrix PASS. See
  `docs/performance/multi_model_report_20260716_005004.md`.
- `cargo test --release --test crash` — **38/38**.
- `cargo test --release` (workspace, default features) — **407 lib/bin tests +
  all integration suites green** (also fixed a pre-existing, unrelated gap
  found while running this gate: `tests/server_observability.rs` was missing
  its `required-features = ["server"]` `[[test]]` registration in
  `Cargo.toml`, so cargo auto-discovered and tried to compile it
  unconditionally, breaking plain `cargo test` for anyone — added the missing
  registration).
- `cargo clippy --release -- -D warnings` — clean.
- `cargo fmt --all --check` — clean.

No on-disk format, WAL record, or catalog change — `patch_many`'s WAL output
and undo entries are unchanged; only the in-memory grouping loop's control
flow is fixed. No `FORMAT_VERSION` bump.

## Also discovered while running the full verification gate (separate, pre-existing, NOT fixed here)

`cargo test --release --features server` has one failing test,
`slow_query_captured_after_threshold_set`
(`tests/server_observability.rs`), independent of anything in this item —
confirmed by `git stash`-ing this item's changes and re-running it in
isolation on the unmodified tree: fails identically. Sets
`threshold_ms=1` (the endpoint's unit is **milliseconds**) and asserts a
`CREATE TABLE ...; INSERT ...` shows up in `recent_slow_queries`, but the
test's own comment claims "the engine records all queries ≥ 1 µs as
slow" — a 1000x unit mismatch between the comment and the actual
`PUT /config/slow_query_threshold_ms` semantics. Plausible cause: that
combined statement now completes in under 1ms on this hardware (several
independent perf items landed between when this test was written,
2026-07-13, and now: 43, 45/46/48, 47/44), so it no longer clears its own
threshold. Not investigated further or fixed — out of scope for this item;
flagging so it isn't silently swept under "all tests green." Also notable:
this test was **not part of the routine verification loop at all** until
this item's `Cargo.toml` fix (the missing `[[test]]` registration, above)
made `tests/server_observability.rs` a real, always-compiled target — before
that, whether it ran depended on incidental auto-discovery behavior, so this
failure may have been silently invisible for a while.

## Also discovered while scoping the report (not a bug, a config-surface note)

`benches/decompose.rs`'s multi-model report has **five independent row-count
knobs** that must all be set to scope a report down: `MM_SIZES` (Tables 1/2),
`MM_BULK_SIZES` (Table 3.1), `MM_CRUD_ROWS` (Table 3, default 100,000 —
single value, not a sweep), `MM_FK_ORDERS` (Table 5, default 20,000), and
`MM_TX_SWEEP` (Table 4, default `1000,10000,100000,1000000`). None share a
default or a single override. Not fixed here (no bug, just friction observed
while producing a same-day report) — worth a follow-up to unify them if this
keeps causing under-scoped runs.

## Depends on / builds on

- Item 47 (`47_update_delete_write_throughput.md`) — introduced `patch_many`'s
  batched-by-leaf grouping; this item fixes a control-flow bug in that new
  code, found the day after it merged.
- Item 49 (`49_bench_pg_connect_no_timeout_hang.md`) — the connect-timeout
  investigation that led directly to this finding (Table 3 only runs, and
  therefore this code path only executes, when Postgres is actually reachable
  — exercising the fix for item 49 is what surfaced item 50).
