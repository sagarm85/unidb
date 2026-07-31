# Auth core — credentials, real login, sessions (Workstream A)

**Type:** Milestone
**Status:** NOT STARTED

> Turns the existing dev-only, passwordless identification surface into a real
> authentication service: stored credentials, password login, signup, refresh
> tokens and sessions. Part of the [Supabase parity roadmap](120_supabase_parity_roadmap.md)
> (Wave 1, P0). Independent of Workstream B (122) — coordinate only on the login
> route in `handlers.rs`.

## Problem (verified in code)

- `src/server/handlers.rs::post_auth_login` issues a token after checking only
  that the username *exists* — the doc comment says so: *"passwordless =
  identification, not authentication."* No credential is stored or verified.
- The issuer is gated behind `UNIDB_DEV_LOGIN=1` and 404s otherwise, by design
  "no prod issuer" — so there is no production login path at all.
- There is no refresh/session concept: tokens are 1 h HS256, no revocation.

The user store, roles, grants, `/auth/whoami`, `/auth/meta`, and `open_mode`
already exist (items 100/103/24) — this milestone adds the *authentication* half.

## Scope

### A1 — Credential store (MUST)
- Add a password-credential record per user: `argon2id` hash + params (salt, m/t/p),
  never the plaintext. Store alongside the existing `AuthState` in `src/authz/mod.rs`
  (persisted to `roles.json` — control-plane metadata, `serde` allowed per §4).
- `CREATE USER <name> [SUPERUSER] [PASSWORD '<pw>']` extension in the hand-rolled
  authz DDL parser; plus a Rust-API `set_password(user, pw)`.
- Passwords never logged, never returned by `/auth/whoami` or catalog views.

### A2 — Real login (MUST)
- `post_auth_login` verifies the supplied password against the stored hash
  (constant-time compare via `argon2::verify`). Wrong password → 401, same shape
  as a missing user (no user-enumeration oracle).
- Keep `open_mode` semantics: when no users exist, the server stays open (unchanged).
- Un-gate from `UNIDB_DEV_LOGIN` once real verification exists — but keep the
  issuer key configuration explicit (see A5).

### A3 — Signup (SHOULD)
- `POST /auth/signup { username, password }` → creates the user (non-superuser)
  with an `argon2id` credential and returns a token. Gated by a server policy flag
  (`UNIDB_ALLOW_SIGNUP`, default off) so it is opt-in, not open by default.

### A4 — Refresh tokens + sessions (SHOULD)
- Short-lived access token (existing 1 h) + long-lived refresh token; `POST
  /auth/refresh` exchanges a valid refresh token for a new access token.
- Persisted session table (opaque refresh-token id → user + expiry + revoked flag);
  `POST /auth/logout` revokes. Reuse the heap + an index, not a new store.

### A5 — Production issuer (SHOULD)
- Replace the dev-only gate with an explicit signing-key configuration
  (`UNIDB_JWT_SIGNING_KEY` / secret), so issuance is a first-class, documented
  production capability rather than a demo flag.

### A6 — Asymmetric JWT (COULD, P1)
- Add RS256/ES256 verification + a JWKS endpoint (`GET /.well-known/jwks.json`)
  so external verifiers (and Workstream H's SDK) can verify without the shared
  secret. `jsonwebtoken` supports this natively; `require_jwt` gains an alg branch.

## Touch-points
- `src/authz/mod.rs` — credential store, `set_password`, DDL `PASSWORD` clause.
- `src/server/auth.rs` — issuer config (A5), asymmetric verify (A6).
- `src/server/handlers.rs` — `post_auth_login` verify; new `signup`/`refresh`/
  `logout` handlers.
- `src/server/router.rs`, `src/server/dto.rs` — routes + request/response DTOs.
- Depends on nothing from B; **do not** edit `logical.rs`/executor here.

## Security requirements (gate before ship)
- `argon2id` with sane defaults; constant-time verify; no plaintext at rest or in
  logs; no user-enumeration timing/shape oracle on login.
- Pair with **I1 (rate limiting / brute-force protection)** before any production
  exposure — filed separately in Workstream I.

## Acceptance
- Signup → login-with-password → access + refresh → refresh → logout(revoke)
  end-to-end integration test; wrong-password and revoked-token paths return 401.
- Existing token flow and `open_mode` unchanged (regression tests stay green).
- Crash-harness unaffected (no new WAL record types; credentials are control-plane
  metadata) — but a persisted-session-survives-restart test is required.
- Metrics/outcomes recorded in `PROGRESS.md` per §6.
