# Phase 2 — SQL Capability Expansion (OR, ORDER BY/LIMIT, Aggregates, JOIN)

> Status: **PAUSED, not started.** Saved here as a durable reference so it
> isn't lost between sessions. Do not begin implementing from this plan
> until explicitly told to resume — the user wants a performance baseline
> of the current implementation checked/recorded first (see "Immediate
> next step" below).

## Context

Follow-up on "build the application like Supabase" (table definitions +
bulk import + SQL capabilities): raw SQL for table definition is confirmed
fine (no new structured endpoint needed), and bulk multi-row `INSERT INTO t
VALUES (...), (...), (...)` already works end-to-end today — no new work
required there. The genuinely open ask is broader SQL capability, which
directly revisits `CLAUDE.md` §1's stated non-goal ("not full ANSI SQL —
practical subset"). The user selected all four gaps as wanted: **OR in
WHERE, ORDER BY + LIMIT, aggregates (COUNT/SUM/AVG/GROUP BY), and JOIN**.
This should be recorded as a deliberate, scoped expansion (these four
additions, not "full ANSI SQL") — not a silent re-litigation of that
project philosophy.

Two research passes (via Explore agents) confirmed the engine's foundations
already support this cleanly:
- `Heap::scan()` (`src/heap.rs`) returns a fully materialized,
  MVCC-visibility-filtered `Vec<(RowId, Vec<u8>)>` — the right primitive to
  sort/group/join on top of.
- **No read locks exist anywhere** (`src/lockmgr.rs` is write-only, MVCC
  snapshot isolation covers all reads) — so JOIN needs **zero new locking
  code**.
- A single `Snapshot` (`src/mvcc.rs`, from
  `txn_mgr.snapshot_for_statement(xid)`) is immutable and already proven
  safe to reuse across multiple `heap.scan()` calls in one statement —
  exactly what a JOIN needs to read two tables consistently.
- `exec_select_near` (`src/sql/executor.rs` ~391-468, the vector `NEAR`
  path) already does over-fetch → per-row filter → `out.truncate(k)` — the
  direct template for ORDER BY + LIMIT (scan everything, sort in memory,
  truncate).
- `predicate_matches`/`eval_expr` (`src/sql/executor.rs`, `pub(crate)`) is
  shared automatically by the Cypher executor (`src/graph/executor.rs`
  imports `predicate_matches`) — adding `Expr::Or` there gives Cypher OR
  support for free, no graph-module changes needed.
- RLS composition (`apply_rls`/`and_policy` in `src/sql/logical.rs`) ANDs a
  table's policy onto the *outside* of the user predicate
  (`Expr::And(user_predicate, policy)`) — confirmed this keeps working
  unchanged even once the user predicate can contain `Or` nodes, since the
  policy is always the mandatory outer conjunct.
- Zero existing sort/limit/aggregate/join code anywhere — these are built
  from scratch, not wired to dead/partial code.

## Current state (exact, from the two research passes)

- `LogicalPlan::Select { table: String, projection: Vec<String>, predicate: Option<Expr> }` — single table only, no order/limit/group-by fields (`src/sql/logical.rs:76-109`).
- `Expr` enum: `Column`, `Literal`, `BinOp{Eq,Ne,Lt,Gt,Le,Ge}`, `And`, `JsonExtract`, `JsonExtractText`, `Near` — no `Or`, no aggregate functions (`src/sql/logical.rs:42-73`).
- `BinaryOperator::Or` is explicitly rejected at `src/sql/parser.rs:486-488`.
- Qualified column names (`table.column`) are **not preserved** today — `column_name_from_parts` (`src/sql/parser.rs`) keeps only the last identifier segment. This must be fixed for JOIN's ambiguous-column resolution.
- `ColumnDef { name, ty, index }` / `TableDef { name, columns, pages, rls_policy, events_enabled }` (`src/catalog.rs:69-94`) — no cross-table concept exists yet.

## Recommended sequencing (4 checkpoints, mirrors M1–M5's a/b/c/d discipline)

### Phase 2.a — OR support (cheapest, foundational)
- Add `Expr::Or(Box<Expr>, Box<Expr>)` to `logical.rs`.
- `parser.rs:486`: replace the `SqlUnsupported` error with `Ok(Expr::Or(lhs, rhs))`.
- `executor.rs::eval_expr`: add an `Or` arm (`l || r`, correctness-first, no short-circuit needed given side-effect-free `eval_expr`).
- No RLS changes needed (confirmed above).
- Tests: `WHERE a = 1 OR b = 2` round-trip; an RLS-with-OR test proving a row must satisfy `(a OR b) AND policy`, not just `(a OR b)`.

### Phase 2.b — ORDER BY + LIMIT (single table)
- Extend `LogicalPlan::Select` with `order_by: Vec<(String, bool)>` (column, ascending) and `limit: Option<usize>`.
- Parser: read `sqlparser`'s `Query.order_by`/`Query.limit` AST fields (confirm exact field names/shapes during implementation — the crate is already a dependency).
- Executor: after building `out: Vec<Vec<Literal>>`, sort by the ordered column (numeric compare for `Int64`, lexicographic for `Text`, `false < true` for `Bool`; `Json`/`Vector` columns in `ORDER BY` → `SqlUnsupported`), then `truncate(limit)` — directly reusing `exec_select_near`'s idiom.
- Tests: ASC/DESC ordering, LIMIT truncation, combined with WHERE + RLS.

### Phase 2.c — Aggregates + GROUP BY (single table)
- New small `AggFunc` enum (`Count`, `Sum`, `Avg`, `Min`, `Max`); projection becomes able to carry aggregate items alongside plain columns (e.g. a `ProjectionItem` enum: `Column(String)` | `Agg{func, column}`).
- `LogicalPlan::Select` gains `group_by: Vec<String>`.
- Executor: hash-aggregate after WHERE filtering — bucket rows by group-by key(s) into a `HashMap`, compute per-bucket aggregate(s), emit one row per group. `SUM`/`AVG` restricted to `Int64` columns (no float type exists outside `Vector` components) — non-numeric → `SqlUnsupported`.
- Tests: `COUNT(*)`, `SUM`/`AVG`/`MIN`/`MAX` per group, GROUP BY interaction with WHERE + RLS (RLS must filter *before* aggregation).

### Phase 2.d — JOIN across two tables (biggest lift)
- Add a **new** `LogicalPlan::Join { left: String, right: String, on: Expr, projection, predicate }` variant rather than generalizing `Select` itself — keeps the single-table path (used everywhere else: UPDATE/DELETE/RLS/NEAR) untouched, matches the "practical subset, not over-generalized" ethos.
- **Scope, stated explicitly (not silently narrowed later)**: exactly 2 tables, INNER JOIN only, single equality `ON a.col = b.col` condition. No 3+ way joins, no OUTER joins, no non-equi joins.
- Fix column qualification in the parser (`column_name_from_parts`) so `table.column` survives for joined queries; resolve against a combined schema (`left.columns` ++ `right.columns`, disambiguated by table prefix).
- **RLS correctness (security-critical, needs its own explicit test)**: apply each table's own `rls_policy` to its own side's scan *before* the join combines rows — never let a nested-loop join accidentally read past one side's policy. Reuse the existing single-table filter pass per side.
- Execution: nested-loop join for v1 (materialize left side filtered by its own predicate+RLS, same for right, then loop applying the `ON` equality + remaining predicate). No hash-join optimization yet — document as a known, deliberate limitation (consistent with "no premature optimization").
- Tests: basic 2-table equi-join correctness; the RLS-per-side enforcement test (most important); MVCC consistency (both sides read from one snapshot obtained once — already confirmed safe).

## Documentation / process notes

- This intentionally revisits `CLAUDE.md` §1's "not full ANSI SQL (practical subset)" line. Recommend an explicit note in `PROGRESS.md` when work begins — not one of the numbered D1–D13 locked decisions, but a stated project philosophy worth acknowledging deliberately, the same way the M5 xid-bug fix's D3/D9 change was recorded.
- Suggest tracking this as **"M6 — SQL capability expansion"** in `MEMORY.md`/`PROGRESS.md`, continuing the existing milestone convention now that M0–M5 are all done.

## Immediate next step (before any of the above): performance baseline

The user wants current performance recorded **before** this scope is added,
so Phase 2's impact can be measured against a known "before" state:
- Re-run `cargo bench` (load/vector/graph/queue) and
  `cargo bench --features server` (server overhead) for a fresh, current
  snapshot — prior numbers exist in `PROGRESS.md` from M1–M5 but predate
  today's environment/state.
- Pay particular attention to `benches/load.rs`'s SELECT numbers —
  Phase 2.b/c/d all add cost directly to `exec_select`'s hot path (sorting,
  grouping, joining), making this the benchmark most likely to move and
  most important to have a clean "before" reading for.
- Record this baseline (with today's date) before any Phase 2 code lands,
  so the eventual comparison is honest and evidence-based (`CLAUDE.md` §6).

**Baseline status as of 2026-07-07**: a partial baseline was captured —
`load.rs` INSERT (155.11–156.33 elem/s) and `graph.rs` adjacency-scan
numbers matched the already-recorded M1/M3 `PROGRESS.md` figures exactly
(no drift). The `server.rs` concurrent-throughput numbers from that same
run are **not trustworthy** — they were captured while another benchmark
suite ran concurrently on the same machine, causing CPU contention that
roughly halved the measured ops/s. The already-recorded, cleanly-measured
M5 baseline in `PROGRESS.md` (~135→157→158 ops/s flat across 1/10/50
concurrent clients) remains the correct reference. `load.rs`'s
`select_point`/`update_in_place`/contention workloads and all of
`vector.rs`/`queue.rs` were not re-run (the `insert/10000` benchmark alone
projected 100+ minutes) — no reason to expect drift since no code has
touched those paths since they shipped. A full, clean (single-suite-at-a-
time) re-run is still open if an exact fresh number is wanted before
Phase 2 starts.

## Verification (once implementation resumes)

- `cargo test` / `cargo test --features server` green at every checkpoint (2.a–2.d).
- `cargo clippy --all-targets -- -D warnings` / `cargo fmt --all --check` clean, with and without `--features server`.
- New tests per checkpoint as listed above.
- Re-run the same benchmark suite after 2.d and diff against the pre-Phase-2 baseline in `PROGRESS.md`.

## Critical files

- `src/sql/logical.rs` — `Expr`, `LogicalPlan` enums, `apply_rls`
- `src/sql/parser.rs` — `convert_expr`, `convert_query`, the OR rejection (~line 486), column qualification
- `src/sql/executor.rs` — `eval_expr`, `predicate_matches`, `exec_select`, `exec_select_near` (the template to reuse)
- `src/graph/executor.rs` — inherits `predicate_matches` automatically; re-verify Cypher tests still pass after `Expr::Or` lands
- `src/catalog.rs` — `ColumnDef`/`TableDef`; column qualification needed for JOIN
- `benches/load.rs` — today's numbers are this phase's baseline; extend once 2.b/2.c/2.d land
- `tests/` — new test files or additions to existing SQL test coverage
