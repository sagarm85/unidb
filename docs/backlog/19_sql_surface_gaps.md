# SQL surface gaps — unsupported query features

**Type:** Improvement
**Status:** NOT STARTED

> A single tracker for the SQL constructs unidb does **not** support yet, so
> builders (and future us) have one honest list and each gap has a scope/ROI
> read. Surfaced by Milestone 18's Application Builder's Guide (`docs/
> engine_access_guide.md` §2 "not supported yet" + §8 limitations) — this file
> is where those items live as *tracked work*, not just documentation.
>
> These are practical-subset gaps, consistent with CLAUDE.md §1's scope
> discipline ("not full ANSI SQL"). Filing them is not a commitment to build all
> of them — it is the honest backlog so the guide's not-supported list has a home
> and a priority order. Metrics/outcomes for anything shipped go in `PROGRESS.md`
> per `CONVENTIONS.md`; pick items via `backlog_index.md`'s *Next up*.

## Gaps (rough ROI order — highest first)

### G1 — Scalar `CASE` / `COALESCE` / `NULLIF` expressions
- **What:** conditional scalar expressions in SELECT/WHERE/ON. The executor's
  `QExpr` has no `Case`/`Coalesce` node today.
- **Why it matters:** broadly useful (computed columns, null-defaulting), and it
  is the concrete **blocker for G2** — a `FULL OUTER JOIN … USING` column is
  `COALESCE(left.c, right.c)`, which we can't express without it. `LEFT`/`RIGHT`
  `USING` already merge correctly by keeping the outer-preserved side (see
  `plan.rs::plan_using_join`, Milestone 18); only the both-sides-outer case needs
  real `COALESCE`.
- **Scope:** add `QExpr::Case`/`Coalesce` variants + parser mapping + evaluator
  arms; no storage/format impact. Self-contained, medium.

### G2 — `FULL OUTER JOIN`
- **What:** the fourth join type (currently `INNER`/`LEFT`/`RIGHT`/`CROSS`).
- **Why it matters:** completes the join set; needed by some reporting tools.
- **Scope:** join operators (`join.rs`) gain a full-outer path (emit unmatched
  rows from **both** sides). Correctness-wise straightforward for `ON`; a
  `USING`/`NATURAL` full-outer needs **G1** for the coalesced key column, so
  order G1 first. Medium.

### G3 — Set operations: `UNION` / `UNION ALL` / `INTERSECT` / `EXCEPT`
- **What:** combine two query results. Parser currently rejects any non-`SELECT`
  set-expr body (`convert_query`'s `SetExpr::Select` only).
- **Why it matters:** common in dashboards and "combine two filters" flows.
- **Scope:** a new logical/physical set-op node over two `QuerySpec`s (schema
  compatibility check, `ALL` vs dedup). Reuses the `Distinct` machinery for dedup.
  Medium; no storage impact.

### G4 — `ORDER BY` on a non-projected expression
- **What:** `ORDER BY <expr>` where `<expr>` is not in the SELECT output list
  (only output column names / ordinals sort today —
  `plan.rs::resolve_order_by`). This is why Milestone 18's worked-example test
  drops `ORDER BY kcu.ordinal_position` and sorts client-side.
- **Why it matters:** a normal SQL convenience; its absence surprises builders.
- **Scope:** sort over the *pre-projection* schema (compute the sort key as an
  expression against the input rows, then project) — the classic "sort sees more
  columns than the SELECT list" plan shape. Small–medium; no storage impact.

### G5 — `INSERT/UPDATE/DELETE … RETURNING`
- **What:** return affected rows from a DML statement.
- **Why it matters:** saves a round-trip (get the SERIAL id back, etc.).
- **Scope:** thread a projection through the DML executors so they can emit a
  `Rows` result instead of just a count. Medium; touches the write path but no
  storage/format change.

### G6 — `NATURAL JOIN`
- **What:** join on all commonly-named columns implicitly.
- **Why it matters:** low — mostly discouraged (implicit key set is fragile);
  `USING (cols)` (supported since Milestone 18) is the explicit, safer form.
- **Scope:** desugar to `USING` over the intersection of both sides' column
  names — reuses `plan.rs::plan_using_join` entirely once the shared column set
  is computed from the two schemas. Small. Low ROI.

### G7 — Window functions & recursive CTEs
- **What:** `OVER (PARTITION BY … ORDER BY …)` window functions; `WITH
  RECURSIVE`.
- **Why it matters:** real analytics power, but large and squarely in the
  "OLAP-class, out of scope" bucket CLAUDE.md §1 flags. Non-recursive CTEs and
  ordinary aggregates already cover most needs.
- **Scope:** **large** — each is its own milestone-sized effort (a window
  operator with framing; recursive-CTE fixpoint evaluation). File as separate
  numbered work if/when picked up; not a single-PR improvement.

### G8 — `SELECT` without `FROM` (`SELECT 1`, `SELECT now()`)
- **What:** a constant/expression-only select. Rejected today
  (`SELECT without FROM is not supported`).
- **Why it matters:** low, but tools sometimes probe with `SELECT 1` as a health
  check.
- **Scope:** a one-row synthetic input for a FROM-less select. Small. Low ROI.

## Explicitly *not* in this file (tracked elsewhere — don't duplicate)

- **Row-level FK enforcement + `ON DELETE`/`ON UPDATE` actions.** FK is parsed,
  persisted, and introspectable (M11 + Milestone 18); enforcement beyond
  referenced-*table* existence is the M11-scoped follow-up recorded in
  `catalog.rs`'s `ForeignKeyRef` doc + `PROGRESS.md`'s M11 entry, not here.
- **Multi-page catalog** (a very wide `ANALYZE`d schema can overflow the single
  ~8 KiB catalog blob) — tracked as Phase-4 tech debt in `engine_design.md`, not
  a query-surface gap.

## References

- `docs/engine_access_guide.md` §2 (SQL surface + not-supported list) / §8
  (honest limitations) — the user-facing source of these items.
- `src/sql/parser.rs` — where each unsupported construct currently returns
  `SqlUnsupported` (the authoritative, always-current rejection list).
- `docs/backlog/CONVENTIONS.md` — naming/lifecycle this file follows.
