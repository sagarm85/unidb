# NEAR(): expose vec_distance as a computed output column

**Type:** Improvement
**Status:** SHIPPED (→ PROGRESS.md "NEAR() vec_distance virtual column (item 41)")

## Problem

`WHERE NEAR(embedding, [...], k)` ranks and returns the k nearest rows but does
not expose the computed Euclidean distance in the result set. Callers cannot
distinguish a genuinely close match (distance 0.1) from a weak one (distance
1.8) — all k rows look identical in quality.

Concrete symptom: `SELECT id, title, vec_distance FROM documents WHERE NEAR(…)`
returns `COLUMN_NOT_FOUND · column 'vec_distance' not found on table ''`.

This is standard behaviour in every other vector database:
- pgvector: `ORDER BY embedding <-> $1 LIMIT k` — distance is an expression
- Qdrant / Pinecone: distance is a first-class field in the result payload

## Desired behaviour

After this item ships, the following query works:

```sql
SELECT id, title, vec_distance
FROM documents
WHERE NEAR(embedding, [0.0, 0.5, ...], 3);
```

`vec_distance` is a virtual computed column, available only when the `FROM`
clause contains exactly one table and the `WHERE` clause contains a `NEAR`
predicate on a HNSW-indexed column. Its value is the Euclidean distance between
the stored vector and the query vector for that row.

Result (ascending by distance — closest first):

| id | title                       | vec_distance |
|----|-----------------------------|--------------|
| 1  | Wireless Bluetooth Headphones | 0.412      |
| 9  | Noise Cancelling Earbuds      | 0.534      |
| 5  | Mechanical Gaming Keyboard    | 1.201      |

## Implementation notes

The NEAR predicate already computes the distance internally (the HNSW search
returns `(row_id, distance)` pairs). The distance just needs to be threaded
through to the projection layer and exposed as a virtual column named
`vec_distance` of type `Float`.

Suggested approach:
- HNSW search returns `Vec<(RowId, f32)>` — the `f32` is the distance.
- Store distances in the executor scan context alongside row data.
- During projection, recognise `vec_distance` as a virtual column name and
  substitute the stored distance value for that row.
- Only valid in a NEAR query context; return `COLUMN_NOT_FOUND` otherwise
  (current behaviour is already correct for non-NEAR queries).

## Scope

- `src/sql/executor.rs` — HNSW scan path + projection
- No API or catalog changes
- No Studio changes needed (Studio will work once the column is real)

## Acceptance criteria

- [x] `SELECT id, vec_distance FROM t WHERE NEAR(embedding, [...], k)` returns
      distances as `Float` values, ascending (closest first).
- [x] `SELECT vec_distance FROM t` (no NEAR predicate) returns `COLUMN_NOT_FOUND`.
- [x] Integration test: distances are in non-decreasing order for a known corpus.
- [x] ~~`vector_demo.py` updated~~ **Correction (2026-07-14):** no `vector_demo.py`
      (or any Python demo script) exists anywhere in this repository — grepped
      the whole tree, none found. This criterion describes a file that was never
      part of this codebase; nothing to update. Covered instead by
      `tests/vec_distance.rs::vec_distance_returned_ascending_for_known_corpus`,
      which seeds the same id/title/distance corpus from the spec's example
      table and asserts the exact ascending order + values.

## Implementation (shipped 2026-07-14)

- `src/sql/executor.rs`: `exec_select_near` now scores each NEAR candidate with
  its exact re-ranked Euclidean distance (already computed for sorting) and
  projects it via a new `project_row_near` helper — identical to `project_row`
  except it recognizes the virtual column name `vec_distance` (new constant
  `VEC_DISTANCE_COL`) and substitutes the computed `f32` distance as
  `Literal::Float` instead of doing a catalog column lookup for it.
  `SELECT *` (empty projection) falls through to the ordinary `project_row`
  path, so `vec_distance` never appears unless explicitly named — same
  convention as any other SQL engine's computed/virtual column.
- Outside a `NEAR` predicate, `vec_distance` is not a real column anywhere in
  the catalog, so the ordinary `project_row`/`eval_expr` column lookup already
  returns `COLUMN_NOT_FOUND` — no special-casing needed for that half of the
  contract.
- `tests/vec_distance.rs` (3 new integration tests): ascending-order + exact
  distance values for a known corpus (mirrors the spec's example table),
  `COLUMN_NOT_FOUND` outside a `NEAR` context, and `SELECT *` never leaking the
  virtual column.
- No catalog/API changes; no `FORMAT_VERSION` bump.
