# SELECT filtered remaining gap: serial B-tree range scan + per-query thread spawn + row materialisation

**Type:** Performance
**Status:** NOT STARTED
**Priority:** High — after items 43 + 46 + 48 the gap has narrowed to 0.63× PG (was 0.35×); thread-spawn overhead is the biggest remaining lever and is the recommended entry point.
**Recommended entry point:** Lever 2 (pre-spawned worker pool) first — eliminates the ~900 µs fixed per-query spawn tax without touching the B-tree or row-encoding layers. Lever 1 and 3 are follow-ons once lever 2 is measured.

---

## Measured gap (updated 2026-07-16 post items 43 + 46 + 48, MM_CRUD_ROWS=20000)

| engine | rec/s | ratio |
|---|---:|---|
| unidb | 4 066 108 | 0.63× |
| Postgres | 6 502 039 | — |

_(Earlier measurement 2026-07-15 pre-item-43: unidb 2,595,212 vs PG 7,316,962 = 0.35×. The improvement is from item 43's correct predicate-arm selection + item 46's cols/row reduction on the aggregate path; the filtered-read path itself was not changed.)_

`cols/row = 4.00` confirms the correct selective arm (`k < N`) is being picked and the parallel path fires. The deficit is structural: thread-spawn overhead is the dominant remaining cost.

## Root causes (three independent levers)

### 1. Serial B-tree range scan before any parallelism
`try_exec_select_btree` calls `tree.search_range(Lt, &N, pool)` serially to collect all 20 k `RowId`s before the first worker starts. This serial phase touches ~150 B-tree leaf pages (one `Node::deserialize` per page = one `Vec` alloc + decode loop), and none of it overlaps with the parallel heap fetch.

**Fix:** Partition the key range across workers — each worker gets a sub-range `[lo, hi)`, descends the tree independently, and produces its own candidate slice. Requires a `search_range_partition(n: usize)` API on `DiskBTree` that returns `n` approximately equal key-range slices by walking internal node separators. Workers then merge results (order is unspecified without `ORDER BY`).

### 2. Fresh `std::thread::scope` per query (thread-spawn tax)
`parallel_resolve_candidates` spawns `degree` OS threads on every call via `std::thread::scope`. At 18 workers, thread creation costs ~50 µs/thread × 18 = ~900 µs fixed overhead per SELECT — a floor independent of how many candidates there are.

**Fix:** Replace the per-call `std::thread::scope` with a pre-spawned worker pool (a ring of `degree` parked threads, woken via channel or futex). Workers park between queries; the query posts a batch job and waits for completion. The pool lives in `parallel_scan`'s statics, initialised on first `acquire`. This is the same approach Postgres uses for parallel workers (background workers pre-forked at startup).

### 3. Per-row `Vec<Literal>` + `String` allocation
`deform_row` and `project_row` allocate a `Vec<Literal>` per row, and each `TEXT` / `Bytea` value allocates a `String`. For 20 k rows with a TEXT `body` column, this is 20 k `String` allocations plus 20 k `Vec` allocations, all under the global allocator.

**Fix:** Arena-allocate row data within the scope of a single query. A per-query bump allocator (`bumpalo` or a hand-rolled slab) eliminates per-row alloc/free round-trips; the entire arena is dropped at query end. This requires `Literal` to be lifetime-parameterised (`Literal<'a>`) or a parallel row-data representation (`RawRow`) that borrows from the arena.

## Acceptance criteria

- Isolated probe (`cargo test --release --test par_check_test`) at 40 k rows / 20 k candidates reaches ≥ 5 M rec/s (vs current 4 M isolated / 2.6 M bench).
- `a3_gate_size_swept_crossover_correctness` and all existing correctness tests remain green.
- `cols/row` stays at 4.00 (selective arm still chosen).
- `PROGRESS.md` records before/after with absolute numbers.

## Depends on / builds on

- Item 43 (`43_a3_gate_size_aware_selectivity.md`) — A3 gate shipped; correct predicate arm now selected.
- `src/sql/parallel_scan.rs` — `parallel_resolve_candidates`, worker governance.
- `src/btree_index.rs` — `DiskBTree::search_range` → needs `search_range_partition`.
- `src/sql/executor.rs` — `try_exec_select_btree`.
