# Implicit parameter type coercion in SQL comparisons

**Type:** Improvement
**Status:** SHIPPED 2026-07-20 — see PROGRESS.md "Item 38 — Parameter type coercion"

## Problem

When a parameterized query compares a column of type `Int` against a bound
parameter of type `Text`, the engine rejects it:

```
SQL_UNSUPPORTED · 400  unsupported SQL feature: cannot compare Int(14000) with Text("20")
```

Reproducer (Studio Record Browser filter, or any client):

```json
{
  "sql":    "SELECT * FROM customers WHERE id = $1",
  "params": ["20"]
}
```

PostgreSQL, SQLite, and MySQL all coerce the bound value to the column type
implicitly. unidb currently treats the bound-param type as authoritative rather
than the column/expression type.

## Expected behaviour

When evaluating `expr <op> $n`, if `expr` resolves to a typed value (e.g.
`Int`) and the bound param carries a different but coercible type (e.g.
`Text`), the engine should attempt a lossless coercion:

| Column type | Bound param | Coercion |
|-------------|-------------|---------|
| `Int` / `BigInt` / `SmallInt` | `Text` | `str.parse::<i64>()` — error if non-numeric |
| `Float` / `Double` / `Real` | `Text` | `str.parse::<f64>()` |
| `Bool` | `Text` | accept `"true"/"false"/"1"/"0"/"t"/"f"` |
| `Decimal` / `Numeric` | `Text` | parse as decimal string |
| `Text` | `Int` / `Float` | `to_string()` (safe widening) |

Coercion should only apply to the `=`, `<`, `>`, `<=`, `>=`, `!=` operators.
`LIKE` / `ILIKE` / `MATCH` require `Text` on both sides — no coercion there.

## Why the Studio workaround is not sufficient

The Studio's `bindForColumn()` fix (commit `2dd2e3a`) sends the right type when
it knows the column type from the catalog. But:

1. Any other client (CLI, SDK, direct HTTP) will hit the same error.
2. The Studio only knows the type for explicit column filters — subquery or
   expression filters still hit the raw-text path.
3. It masks an engine correctness gap that will resurface in other contexts
   (e.g. `WHERE CAST(x AS INT) = $1` where `$1` is text).

## Scope

- Coercion point: the expression evaluator, when resolving a binary comparison
  where one side is a typed `Value` and the other is `Value::Text` (or vice versa).
- No DDL or catalog changes needed.
- Affects `SELECT`, `UPDATE … WHERE`, `DELETE … WHERE`.
- Does **not** affect `INSERT` bound params — those are already column-typed
  by the bulk/row insert paths.

## Acceptance criteria

- [ ] `SELECT * FROM t WHERE int_col = $1` with `params: ["42"]` returns rows.
- [ ] `SELECT * FROM t WHERE text_col = $1` with `params: [42]` returns rows.
- [ ] Non-parseable value (`params: ["abc"]` against `Int` column) returns a
      clear `TYPE_MISMATCH` / `400` error, not a crash.
- [ ] Existing typed-param tests still pass (no regression on correct-type paths).
- [ ] 4 new integration tests covering int/float/bool text-coercion and the
      non-parseable error case.
