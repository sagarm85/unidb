# Item 117 — HOT UPDATE on PK/UNIQUE tables when the key column is unchanged

**Type:** Performance
**Status:** 🔄 IN PROGRESS — code implemented, native tests + crash harness green
2026-07-30; Docker bench (Table 5) pending to certify the ratio. Branch
`feat/hot-update-unique-key-gate`.

**Target:** Table 5 `UPDATE bulk (re-checks FK path)` — `UPDATE orders SET status =
'shipped' WHERE id < N` on `orders (id PRIMARY KEY, customer_id REFERENCES …,
amount, status)`. Baseline `report_20260728_102745.md`: **0.19×** vs PG (138,840 vs
747,068 rec/s) — the worst CRUD row in the report. Expected to move toward the
PK-less UPDATE-HOT band (Table 3 UPDATE HOT = 1.19×), since the two now take the
same path.

## Root cause (from the 2026-07-30 architecture review)

`hot_eligible` was gated on `!has_unique` (`src/sql/executor.rs`): HOT update was
disabled by the mere *existence* of any PK/UNIQUE index, regardless of whether the
`SET` clause touches a unique column. Because virtually every real table has a PK,
the fast HOT path (which won the PK-less Table 3 its 1.19×) almost never applied in
practice — Table 5 (with a PK) was the representative case at 0.19×. Postgres does
HOT precisely when no *indexed* column changes, PK or not.

The mislabel: the report row calls this "re-checks FK path", but the FK check is
already skipped (item 53 — `status` is not the FK column). The cost was the
`has_unique` blanket forcing the statement off BOTH the HOT and non-HOT batch
paths onto the per-row loop, plus a redundant `enforce_unique` (O(log n) PK B-tree
point lookup + visibility check) on every row even though `id` never changes.

## Change

Mirror the item-53 FK gate with `has_unique_in_set` — true only when a unique/PK
column actually appears in the `SET` clause:

- `hot_eligible`: `!has_unique` → `!has_unique_in_set`. `set_touches_indexed_col`
  (which already checks `unique_index_root`) remains the precise backstop that
  forbids HOT when the unique/PK column itself is assigned.
- Constraint block gate + `UniqueKey` phantom-lock acquire + `enforce_unique`:
  `has_unique` → `has_unique_in_set` (an unchanged key needs no lock and cannot
  introduce a duplicate — the check can only match this row's own excluded old
  version).
- `can_batch_non_hot` left conservative on `!has_unique`: the non-HOT batch
  (`Heap::update_many`) documents a no-UNIQUE-index precondition; relaxing it is a
  separate follow-up (would need the batch to re-point the unique B-tree). Not
  needed for the Table 5 win, which is HOT (`status` is non-indexed).

## Safety (verified, not assumed — §0.6.2/§0.6.4)

The relaxation is only correct if the unchanged unique/PK index entry still
resolves to the live (HOT-updated) version. Confirmed: unique/PK lookups follow
HOT chains through the **identical** `DiskBTree::search_eq` → per-candidate
`get_visible` path as secondary indexes, and `get_visible` is the single function
that walks both same-page (`hot_next`) and cross-page (`HOT_NEXT_XPAGE`) chains.
So SELECT-by-PK, `enforce_unique`, and secondary lookups all resolve identically.
This exact "index points at old slot, HOT chain resolves it" pattern already ran
for secondary btrees on non-indexed-column updates; unique is structurally the
same case.

## Tests

- `tests/constraints.rs::item117_hot_update_of_nonkey_col_keeps_unique_indexes_correct`
  — PK+UNIQUE table, HOT-update a non-key column, then assert: PK lookup returns
  the new value; duplicate email/PK INSERTs still rejected (stale index entry →
  live version via HOT chain); changing the key itself is still re-checked.
- Full `tests/constraints.rs` (29) green — incl. `update_into_existing_unique_value…`,
  `pk_update_throughput_is_flat`, `fk_update_non_fk_col_skips_enforcement`.
- Crash harness 54/54 green (p74 batch HOT, cross-page HOT, p58b index, p17/p60
  vector). fmt + clippy clean.

Metrics land in `PROGRESS.md` once the Docker bench runs.
