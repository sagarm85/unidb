# 147 — Stored SQL functions (v1) + RPC (`POST /rest/v1/rpc/<fn>`)

**Type:** Improvement
**Status:** SHIPPED (2026-08-03, PR #253 merged — see PROGRESS.md "Item 147 —
Stored SQL functions v1 + RPC"; the compute-cluster HELD was lifted by user
go-ahead 2026-08-02, and this item was the cluster's foundation)

> Supabase-parity Wave-2 compute cluster, phase 1 of N. This item ships
> **stored SQL functions as a control-plane object + the RPC route** — the
> two pieces that need NO engine/WAL/heap/catalog change. Triggers and
> `INSERT … ON CONFLICT` (upsert) are the *actual* ACID-write-path items and
> are explicitly **NOT in this item** — they get their own `NN_` specs after
> this lands (re-derived ROI order per CLAUDE.md §0.6: unlock RPC now at
> zero engine risk; take the engine risk in its own, smaller, reviewable
> item).

## Design (locked for v1)

**Model.** A stored function is a named, parameterized list of SQL
statements, persisted as a control-plane object exactly like
`CronJobDef`/`WebhookDef` (`src/authz/mod.rs`, `AuthState` + `#[serde(default)]`
field in `roles.json`, no FORMAT_VERSION bump):

```rust
pub struct FunctionDef {
    pub name: String,            // unique, upsert key; ^[a-zA-Z_][a-zA-Z0-9_]{0,62}$
    pub params: Vec<String>,     // declared parameter NAMES, in $1..$n order; unique, same pattern
    pub body: Vec<String>,       // 1+ SQL statements; $1..$n refer to params by position
    #[serde(default)]
    pub run_as: Option<String>,  // None = INVOKER (the RPC caller); Some(role) = definer-analog
}
```

**Security — the one place this deliberately differs from cron:**
`run_as: None` means **the calling principal** (invoker semantics — the
caller's own RLS/grants apply, PostgREST's default posture), NOT the
embedded superuser. Cron's `None = admin` default is safe because only a
superuser can register *and* only the scheduler can fire a job; RPC is
callable by **any authenticated principal**, so an implicit-admin default
would be a privilege-escalation hole. A definer-style function must say so
explicitly (`run_as: "some_role"`). Registration stays superuser-only, so
`run_as` is admin-granted, same trust model as cron's.

**Execution (RPC).** `POST /rest/v1/rpc/{fn}` with a JSON object body
(named args: `{"a": 1, "b": "x"}`, matched to `params` by name → positional
`$1..$n`) or a JSON array (positional). All body statements run **in one
transaction** (one `begin` → each statement via
`execute_sql_params_as_principal` with the same bind vector → `commit`;
any error → `abort` → the whole call rolls back atomically). Response:
the **last** statement's rows as JSON (same row-shape `/sql` returns),
`200`. Params are bound through the existing item-38 coercion layer — no
declared types in v1 (documented simplification).

**Admin surface** (mirror `/cron/jobs` exactly — superuser-gated via
`ensure_superuser`, idempotent delete):
- `POST /functions` — upsert `{name, params?, body, run_as?}`. Validation:
  name/param patterns, params unique, body non-empty with every statement
  non-empty, `$k` references in body within `1..=params.len()`
  (reject-at-registration, not at call time, where checkable by a simple
  scan outside string literals — document the limits of the scan).
- `GET /functions` — list (full defs; superuser-only so body exposure is fine).
- `DELETE /functions/{name}` — idempotent.

**Errors** (register in `docs/REST_API.md`'s table):
- `404 FUNCTION_NOT_FOUND` — RPC on an unknown name.
- `400 INVALID_FUNCTION_ARGS` — missing/extra/mismatched args.
- `400 INVALID_FUNCTION_DEF` — registration validation failures.
- SQL execution errors map exactly as `POST /sql` maps them (reuse the
  existing `ApiError` conversion, no new mapping layer).

**Explicit non-goals (v1, document each in REST_API.md):** no SQL-callable
`SELECT my_fn(...)` surface (that is the plpgsql-analog future step); no
`GET /rest/v1/rpc/<fn>`; no declared param types; no return-type contract
beyond "last statement's rows"; no triggers; no upsert; no auth hooks
(next phases). No engine/WAL/heap/catalog/on-disk change of any kind.

## Files to touch (mirror item 144's shape)

- `src/authz/mod.rs` — `FunctionDef`, `AuthState.functions` (`#[serde(default)]`),
  `upsert_function` (validates; `DbError` variant → 400), `remove_function`,
  `list_functions`, `get_function`; Debug impl field.
- `src/error.rs` — `InvalidFunctionDef(String)` variant (or reuse pattern of
  `InvalidCronSchedule`).
- `src/lib.rs` — `Engine::{upsert_function, remove_function, list_functions, get_function}`.
- `src/server/engine_handle.rs` — async wrappers (mirror cron's).
- `src/server/dto.rs` — `FunctionUpsertRequest`, `FunctionDto`.
- `src/server/handlers.rs` — `post_function`/`get_functions`/`delete_function`
  (mirror cron handlers) + `rpc_call` (the transaction loop above; extract the
  caller principal the same way `POST /sql`'s handler does).
- `src/server/router.rs` — `/functions`, `/functions/{name}`, and
  `/rest/v1/rpc/{fn}` routes (RPC goes in the same authenticated layer as
  the other `/rest/v1` routes).
- `docs/REST_API.md` — new "Stored functions & RPC (item 147)" section +
  error-code rows. `README.md` — What's-included bullet + curl example.
- `tests/item147_stored_functions_rpc.rs` — `#![cfg(feature = "server")]`.

## Required tests (all in the new file; use `TestServer` like item 144's)

1. Register/list/delete round-trip; delete idempotent; non-superuser gets 403
   on all three admin routes.
2. Registration validation: bad name, dup params, empty body, `$3` with 2
   params → `400 INVALID_FUNCTION_DEF`.
3. RPC named args + positional args produce identical results.
4. **Invoker RLS parity (the critical test):** table with an RLS policy;
   function `SELECT`s it; calling as alice via RPC returns exactly what
   alice's direct `/sql` SELECT returns (not the unfiltered set); calling as
   bob returns bob's. A `WITH CHECK`-violating INSERT through RPC is rejected
   exactly as via `/sql`.
5. `run_as` definer-analog: function with `run_as` of a scoped role behaves
   as that role regardless of caller; caller still must be authenticated.
6. **Atomicity:** 2-statement body where statement 2 fails → statement 1's
   insert is NOT visible afterward (rolled back).
7. Unknown fn → 404; wrong arg names/count → 400.
8. Params flow through coercion: INT + TEXT args round-trip.

## Verification gates (every one, before hand-back)

`cargo build --features server` · `cargo clippy --all-features --all-targets
-- -D warnings` · `cargo fmt --all -- --check` · plain `cargo test --no-run`
(NO features — new test file must be `#![cfg(feature = "server")]`-gated) ·
new `item147` suite green · pre-existing `item144_cron` + `server_rest` +
`server_authz` suites green (regression) · **crash harness `cargo test
--test crash` 54/54** (must be untouched — this item has no storage-side
change to excuse any delta).

## Follow-ups filed by this item (not in scope)

Triggers (BEFORE/AFTER row — engine write path); upsert `ON CONFLICT`
(engine); auth hooks (control-plane, consumes this item's functions); SQL
grammar surface `CREATE FUNCTION`/`SELECT fn()`; GraphQL exposure of
functions; OpenAPI (`GET /rest/v1/`) listing of RPC routes.
