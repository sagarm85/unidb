# Phase 2 — Real data model (SQL lane, `sql-types`)

## Status as of 2026-07-08: IN PROGRESS — P2.a (DECIMAL + TIMESTAMP) shipped (see `PROGRESS.md`); P2.b next.

Runs **in parallel with Phase 1** (disjoint files). Companion to
[`roadmap.md`](roadmap.md) §4/§5 — this is the detailed spec the `sql-types`
SQL-lane worktree executes. Owns the SQL/catalog layer only (`catalog.rs`,
`sql/*`); must not touch storage-core files (the Core lane owns those); `lib.rs`
edits are additive-only.

## Context — why this matters

Today the type system is `Int64 / Text / Bool / Json / Vector` — you cannot
store **money** (no `DECIMAL`) or **time** (no `DATE`/`TIMESTAMP`), can't evolve
a schema (no `ALTER`/`DROP`), have no surrogate keys (no `SERIAL`), and every
query is raw SQL string interpolation (injection surface, no plan reuse). This
phase makes unidb usable for real applications. None of it touches durability;
it builds on the existing catalog + encoding + parser machinery.

## Scope

- **IN:** rich scalar types, schema DDL (`ALTER`/`DROP`/`TRUNCATE`) with
  transactional catalog, sequences/`SERIAL`, prepared statements + bind params.
- **OUT (Phase 4):** joins, aggregates, `ORDER BY`/`GROUP BY`, subqueries, the
  cost-based optimizer, `EXPLAIN`.

## Checkpoints

### P2.a — DECIMAL + TIMESTAMP (money + time first)

- **`ColumnType::Decimal(precision, scale)` and `Timestamp`** in `catalog.rs`.
- **Encoding (hand-rolled, little-endian — no serde on the page path, per
  CLAUDE.md §4):**
  - `DECIMAL(p,s)` → exact fixed-point: store the value as an **`i128` scaled by
    10^s** (16 bytes LE). Exact arithmetic, no float error. (Alternative:
    `rust_decimal`; prefer the hand-rolled `i128` for dependency-light, exact
    byte control — decide and document.)
  - `TIMESTAMP` → **`i64` microseconds since Unix epoch, UTC** (8 bytes LE).
    (`TIMESTAMPTZ` normalizes to UTC on input; v1 stores UTC.)
- **`Literal::{Decimal, Timestamp}`** in `sql/logical.rs`; row encode/decode
  tags in `sql/executor.rs` (`encode_row`/`decode_row`).
- **Parser** (`sql/parser.rs::convert_data_type`): map sqlparser's
  `DataType::Decimal/Numeric(p,s)` and `Timestamp` → the new `ColumnType`s;
  parse literals (`'2024-01-01 12:00:00'`, numeric decimals).
- **M11-constraint compatibility:** `DEFAULT`, `CHECK`, PK/UNIQUE, and
  comparison operators must work for the new types (ordering, equality).
- **Tests:** round-trip (insert → select exact value), `DECIMAL` arithmetic has
  no rounding error, `TIMESTAMP` ordering + range predicates, constraints on
  both.

### P2.b — FLOAT, UUID, BYTEA, DATE, TIME

- `Float` → `f64` (8 bytes LE). `Uuid` → 16 bytes. `Bytea` → variable-length
  opaque bytes (model on the existing `Text`/`Json` variable-length path).
  `Date` → `i32` days since epoch. `Time` → `i64` micros since midnight.
- Same four touch-points each (catalog variant, `Literal`, encode/decode,
  parser); reuse the P2.a pattern. Round-trip + constraint tests per type.

### P2.c — ALTER / DROP / TRUNCATE + transactional catalog

- `ALTER TABLE ADD COLUMN` (with `DEFAULT`), `DROP COLUMN`, `DROP TABLE`,
  `TRUNCATE`. `DROP`/`TRUNCATE` must release the table's heap pages (hand to the
  FSM / free-page list once Phase 1 P1.c lands; until then, mark reclaimable).
- **Make catalog DDL transactional** (today it is not — a documented gap):
  log catalog mutations so `CREATE`/`ALTER`/`DROP` redo/undo with the
  transaction and a failed multi-statement request rolls DDL back too.
- Files: `catalog.rs` (mutation + WAL-logged catalog change), `sql/parser.rs`,
  `sql/logical.rs`, `sql/executor.rs`. Coordinate with the Core lane on the WAL
  record for catalog changes (additive; agree the record shape at land-time).
- Tests: add/drop column round-trips; `DROP TABLE` then re-`CREATE`; DDL
  rollback on a failing `;`-separated request.

### P2.d — Sequences / SERIAL

- A durable, monotonic sequence generator (a catalog-tracked counter, crash-safe
  via the WAL) and a `SERIAL`/`GENERATED` column that auto-fills from it on
  `INSERT` when the column is omitted.
- Files: `catalog.rs` (sequence state), `sql/executor.rs` (fill), `sql/parser.rs`
  (`SERIAL` / `GENERATED ... AS IDENTITY`).
- Tests: concurrent-safe monotonicity (no duplicates), survives reopen.

### P2.e — Prepared statements + bind parameters

- Extend the API to accept **parameterized SQL** (`... WHERE id = $1` + a values
  array) instead of only interpolated strings — closes the **SQL-injection
  surface** and enables **plan reuse** (parse once, execute many).
- Parser recognizes `$n` placeholders → a plan with parameter slots; executor
  binds values by position; server DTO gains a `params: [...]` field on `/sql`.
- Files: `sql/parser.rs`, `sql/logical.rs`, `sql/executor.rs`,
  `server/dto.rs`/`handlers.rs`, `docs/REST_API.md` (document the new shape).
- Tests: a bound query with a value that would be malicious as a string literal
  is treated as data, not SQL; same plan reused across executions.

## Locked decisions touched

| Decision | Effect |
|---|---|
| D9 (little-endian, exact byte encoding) | New fixed-width type encodings; `FORMAT_VERSION` bump when the row-encoding tag set changes |
| D4 (forward-compatible tuple format) | New types slot into the existing tag scheme; old rows still decode |
| Catalog DDL transactionality | Completes the documented M1 gap (P2.c) — a strengthening, not a reversal |
| D6 / D8 (single file, 8 KiB) | Unchanged |

## Verification gates (Phase 2 done =)

- Every new type: insert→select round-trips exactly; ordering/equality correct;
  works under M11 constraints (`DEFAULT`/`CHECK`/PK/UNIQUE).
- `ALTER`/`DROP`/`TRUNCATE` correct; DDL rolls back in a failed transaction.
- `SERIAL` monotonic + crash-safe; sequences survive reopen.
- Prepared statements: no injection surface; documented in `docs/REST_API.md`.
- `clippy -D warnings` + `fmt` clean; own dated subsections in
  `PROGRESS.md`/`MEMORY.md`; one PR per checkpoint; rebase onto `origin/main`
  before each PR.

## Known limitations / deferred

- No `NUMERIC` beyond `i128` range (document the precision cap); arbitrary-
  precision is out.
- No time zones beyond UTC-normalization in v1 (`TIMESTAMPTZ` stores UTC).
- No arrays/enums/composite types yet — additive later on the same machinery.
- Referential-action FKs (`ON DELETE CASCADE`), collations, and generated/
  computed columns are follow-ups, not Phase 2.
