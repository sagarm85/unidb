**Type:** Performance
**Status:** ⏳ NOT STARTED

# Item 90 — Batched B-tree maintenance for non-HOT UPDATE

> Formalizes the "write_node reduction / lazy leaf coalescing" proposals
> discussed 2026-07-19 (originally floated as "items 85/86" before the number
> collision with the concurrency-hang item).

## Problem

UPDATE non-HOT (indexed column in SET) is the weakest CRUD row: **0.42× PG**,
WAL **202 B/row** vs the ~82 B/row heap-only floor — the ~120 B/row difference
is B-tree maintenance. Item 83 batched the heap side via `update_many`; index
entries still produce one `write_node` / WAL page-image per touched leaf *per
flush*, with per-row staging.

## Fix

1. **Sort-then-merge**: accumulate the statement's index inserts, sort by key,
   and merge into the leaf chain in one left-to-right pass — each dirtied leaf
   is written and WAL-imaged **once per statement**, regardless of how many
   new keys landed in it. (Extends item 47's unchanged-key patch batching to
   changed-key inserts; item 84's merge-split machinery is the insert-path
   precedent.)
2. **Lazy leaf coalescing**: within the statement, keep dirtied leaves in a
   small write-back set; flush on statement end or set overflow — cuts
   `write_node` call count when consecutive keys hit the same leaf.

## Expected gain

- UPDATE non-HOT: 0.42× → **~0.5–0.6× PG**; WAL 202 → **~100–120 B/row**.
- Bulk INSERT into indexed tables benefits from the same pass.

## Risks

- In-statement visibility: uniqueness/FK checks that read the index mid-
  statement must see staged entries (or the staging must be gated to the
  no-unique/no-FK case, like `update_many` already is — start there).
- Crash safety: leaf images are WAL-logged before page write as today; the
  write-back set is volatile staging, never a durability boundary.

## Acceptance criteria

- Docker Table 3: UPDATE non-HOT ≥ 0.50× PG; WAL ≤ 130 B/row; other rows
  non-regressed. Crash harness green.
