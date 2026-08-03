# 149 — Row triggers v1 (BEFORE/AFTER INSERT/UPDATE/DELETE)

**Type:** Improvement
**Status:** IN PROGRESS (user go-ahead 2026-08-03; branch `feat/149-row-triggers`,
implemented after item 150 — both touch `exec_insert`, sequential by design)

> Compute-cluster next phase: row-level triggers executing item-147 stored
> functions **inside the same transaction** as the row write. This is where
> the unified-commit thesis pays off: an AFTER trigger's audit row commits
> atomically with the triggering row — no outbox, by construction. This IS
> an ACID-write-path change; the safety comes from triggers' writes being
> ordinary same-txn WAL-logged statements.

## Grammar (locked v1)

```
CREATE TRIGGER <name> {BEFORE|AFTER} {INSERT|UPDATE|DELETE}
  ON <table> [FOR EACH ROW] EXECUTE FUNCTION <fn_name>
DROP TRIGGER <name> ON <table>
```

- `FOR EACH ROW` optional and implied; `FOR EACH STATEMENT` is a v1
  non-goal. One event per trigger (no `INSERT OR UPDATE` lists in v1).
- sqlparser AST route if the pinned version parses `CREATE TRIGGER` under
  GenericDialect (verify empirically, 148-style); else the pre-parse
  precedent.
- `<fn_name>` must be an existing item-147 `FunctionDef` with **zero
  declared params**; its body statements may reference **`NEW.<col>`** and
  **`OLD.<col>`**, bound per fired row as ordinary bind parameters:
  INSERT → NEW only · UPDATE → NEW + OLD · DELETE → OLD only.
  `CREATE TRIGGER` validates at registration: function exists, referenced
  `NEW`/`OLD` columns exist on the table, and the image is available for
  the event type (BEFORE-INSERT referencing OLD = error at CREATE time).

## Semantics (locked v1)

- **Same transaction, synchronous, per row.** BEFORE triggers run before
  the row write; AFTER triggers run after it (post-image final). Multiple
  triggers on the same (table, timing, event) fire in **name order**
  (deterministic).
- **Errors veto.** Any error in a trigger body aborts the whole statement
  exactly like any other statement error (existing txn/abort machinery —
  nothing new). This makes BEFORE triggers the validation hook.
- **BEFORE cannot modify NEW** — v1 non-goal (needs function-returns-row).
  The `updated_at`/`updated_by` stamping use case is served by the
  documented AFTER pattern below.
- **No cascading (the recursion rule):** statements executed from within a
  trigger body do NOT fire triggers. This is v1's entire recursion story —
  simple, safe, and it makes the canonical stamp pattern legal:
  `AFTER UPDATE ON t` → fn body `UPDATE t SET updated_at = <now expr>
  WHERE pk = NEW.pk` terminates because the inner UPDATE fires nothing.
  Document this rule loudly (it diverges from Postgres, where triggers
  cascade and recursion is the user's problem).
- **Privilege model:** `CREATE TRIGGER`/`DROP TRIGGER` are superuser-only
  DDL (v1). The trigger body executes **unrestricted (embedded identity)**
  — admin-authored code, the exact trust posture of cron's default
  `run_as: None`. Rationale: running as the statement's caller would break
  the audit-table pattern (callers lack grants on the audit table) and
  Postgres solves that with SECURITY DEFINER, which is precisely
  "definer = the superuser who created the trigger" here. An invoker
  option is a follow-up. Document this prominently in REST_API/sql docs.
- **Persistence:** `TriggerDef {name, table, timing, event, function}` in
  the catalog (serde-default map, no FORMAT_VERSION bump — 148's
  precedent). `DROP TABLE` drops its triggers. `DROP FUNCTION`-equivalent
  (`DELETE /functions/{name}`) is **rejected** while a trigger references
  the function (mirror 148's in-use rule).
- **No-trigger fast path:** the per-statement setup checks the catalog
  once; tables without triggers must take exactly today's code path with
  zero added per-row cost. Statement plans of trigger bodies are parsed
  once per firing statement, not once per row.

## Engine mechanics (expected shape — implementer verifies)

`exec_insert`/`exec_update`/`exec_delete` gain a per-row
`fire_triggers(timing, event, old, new)` hook that executes the function's
statements through nested plan+execute against the **same ctx/xid** (locks
already held by this txn are re-entrant by design — same lock owner). The
nested execution must not re-enter trigger firing (the recursion rule) —
thread a `in_trigger: bool` (or depth-0 flag) through the exec context, not
a global.

## Required tests

1. CREATE/DROP round-trip + validation errors (unknown fn, fn with params,
   bad column ref, OLD on INSERT, duplicate name); DROP TABLE cleans up;
   function-in-use-by-trigger deletion rejected.
2. BEFORE veto: body error → statement aborts, no row and no trigger side
   effects persist (both same-txn).
3. AFTER audit pattern: INSERT on `t` writes an audit row via trigger; both
   visible after commit; neither after an abort.
4. The stamp pattern: AFTER UPDATE self-UPDATE via NEW.pk terminates (no
   recursion) and lands the stamp.
5. NEW/OLD binding correctness for all three events (values, NULLs).
6. Name-order firing determinism (two triggers, observable order).
7. Non-superuser caller's statement still fires the trigger; trigger body
   succeeds against a table the caller has no grants on (documented
   privilege model); non-superuser CREATE TRIGGER → denied.
8. **Crash test:** AFTER INSERT trigger writes an audit row; crash injected
   between the two writes' WAL records and after both (existing harness
   points); reopen → both rows or neither, never one.
9. No-trigger fast path regression: full existing suites green
   (`constraints`, RLS suites, item150 upsert suite — upsert's update arm
   must fire UPDATE triggers; add one test proving it).
10. Concurrency smoke: two writers on a triggered table — no hang, counts
    correct.

## Verification gates

Standard set (build both, clippy `-D warnings`, fmt, plain `--no-run`,
item149 suite, item150 + constraints + RLS regressions) + **crash harness
green including the new point**. Any perf-relevant claim ("zero cost
without triggers") backed by code-path reasoning in the PR text, and a
follow-up flag to watch the next full bench report per §0.6.

## Non-goals (document each)

FOR EACH STATEMENT · WHEN clauses · UPDATE OF <cols> · INSTEAD OF ·
NEW modification in BEFORE · cascading/recursive triggers · invoker-mode
bodies · auth hooks (separate item, consumes this + 147) · GraphQL/REST
trigger admin surface beyond SQL DDL.
