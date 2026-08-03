# /rest/v1 count + Prefer response controls

**Type:** Improvement
**Status:** SHIPPED (2026-08-01, PR #242) — `Prefer: count=exact` → RLS-respecting `Content-Range`; `Prefer: return=representation|minimal` on POST/PATCH/DELETE (RETURNING *); unknown prefs ignored, `Preference-Applied` echoed. REST-layer only, no-Prefer responses byte-identical. Crash 54/54.

> Wave-1 free-roadmap item (`137`). PostgREST-parity response controls that
> clients (incl. supabase-js / unidb-js) expect: **exact-count** on collection
> GETs and the **`Prefer`** header for mutation return shape. Pure REST-layer —
> no SQL-engine change, no ACID/perf impact, crash harness untouched.
>
> **NOTE — upsert was NOT in this item.** PostgREST upsert (`Prefer:
> resolution=merge-duplicates` / `on_conflict=`) maps to `INSERT … ON CONFLICT
> DO UPDATE`, which the SQL engine did **not** support at the time this item
> shipped (verified: no `ON CONFLICT` in parser/logical/executor). Doing a
> read-then-write "upsert" at the REST layer would have been racy /
> non-atomic and was explicitly rejected. **Resolved by item 150** (`docs/
> backlog/150_upsert_on_conflict.md`), which added real `ON CONFLICT` support
> to the SQL engine and wired `on_conflict=` + `Prefer: resolution=
> merge-duplicates|ignore-duplicates` into `POST /rest/v1/<table>` — see that
> item's spec and `docs/REST_API.md`'s `Prefer` section for the shipped
> behavior.

## Scope

### 1. Exact count on `GET /rest/v1/<table>` — `Prefer: count=exact`
- When the request carries `Prefer: count=exact`, after running the normal
  (possibly `limit`/`offset`-paginated) SELECT, run a second
  `SELECT COUNT(*) FROM <table> [WHERE <same filters>]` through the **same
  enforced `run_stmt` path** (same RLS/grants/binds — the count must reflect
  only rows the caller can see) and return a **`Content-Range`** response header:
  `Content-Range: <from>-<to>/<total>` where `<from>-<to>` is the returned
  row window (offset..offset+returned-1, or `*` when empty) and `<total>` is the
  exact count. Body is unchanged.
- Without the header, behavior is exactly as today (no extra query — zero cost).
- `count=exact` is the only planned mode; `count=planned`/`estimated` are
  documented as not supported (we have no planner row estimate to expose).

### 2. `Prefer: return=` on mutations (`POST`/`PATCH`/`DELETE`)
- **`return=representation`** — the mutation returns the affected rows (the
  handler already builds `RETURNING`-style results for POST; PATCH/DELETE return
  a count today, so representation must re-fetch or use RETURNING under the same
  principal). Return them as the body.
- **`return=minimal`** — return `204 No Content` (or 201 for POST) with an empty
  body, just the status + any `Location`-style metadata already emitted.
- **Default when no `Prefer`** — keep today's current behavior (do not silently
  change existing responses; document whatever today's default is and keep it).

### Header parsing
- Parse `Prefer` case-insensitively; it may carry multiple comma-separated
  preferences (e.g. `return=representation, count=exact`). Unknown preferences
  are ignored (PostgREST posture), not an error. Reflect applied preferences in
  a `Preference-Applied` response header (PostgREST does this) — optional but
  nice; at minimum don't error on unknown ones.

## Enforcement / correctness
- The COUNT query runs through the identical `run_stmt` →
  `execute_sql_params_as_principal` path — RLS + grants apply, so the count never
  leaks rows the caller can't see. Same binds as the main filter.
- No new SQL string interpolation of user values (COUNT reuses the same
  `append_where` + binds).
- Handler signature changes (returning headers) must keep every existing
  response byte-identical when no `Prefer`/count header is present.

## Acceptance
- `GET …?limit=2` with `Prefer: count=exact` over a 5-row (caller-visible) table
  → 2 rows in the body + `Content-Range: 0-1/5`.
- Count respects RLS: a caller who can see 3 of 10 rows gets `/3`, not `/10`
  (parity test against a direct `SELECT COUNT(*)` as that principal).
- `POST` with `Prefer: return=minimal` → no body; with `return=representation`
  → the inserted row(s). `PATCH`/`DELETE` likewise.
- No-`Prefer` requests are unchanged (regression: existing `server_rest` tests
  pass untouched).
- New `tests/item139_rest_count_prefer.rs` (`#![cfg(feature = "server")]` first
  line).
- **Crash 54/54**; `cargo test --no-run` (no features) + `clippy --all-features
  --all-targets -D warnings` + `fmt` clean.
- `docs/REST_API.md` (`Prefer`/`count`/`Content-Range` documented), `137` Wave-1
  line, this Status flipped on merge.

## Non-goals
- Upsert / `ON CONFLICT` — was out of scope for this item; shipped separately
  by item 150 (see the NOTE above).
- `count=planned/estimated`; `Range` request header pagination (offset/limit
  already cover it).
