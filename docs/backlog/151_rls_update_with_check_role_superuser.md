# 151 — RLS `UPDATE` WITH CHECK ignores policy `TO <role>` scoping and superuser bypass

**Type:** Improvement
**Status:** NOT STARTED
**Numbering note:** filed by the studio session as `149` concurrently with the
triggers item; renumbered to 151 in the item-150 merge (149/150 were already
baked into shipped code/tests/PRs — see `backlog_index.md`'s header note).
**Related:** the item-133/147 finding that a *named* superuser is not exempt
from per-row INSERT `WITH CHECK` (`sql/executor.rs::exec_insert`, item-24 Z1
path) is the same enforcement family on the INSERT side — fix both ops in one
considered pass rather than patching UPDATE alone.

> Filed 2026-08-03 from a unidb-studio walkthrough session. A row-level-security
> policy scoped `FOR UPDATE TO <role>` has its `WITH CHECK` predicate enforced
> against **every** caller — including callers who are **not members of that
> role** and including a **superuser**. The `SELECT`/`USING` read path does *not*
> have this bug (it correctly honours `TO <role>` and superuser bypass), so the
> defect is isolated to the write-side `WITH CHECK` evaluation on `UPDATE`.

## Summary

For a `FOR UPDATE TO <role>` policy, the engine evaluates the policy's `WITH
CHECK` (the new-row predicate) for callers the policy should not apply to:

1. **`TO <role>` scoping is ignored** — a caller who is not a member of the
   policy's target role is still subject to its `WITH CHECK`.
2. **Superuser is not bypassed** — a superuser (who bypasses RLS everywhere
   else) is blocked by the `WITH CHECK`.

The `SELECT` path (`USING`) behaves correctly for the *same* policy shape, which
localizes the bug to the `UPDATE` `WITH CHECK` enforcement path.

## Reproduction

Seed a role-scoped update policy (as done by the Studio demo `seed_auth.py`):

```sql
CREATE ROLE ops;
CREATE POLICY ord_ops_pending ON orders
  FOR UPDATE TO ops
  USING (status = 'pending');          -- no explicit WITH CHECK → defaults to USING
```

Now act as the **superuser** `dev` (`GET /auth/whoami` → `"is_superuser": true`),
who is **not** a member of `ops`:

```text
-- (A) superuser UPDATE that KEEPS status = 'pending' on a pending order
UPDATE orders SET total_amount = total_amount + 1 WHERE id = <pending id>;
→ {"type":"updated","count":1}            -- OK: new row still satisfies the predicate

-- (B) superuser UPDATE on a NON-pending order, changing NOTHING about status
UPDATE orders SET total_amount = total_amount + 1 WHERE id = <shipped id>;
→ ERROR: new row violates WITH CHECK policy for table "orders"   -- BUG

-- (C) control: the analogous SELECT-side policy IS correctly bypassed
--     (cust_support_de = FOR SELECT TO support USING (country='DE'))
SELECT COUNT(*) FROM customers;           → 1000   -- OK: superuser sees all rows
```

(B) is the bug: `dev` is a superuser, is not in `ops`, and only touched a
non-`status` column — yet the `TO ops` policy's `WITH CHECK` (`status =
'pending'`) is enforced against it because the pre-existing row's status is not
`'pending'`. (A) only "passes" because the new row happens to satisfy the
predicate — it is not evidence of a bypass.

## Expected vs actual

| Caller | Policy | `SELECT` (USING) | `UPDATE` (WITH CHECK) |
|--------|--------|------------------|-----------------------|
| superuser (`dev`) | `TO ops` | bypassed ✅ (evidence C) | **enforced ❌ (evidence B)** — should be bypassed |
| non-member of `ops`, non-superuser | `TO ops` | not applied (correct) | **applied ❌** — should not apply |
| member of `ops` | `TO ops` | applied ✅ | applied ✅ (correct) |

Expected: a `FOR UPDATE TO <role>` policy's `WITH CHECK` applies **only** to
non-superuser callers who are members of `<role>` — identical gating to the
`USING`/`SELECT` path.

## Impact

- **Data-integrity footgun.** While such a policy exists, *no one* — not even a
  superuser — can change the guarded column on rows that don't already satisfy
  the predicate (here: no order can leave `'pending'`). This silently breaks
  legitimate workflows and admin/maintenance writes.
- **Surfaced in the demo:** `demo/events_demo.py`'s status `UPDATE`s (the live
  CDC demo) were rejected once `ord_ops_pending` was seeded; the walkthrough had
  to drop the policy around the capture. `docs`/Studio `DEMO_GUIDE.md` should
  note this until fixed.
- Contradicts Postgres RLS semantics, which unidb otherwise mirrors: a superuser
  (and any non-`TO`-role caller) bypasses the policy on both read and write.

## Root-cause hypothesis (to confirm in code)

The `SELECT` planner path gates policy application on `(a)` superuser bypass and
`(b)` target-role membership before injecting the `USING` predicate; the
`UPDATE` planner path injects the `WITH CHECK` predicate **without** the same two
guards. Fix is to route the `WITH CHECK` policy selection through the identical
"does this policy apply to this principal?" predicate the `USING` path already
uses (superuser → skip; `target_roles` non-`*` → require membership), rather than
enforcing every table policy's `WITH CHECK` unconditionally.

## Acceptance

- A `FOR UPDATE TO <role>` policy's `WITH CHECK` is **not** enforced for a
  superuser, nor for a non-member of `<role>` (evidence-(B) case succeeds).
- It **is** still enforced for members of `<role>` (the intended write-side RLS
  still works — a member cannot move an order off `'pending'`).
- `WITH CHECK` on an **unscoped** (`TO *` / no `TO`) policy still applies to all
  non-superusers, unchanged.
- Regression test covering all three principals × {SELECT, UPDATE} for a
  `TO <role>` policy; crash harness unaffected (planner-only change).
- `DEMO_GUIDE.md` note removed once shipped.

## Provenance

Discovered 2026-08-03 building the unidb-studio visual walkthrough, when
`seed_auth.py`'s write-side example policy (`ord_ops_pending`, `FOR UPDATE TO
ops`) blocked the superuser-run `events_demo.py` UPDATEs. Characterized with the
(A)/(B)/(C) probes above against a live `unidb-server-full`.
