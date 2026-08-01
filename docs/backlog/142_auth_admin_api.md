# Auth admin API (user management)

**Type:** Improvement
**Status:** SHIPPED (2026-08-01, PR #245) — superuser `/auth/admin/users` (list+pagination/get/create/patch/delete) + per-user `banned` (enforced at login/refresh/verify/magiclink-verify, sessions revoked; access JWT rides expiry) + `app_metadata`/`user_metadata`. Last-superuser + demotion lockout guards on the shared DROP USER path. item142 12/12, crash 54/54.

> Wave-2 free-roadmap item (`137`). Supabase's `auth.admin` gives a REST surface
> for managing users — list (paginated), get, create, update, delete, **ban**,
> and per-user **metadata** — without raw SQL. unidb already has the primitives
> (CREATE/DROP USER DDL, `ALTER USER … PASSWORD` item 124, session list/revoke
> item 124, `unidb_catalog.users`), but no consolidated admin REST surface and
> **no ban flag / user-metadata store**. This adds those. Control-plane only
> (no storage-engine change) — crash harness stays 54/54.

## Scope

### New per-user state (control-plane store, `#[serde(default)]`, no format bump)
- **`banned: bool`** — a banned user is rejected at `POST /auth/login`,
  `/auth/refresh`, and the email flows (recover/magiclink) with a uniform `403
  USER_BANNED` (recover/magiclink still return 200 — no enumeration). *Documented
  limitation:* an already-issued short-TTL **access** JWT stays valid until
  expiry (stateless JWT); ban takes full effect at the next login/refresh, and
  banning also revokes the user's refresh sessions (reuse item-138's
  `revoke_all_sessions_for_user`).
- **`metadata: JSON`** — split into `app_metadata` (admin-only) and
  `user_metadata` (Supabase convention); stored per user, returned in admin
  responses. (Optionally surfaced in the JWT later — out of scope here.)

### REST surface (all superuser-gated, under `/auth/admin/`)
- `GET /auth/admin/users?limit=&offset=` — paginated list (id/username/
  is_superuser/banned/roles/created/metadata; never a password hash or token).
  Return a total count (like item-139's Content-Range or a body field).
- `GET /auth/admin/users/{id}` — one user (404 if absent).
- `POST /auth/admin/users` — create `{username, password?, superuser?,
  app_metadata?, user_metadata?, banned?}` (reuses `create_user_with_password`).
- `PATCH /auth/admin/users/{id}` — update any of: password (→ `set_password` +
  revoke sessions), `banned`, `app_metadata`, `user_metadata`, `superuser`.
- `DELETE /auth/admin/users/{id}` — delete (reuses DROP USER path); reject
  deleting the last superuser / self-lockout guard if one already exists.

Plus `Engine`/`EngineHandle` methods so the embedded crate can do the same.

## Correctness / security
- Every route **superuser-only** (mirror `/realtime/policies`, `/webhooks`);
  non-superuser → 403.
- Never return or log password hashes / tokens / refresh secrets. `app_metadata`
  is admin-writable only; `user_metadata` admin-writable here (a self-service
  `user_metadata` update by the user themselves is a possible follow-up).
- Ban is enforced at auth-decision points (login/refresh/email-flows), fail-safe.
- `banned`/metadata changes are audited (reuse the existing audit path).
- Identifiers validated; reuse existing user/role machinery — no new SQL string
  building of user input.

## Acceptance
- Superuser lists users with `limit`/`offset` and sees a total; a non-superuser
  gets 403 on every `/auth/admin/*` route.
- Create → the user can log in; PATCH `banned=true` → that user's login now
  `403 USER_BANNED` and their refresh sessions are revoked; PATCH `banned=false`
  → login works again.
- PATCH password → old password fails, new works (sessions revoked).
- `app_metadata`/`user_metadata` round-trip through create/get/patch; never a
  hash/token in any response.
- DELETE removes the user; last-superuser guard blocks self-lockout.
- New `tests/item142_auth_admin.rs` (`#![cfg(feature = "server")]` first line).
- Every pre-existing `item100/item121/item124…`/auth test unchanged.
- **Crash 54/54**; `cargo test --no-run` (no features) + `clippy --all-features
  --all-targets -D warnings` + `fmt` clean.
- `docs/REST_API.md` (`/auth/admin/*` routes + ban/metadata semantics + the
  JWT-still-valid-until-expiry note), `README.md`, `137` Wave-2 line, this Status
  flipped on merge.

## Non-goals (v1)
- Putting metadata into the JWT claims (a follow-up; would touch the issuer).
- Self-service `user_metadata` update by the end user.
- Invite-by-email / admin-generate-link flows (email cluster follow-up).
