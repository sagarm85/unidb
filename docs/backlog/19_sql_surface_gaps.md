# SQL surface gaps — unsupported query features

**Type:** Improvement
**Status:** PARTIAL — G1 (CASE/COALESCE/NULLIF), G2-cast (CAST expressions), G3 (UNION/INTERSECT/EXCEPT), G4 (ORDER BY non-projected col), G5 (RETURNING), G8 (SELECT without FROM), G10 (IS NULL) SHIPPED (see `PROGRESS.md` item 19). G2-join (FULL OUTER JOIN)/G6/G7/G9/G11 remain open.

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

### G2-cast — `CAST(expr AS type)` explicit type conversion **(SHIPPED 2026-07-20)**

- **What:** `CAST(expr AS type)` scalar expression in SELECT lists, WHERE
  predicates, and anywhere a `QExpr` is valid.
- **Why it matters:** required by many SQL queries, migration scripts, and
  client ORMs (e.g. `CAST(id AS TEXT)` for string concatenation, `CAST(label
  AS INT)` for numeric comparisons on text columns).
- **Supported types:** `TEXT`/`VARCHAR`, `INT`/`INTEGER`/`BIGINT`,
  `FLOAT`/`REAL`/`DOUBLE`, `BOOLEAN`/`BOOL`. Exotic types
  (TIMESTAMP, DECIMAL, JSON, …) return `SqlUnsupported`.
- **Conversion table:**
  - Any → TEXT: format via `Display` (integers, floats, booleans, decimals)
  - Text → INT: `s.parse::<i64>()`, error on failure
  - Float/Decimal → INT: truncate toward zero (`f as i64`)
  - Bool → INT: `true` → 1, `false` → 0
  - Text → FLOAT: `s.parse::<f64>()`, error on failure
  - Int → FLOAT: `n as f64`
  - Text → BOOL: `"true"|"t"|"yes"|"y"|"on"|"1"` → true; negatives → false
  - Int → BOOL: `n != 0`
  - NULL → any: NULL (SQL standard)
- **Scope:** `QExpr::Cast { expr, to_type: CastTarget }` + parser mapping
  from `SqlExpr::Cast` + `expr_has_case_expr` detection to force Phase-4
  routing + eval arms in both `plan::eval_qexpr` and `query_exec::Runner::eval`
  + `collect_columns`/`collect_qualifiers` in optimizer + substitute_correlated
  in query_exec + `bind_params`/`has_aggregate`/`has_subquery` in query.rs.
  18 tests in `tests/item19_cast.rs`. No storage/format impact.
- **Outcome:** see `PROGRESS.md` item 19 G2-cast entry.

### G2-join — `FULL OUTER JOIN` (previously called G2)
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

### G9 — `LIKE` / `NOT LIKE` (and `ILIKE`) pattern matching **(Delivered under item 30 — Studio API readiness.)**
- **ROI: high** — despite the G9 id (ids are stable, the list is not renumbered),
  this belongs near the **top**. Substring/prefix search is one of the most common
  filter operations a console offers.
- **What:** SQL `LIKE` / `NOT LIKE` with `%` / `_` wildcards (and case-insensitive
  `ILIKE`). Rejected today on **both** expression paths — there is no
  `SqlExpr::Like` arm in the simple row path (`convert_expr`) *or* the query
  planner path (`convert_qexpr`), so any `col LIKE …` returns `SQL_UNSUPPORTED`
  whether the pattern is a literal or a bound `$n` param.
- **Why it matters:** surfaced by the **`unidb-studio` record browser** (2026-07-13):
  its Supabase-style *contains / starts with / ends with* filter operators all
  compiled to `LIKE` and every query failed, so the studio had to **remove those
  operators**. Full-text (`FULLTEXT` index + search) is word-oriented and is *not*
  a substitute for substring/prefix `LIKE`.
- **Scope:** add a `QExpr::Like { expr, pattern, negated, case_insensitive }`
  variant (+ the row-path `Expr` equivalent so it works with and without the
  planner) + parser mapping from `SqlExpr::Like`/`ILike` + an evaluator
  implementing LIKE semantics (`%` = any run, `_` = one char, `ESCAPE`). Pattern
  may be a literal or a `$n` param (binds as text). No storage/format impact.
  Medium. Optional optimization: a pure-prefix pattern (`'abc%'`, no leading
  wildcard) can use an existing B-Tree as a range scan; otherwise scan-and-filter.

### G10 — Row-path predicate parity: `IS NULL` / `IS NOT NULL` (and G9) off the planner path
- **What:** `IS NULL` / `IS NOT NULL` parse on the **planner** path
  (`convert_qexpr`) but **not** the simple **row** path (`convert_expr`). So
  `SELECT * FROM t WHERE c IS NULL` works only when the statement *also* has
  something (ORDER BY, aggregate, join, …) that forces the planner path; a bare
  filtered select — and, notably, the `NEAR()` vector path, which evaluates its
  AND'd filters on the row engine — reject it as `SQL_UNSUPPORTED`.
- **Why it matters:** same studio finding (2026-07-13) — the record browser's
  `IS NULL` filter works while browsing (its queries carry `ORDER BY`) but cannot
  compose with **"Find similar"** (`NEAR(...) AND c IS NULL`), forcing a
  client-side workaround. The inconsistency is surprising: the same predicate
  works or not depending on unrelated clauses.
- **Scope:** bring the row path's predicate coverage to parity with the planner
  path — add `IsNull`/`IsNotNull` (and G9's `Like`) to `convert_expr` +
  `eval_expr`, so filters behave identically regardless of which plan the query
  takes (including under `NEAR`). Small–medium; no storage impact. Best done
  alongside G9 (same evaluator surface).

### G11 — Full-text search has no SQL / REST surface (embed-only today) **(Delivered under item 30 — Studio API readiness.)**
- **What:** you can `CREATE INDEX … USING FULLTEXT (col)`, but there is **no way
  to query it over `/sql`** — no `MATCH`/`@@`/search predicate in the parser
  (0 refs to full-text in `src/sql/`). Search is only reachable from Rust via
  `Engine::search_fulltext`, so any out-of-process client (the studio, attach over
  HTTP, any non-Rust app) can build the index but never use it.
- **Why it matters:** surfaced by `unidb-studio` (2026-07-13). With `LIKE`
  unsupported (G9), full-text is the intended substitute for text search — but it
  isn't reachable over the one surface a browser has (`POST /sql`), so the studio
  can offer neither substring nor word search. The engine advertises full-text as
  a domain extension (guide §2) yet it's inaccessible to every documented access
  path except embed.
- **Scope:** expose full-text as a SQL predicate that routes to the existing
  `Engine::search_fulltext` — e.g. a `MATCH(col, $1)` / `col @@ $1` expression the
  planner lowers to an inverted-index probe (mirroring how `NEAR(...)` maps to the
  vector index in `exec_select_near`). Parser arm + logical/physical node +
  executor wiring; no storage change (the index already exists). Medium; pairs
  naturally with G9/G10's predicate work.

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
