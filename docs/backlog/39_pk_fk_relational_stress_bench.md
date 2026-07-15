# PK/FK relational-integrity stress bench (Table 5)

**Type:** Performance
**Status:** SHIPPED (→ `PROGRESS.md` "PK/FK relational-integrity stress bench
(item 39)") — new Table 5 in `scripts/report.sh` / `scripts/multi_model_report.sh`;
see `PROGRESS.md` for measured numbers.

## Context

Every existing table in `benches/decompose.rs`'s multi-model report either had
no `PRIMARY KEY` at all (the W0→W4 ladder, most of Table 3's CRUD stress) or a
`PRIMARY KEY` with zero `FOREIGN KEY` constraints anywhere in the file — grepped
directly, zero `REFERENCES`/`FOREIGN KEY` hits across the whole bench. That
understated Postgres's real integrity-checking cost in exactly the kind of
multi-table relational schema a real app actually runs, and it meant the report
never exercised unidb's own FK enforcement path at all.

That gap closed itself in a good way: item 36 (FK row-level enforcement,
shipped 2026-07-14, the same day as this item) replaced unidb's old
table-existence-only FK check with real row-level referential integrity — child
INSERT/UPDATE verifies the parent key via the parent's implicit unique-index
B-tree (O(log n), item 35), and parent DELETE/UPDATE enforces RESTRICT. That
made a PK/FK benchmark table genuinely fair for the first time — both engines
now pay a comparable, real integrity-check cost, not "unidb doesn't really
check, Postgres does."

Also relevant, found while reviewing the existing reports before starting: the
"good" 2026-07-10 report had real Postgres numbers because `PG_URL` was set;
every report since had simply been run without it, so Tables 3/3.1/4's
Postgres columns came back blank. Not a script regression — an operator habit
fixed by remembering to set `PG_URL` (and `MM_REPLACED_STACK=1` for the honest
Table 4) on every real run going forward.

## What shipped

A new **Table 5 — PK/FK relational-integrity stress** in `bench_mm_report()`
(`benches/decompose.rs`), wired into the existing report pipeline with no new
script/flag beyond one optional size knob:

- Schema: `customers (id PRIMARY KEY, name)` / `orders (id PRIMARY KEY,
  customer_id REFERENCES customers(id), amount, status)`, pre-loaded with
  20,000 customers (`FK_CUSTOMERS`) then `MM_FK_ORDERS` orders (default
  20,000), identical schema on both engines.
- Throughput rows: INSERT with a real FK check on every row, UPDATE (re-checks
  the FK path), and a JOIN `SELECT` across the two tables — the same
  unidb-vs-Postgres throughput-table shape as Table 3.
- **Correctness proofs, not just speed** (matching the report's existing
  crash-consistency-proof pattern in Table 4): confirms an INSERT referencing a
  non-existent customer is rejected on *both* engines, and confirms a DELETE of
  a still-referenced customer is blocked (RESTRICT) on *both* engines — a
  pass/fail line, not a number, so a future regression in either engine's FK
  enforcement shows up as a flipped ✓/✗ in the report itself.
- Runs unconditionally like Tables 1/2; the Postgres columns follow the same
  `PG_URL`-gated pattern as every other table (skip cleanly, don't fail, if
  unset).

## Verification

- `cargo build --release --bench decompose` — clean.
- `cargo clippy --release --bench decompose -- -D warnings` — clean.
- `cargo fmt --all --check` — clean.
- Smoke-tested at tiny scale (`MM_FK_ORDERS=50`, `MM_SIZES=100`, etc.) before
  the real run: both correctness proofs passed (`unidb **rejected** ✓, Postgres
  **rejected** ✓` / `unidb **blocked (RESTRICT)** ✓, Postgres **blocked
  (RESTRICT)** ✓`), throughput rows rendered sane, non-crashed numbers.
- Full-scale run (default sizes, `PG_URL` set, `MM_REPLACED_STACK=1`) recorded
  in `PROGRESS.md` and `docs/performance/`.

## Known limitations (documented in the report's own Caveats section)

- Table 5's FK check is single-column, point-lookup (item 35's implicit unique
  index). A composite or non-indexable FK column falls back to an O(n) heap
  scan on unidb — not exercised by this table, a real gap if a future
  workload needs it.
- No `ON DELETE CASCADE`/`SET NULL` — RESTRICT only, matching unidb's current
  FK feature set (item 36); Postgres in this bench is configured the same way
  for a fair comparison, not because Postgres can't do more.
