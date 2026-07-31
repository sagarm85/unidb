# unidb-studio G1/G2 completion fixes

**Type:** Improvement
**Status:** SHIPPED (control-plane/catalog only — no `PROGRESS.md` entry by
explicit instruction of the task that shipped this; see the commit and
`tests/studio_g1_g2_fixes.rs` for verification instead of a metrics table)

## What this is

Four small, well-scoped fixes/features requested by the unidb-studio team to
complete the G1 (auth/users) and G2 (RLS/policies) panels. All build on the
existing auth surface (A1–A6, RLS/roles/column-grants B1–B5, rate-limit I1,
auto REST C1, realtime-auth E1) — no new machinery, no parallel
implementations.

1. **Bug fix — `unidb-server-full` ignores `UNIDB_ALLOW_SIGNUP`.**
   `src/bin/unidb-server.rs` read the env var and called
   `AppState::with_allow_signup`; `unidb-server-full/src/main.rs` never did,
   so signup could never be enabled on the storage-capable binary. Fixed by
   mirroring the exact wiring (read env var → `with_allow_signup`).

2. **`unidb_catalog.policies.target_roles`.** `PolicyDef::target_roles`
   (item 122 B4) existed but wasn't exposed in the `unidb_catalog.policies`
   virtual relation, so Studio could author role-scoped policies but not
   display which existing ones were scoped. Added as an **appended** column
   (existing column order/positions unchanged): a comma-joined,
   alphabetically-sorted list of role names for a `CREATE POLICY … TO
   <roles>` policy, or `*` for a no-`TO` policy (applies to every caller —
   the pre-B4 back-compat default).

3. **`ALTER USER <name> PASSWORD '<pw>'`.** Added to the hand-rolled auth
   DDL (`parse_auth_stmt` + `RoleStore::apply`'s `AlterUserPassword`
   variant), superuser-gated exactly like `CREATE USER … PASSWORD` (auth DDL
   already requires superuser at the `execute_sql_as_principal` dispatch
   layer). Errors if the user doesn't exist. Sets the same argon2id
   credential `set_password`/`CREATE USER … PASSWORD` use — reuses
   `hash_password`, no separate credential path. Password is redacted from
   `Debug` and never logged, matching `CreateUser`'s existing posture.

4. **Session listing + revoke-a-specific-session.**
   - `unidb_catalog.sessions` (new virtual relation): `session_id, username,
     created_at, expires_at, revoked`. Per-caller visibility mirrors item
     111: superuser sees all, a named non-superuser sees only their own.
     Never exposes the raw refresh token or its SHA-256 hash.
     `SessionRec::session_id` is a **new, independently-random** field (128
     bits from the OS CSPRNG, a separate draw from the refresh token itself
     — never a hash prefix or other derivation), `#[serde(default)]` so an
     existing `roles.json` still loads; `RoleStore::open` backfills any
     legacy session missing an id on first load and persists the backfill
     once.
   - `DELETE /auth/sessions/{id}` (new REST route, protected/JWT-gated):
     revokes one session by its opaque id. Self/superuser gated — a
     superuser may revoke any session, a named non-superuser only their
     own. Idempotent and shape-uniform: an unknown id and someone else's
     session both return `204` without the foreign session being touched
     (no oracle on session existence/ownership), matching `POST
     /auth/logout`'s existing posture.

## Verification

- New tests: `tests/studio_g1_g2_fixes.rs` (7 tests, all green) — full-binary
  `UNIDB_ALLOW_SIGNUP` wiring (end-to-end through the real
  `AppState`/`build_router` path), `target_roles` rendering (both the
  `TO`-scoped and wildcard cases), `ALTER USER … PASSWORD` (superuser-gated,
  unknown-user error, old-password-fails/new-password-works), and session
  listing/revoke-by-id (isolation between users, idempotent revoke, proof
  the session id is independent of the token/hash by length and
  non-containment).
- `cargo build --all-features`, `cargo build -p unidb-server-full`,
  `cargo clippy --all-features -- -D warnings`, `cargo fmt --all`, `cargo
  test --test crash` (54/54), `cargo test --features server` (existing +
  new suites) all green — see the shipping commit for the exact command
  output.
- `docs/REST_API.md` updated: `ALTER USER … PASSWORD` DDL example,
  `DELETE /auth/sessions/{id}` route section, `unidb_catalog.sessions`
  relation, and the `target_roles` column note.

## Deviations from a full "Improvement" ship per `CONVENTIONS.md`

- No `PROGRESS.md` entry — the task that shipped this explicitly scoped
  docs updates to `docs/REST_API.md` + this backlog note, leaving
  `MEMORY.md`/`PROGRESS.md` untouched. If a future session picks this back
  up for a "real" milestone close-out, add the metrics-table entry there
  then (control-plane/catalog work, no throughput claim to make — the
  entry would be functional-verification only).
