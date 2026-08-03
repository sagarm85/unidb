# 148 — Enums + domains (named types v1; composite types deferred)

**Type:** Improvement
**Status:** IN PROGRESS (user go-ahead 2026-08-02; branch `feat/148-enums-domains`)

> Supabase-parity Wave-3 item: `CREATE TYPE … AS ENUM` and `CREATE DOMAIN`,
> the two named-type features that are pure **plan-time desugars over
> machinery that already exists** (catalog + CHECK constraints). **Composite
> types are explicitly NOT in this item** — they change the row-encoding hot
> path (a real format decision) and get their own spec if ever prioritized.

## Design (locked for v1)

**Core idea — desugar at `CREATE TABLE`, reuse CHECK enforcement.** A named
type is a catalog-registered macro. When a column is declared with a named
type, `CREATE TABLE` resolves it immediately:

- `CREATE TYPE order_status AS ENUM ('pending','paid','shipped')` — column
  `status order_status` resolves to base `TEXT` + a synthesized CHECK
  `status IN ('pending','paid','shipped')`, enforced by the **existing**
  CHECK-constraint machinery (zero new enforcement code). The column
  additionally records `type_name: Some("order_status")` (serde-default
  field on `ColumnDef`) for introspection/error messages.
- `CREATE DOMAIN email AS TEXT CHECK (VALUE LIKE '%@%')` — column
  `contact email` resolves to base `TEXT` + the domain's CHECK with `VALUE`
  substituted by the column name. Domain base may be any existing scalar
  `ColumnType`; the CHECK clause is optional.

**Persistence.** Named types live in the `Catalog` (a new serde-default
field, e.g. `types: Vec<NamedTypeDef>`), persisted in the existing
serde_json catalog blob — control-plane-shaped, **no FORMAT_VERSION bump,
no on-disk tuple change** (enum values are stored as their base-type TEXT;
the compact-ordinal encoding Postgres uses is a documented v1 non-goal).

```rust
pub enum NamedTypeKind {
    Enum { labels: Vec<String> },              // non-empty, unique, order preserved
    Domain { base: ColumnType, check: Option<String> }, // check stores the raw VALUE-form expr
}
pub struct NamedTypeDef { pub name: String, pub kind: NamedTypeKind }
```

**DDL surface** (through `execute_sql`, same authz posture as
`CREATE TABLE`; catalog DDL remains non-transactional — same documented,
pre-existing limitation as every other DDL, note it in the docs):

- `CREATE TYPE <name> AS ENUM ('a', 'b', …)`
- `CREATE DOMAIN <name> AS <base-type> [CHECK (<expr using VALUE>)]`
- `DROP TYPE <name>` / `DROP DOMAIN <name>` — **rejected with a clear error
  if any table column references the name** (catalog scan of
  `ColumnDef.type_name`); idempotent-if-absent behavior matches `DROP TABLE`'s
  existing posture (mirror it, whatever it is).
- Parsing: use the `sqlparser` AST if the pinned version parses these
  statements under `GenericDialect`; otherwise use the repo's existing
  custom-DDL pre-parse pattern (precedent: `CREATE INDEX … USING HNSW`,
  M2.c). Either way, `CREATE TYPE`/`CREATE DOMAIN`/`DROP TYPE`/`DROP DOMAIN`
  must produce dedicated `LogicalPlan` variants, not string hacks.
- Name rules: `^[a-zA-Z_][a-zA-Z0-9_]{0,62}$`; names share one namespace
  (a domain and an enum can't collide); a named type may not shadow a
  built-in type name (`INT`, `TEXT`, …, case-insensitive) — reject at
  creation. `CREATE TABLE` with an unknown type name keeps its existing
  error behavior (must remain a clear "unknown type" error, now suggesting
  it may be an undeclared named type).

**Semantics locked for v1 (document each):**
- Enum comparison/ordering is **TEXT collation**, not declaration order
  (Postgres orders by declaration; our v1 stores TEXT — honest, documented
  divergence; declaration-order comparison would need the ordinal encoding,
  deferred with it).
- `ALTER TYPE … ADD VALUE` is **not** in v1: the synthesized CHECKs are
  resolved at `CREATE TABLE` time, so retro-updating them is exactly the
  work v2 would do. Workaround (document): create a new type + new column.
- Casting/coercion: values bind as their base type through the existing
  item-38 coercion; no enum-specific cast rules.

## Files to touch (expected shape — implementer verifies against real code)

- `src/catalog.rs` — `NamedTypeDef`/`NamedTypeKind`, `Catalog.types`
  (serde-default), lookup/register/remove + in-use scan; `ColumnDef.type_name`
  (serde-default).
- `src/sql/logical.rs` — `LogicalPlan::{CreateNamedType, DropNamedType}`
  (one variant pair covering enum+domain is fine).
- `src/sql/parser.rs` — parse the four statements (sqlparser AST or
  pre-parse per the precedent above); resolve named types in `CREATE TABLE`
  column lists → (base type, synthesized CHECK, `type_name`).
- `src/sql/executor.rs` — execute create/drop (validate, persist catalog);
  `CREATE TABLE` path consumes the resolved columns. **No change to
  INSERT/UPDATE enforcement** — that's the point of the desugar.
- `src/error.rs` — dedicated variants (unknown type, duplicate type,
  type-in-use, invalid enum labels/domain def).
- Docs: `docs/sql/sql_reference.md` (new anchored sections for the four
  commands, runnable examples), `README.md` (What's-included SQL bullet),
  `docs/REST_API.md` only if any error-code table entry is warranted.
- `tests/item148_enums_domains.rs` (NOT feature-gated — this is engine SQL,
  no server needed; mirror an existing engine-level SQL test file's setup).

## Required tests

1. Enum end-to-end: create type → create table using it → valid INSERT ok;
   invalid value rejected on INSERT **and** UPDATE with a CHECK-shaped error.
2. Domain with CHECK: valid/invalid INSERT; domain without CHECK = plain
   base-type alias.
3. Persistence: create type + table + row → **reopen the engine** →
   enforcement still applies and `type_name` introspection survives.
4. DROP TYPE in use → error naming the referencing table/column; after
   dropping the table, DROP TYPE succeeds.
5. Duplicate CREATE TYPE name → error; built-in shadow (`CREATE TYPE int …`)
   → error; empty/duplicate enum labels → error.
6. Unknown named type in CREATE TABLE → clear error.
7. Enum column works with the existing machinery it composes with: a B-tree
   index on an enum column; enum column in WHERE equality; NULL handling
   (nullable enum column accepts NULL unless NOT NULL — CHECK must not
   reject NULL, matching SQL CHECK semantics).
8. RLS/grants unaffected smoke: table with enum column + a policy behaves
   as any TEXT column would.

## Verification gates (all, before hand-back)

`cargo build` · `cargo build --features server` · `cargo clippy
--all-features --all-targets -- -D warnings` · `cargo fmt --all -- --check` ·
plain `cargo test --no-run` · new `item148` suite green · existing SQL
suites green (at minimum the constraint/CHECK-related and catalog-related
test files — find and run them) · **crash harness 54/54** (catalog changes
ride the existing catalog-persist path; any crash-harness delta is a real
regression to investigate, not to explain away).

## Follow-ups filed by this item (not in scope)

Composite types (row-encoding format decision); `ALTER TYPE ADD VALUE`
(+ CHECK regeneration); enum ordinal storage + declaration-order comparison;
REST/OpenAPI/GraphQL schema surfacing of named types;
`information_schema` exposure of type definitions.
