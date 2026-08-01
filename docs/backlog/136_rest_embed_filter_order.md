# /rest/v1 embedded-resource filtering + ordering

**Type:** Improvement
**Status:** IN PROGRESS

> Supabase/PostgREST-parity gap (item 123 C2 follow-up), confirmed 2026-08-01 by
> the unidb-studio session. `/rest/v1` embedded expansion (`select=id,orders(id,
> total)`) today fetches the embedded resource **projection-only**: `SELECT
> <cols> FROM <embed> WHERE <join_col> IN (<parent keys>)` — no per-embed WHERE
> filter, no `ORDER BY`, no `LIMIT`/`OFFSET` (see `rest_resource.rs::
> fetch_embedded`). PostgREST lets you filter/order the embedded resource with
> dotted params. This adds that.

## Scope

Dotted per-embed query params, PostgREST-style, where the prefix names an
embedded relation present in `select=`:

- **Filter:** `<embed>.<col>=<op>.<val>` (same operator grammar as top-level
  filters: `eq/neq/gt/gte/lt/lte/like/ilike/in/is`). AND-combined with the
  existing `<join_col> IN (...)` clause on the embed's second query.
- **Order:** `<embed>.order=<col>.<asc|desc>[,<col2>...]` (same grammar as the
  top-level `order`).
- **Pagination:** `<embed>.limit=<n>` / `<embed>.offset=<n>` — applied
  **per-parent** (correct lateral semantics, matching PostgREST), i.e. after the
  embedded rows are stitched back to their parent by join key, each parent's
  group is sliced `offset..offset+limit` in the requested order. (A single
  combined `LIMIT` on the `IN (...)` query would wrongly cap across all parents —
  do NOT do that.)

Applies to **forward and reverse** embeds alike (both go through
`fetch_embedded`).

## Enforcement (must remain intact)

- The embedded query already runs through `run_stmt` →
  `execute_sql_params_as_principal` under the caller's `AuthPrincipal`, so a
  filter/order on an **ungranted column** is denied by the existing plan-time
  column-grant check exactly as a direct `GET` on the embedded table would be —
  **no new enforcement path**. Add a test proving this parity.
- Filter/order column **names** are catalog-validated + quoted (reuse
  `validate_column`/`quote_ident`); **values** are `$n` binds (never
  interpolated) — identical to the top-level path.
- RLS on the embedded table continues to apply (it already does).
- A dotted param whose prefix is **not** an embedded relation in `select=` → a
  clear `400` (don't silently ignore), matching how an unknown top-level param /
  column is handled.

## Implementation notes
- Parse the query string once: split params into (a) top-level (no dot, or a
  literal column that happens to contain a dot — but our identifiers don't, so
  first-dot split is safe) and (b) `<embed>.<rest>` groups keyed by embed name.
  Reserve `<embed>.{order,limit,offset}`; everything else under a prefix is a
  filter on that column.
- Thread the per-embed `{filters, order, limit, offset}` into `fetch_embedded`;
  append the WHERE (AND with the `IN`), append `ORDER BY` (include `join_col`
  first so grouping is stable, then the requested order), and do the
  per-parent limit/offset slice in Rust during the existing stitch step.
- Consider factoring the top-level filter/order parsing so the embed path reuses
  it rather than forking (`parse_order_value`, the filter parser, `append_where`
  are already `pub(super)`).

## Acceptance
- `GET /rest/v1/customers?select=id,orders(id,total)&orders.total=gt.100&orders.order=total.desc&orders.limit=3`
  returns, per customer, only their orders with total>100, ordered desc, at most 3.
- A filter/order on a column the caller lacks a grant for is denied identically to
  a direct `GET` on the embedded table (**parity test vs `/sql`/direct GET**).
- Unknown embed prefix → 400.
- Every pre-existing `server_rest` test unchanged; new cases in `server_rest`
  (or `tests/item136_embed_filter_order.rs`, `#![cfg(feature = "server")]` first
  line).
- `cargo test --no-run` (no features) + `clippy --all-features --all-targets -D
  warnings` + `fmt` clean; **crash harness 54/54**.
- `docs/REST_API.md` `/rest/v1` embed section documents the dotted params;
  `README.md` if it lists embed capabilities; flip this Status on merge.

## Non-goals (v1)
- GraphQL nested-field filter/order args (separate surface — flag as a follow-up
  if wanted; the studio's ask was `/rest/v1`).
- Cross-embed ordering / deep (>1 level) embed filters.
