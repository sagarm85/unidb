# Supabase parity roadmap — auth, auto-API, and authorization surfaces

**Type:** Milestone
**Status:** NOT STARTED

> Umbrella plan for the work that turns unidb from "a database with the Postgres
> *security primitives*" into a Supabase-class **backend-as-a-service**. It is a
> map + priority + parallelization plan; each workstream that starts gets its own
> numbered spec (Wave-1 tracks already spun out: **121** auth core, **122** RLS↔token
> binding, **123** auto REST API). Wave-2/3 workstreams get their own `NN_…` files
> when they start, per `CONVENTIONS.md`.

## Framing (why this exists)

"Supabase" is not a database — it is a platform built *on top of* Postgres. unidb
already ships the Postgres-layer pieces Supabase relies on, plus vector/graph/
event models in one atomic commit. What is missing is the BaaS layer above the
engine: an **auth service** (create users, log them in, mint tokens), a **rich
token→RLS binding** (`auth.uid()` / `auth.jwt()` / role claims), and an
**auto-generated data API**. Everything else (external identity, realtime
authorization, storage authorization, SDK) stacks on those.

## Already implemented — do NOT re-scope (verified 2026-07-31)

- **RLS engine** — named policies, per-op, `USING` + `WITH CHECK`, `current_user`
  substitution (`src/sql/logical.rs::apply_rls`). Item 24.
- **RBAC** — users/roles/transitive membership + per-table GRANT/REVOKE
  (`src/authz/mod.rs`). Item 24 (Z4 inheritance shipped).
- **Token → identity → RLS is wired** — JWT `sub` → `CurrentUser` →
  `apply_rls(plan, catalog, Some(user))` → `current_user` resolves in policies.
  The connection exists; what is thin is the *richness* (only `sub`, no
  `auth.uid()`/claims/role) and *authentication* (login is passwordless). See 121/122.
- **Login surface exists but is dev-only + passwordless** — `POST /auth/login`
  (`UNIDB_DEV_LOGIN=1`), `/auth/whoami`, `/auth/meta`, user store, `open_mode`
  (items 100, 103). `post_auth_login` only checks the user *exists* — no credential.
- **Storage authorization, partial** — presigned GET/PUT URLs with TTL
  already ship (`unidb-storage`, item 23/31). ~~public/private buckets
  already ship~~ **Correction (2026-07-31, item 125 Step-0 audit):** this was
  false — no `is_public` column existed and no caller identity reached
  `/storage/*` at all (every bucket behaved as public to any authenticated
  caller). F1 (item 125) built both the public/private flag and per-object
  ownership/bypass enforcement; only a richer storage policy-DDL surface
  remains as a documented follow-up (see `125_storage_per_object_authz.md`).
- **unidb-studio** — SQL editor, table editor, schema/ERD, CSV, storage browser,
  events/realtime inspector, observability, logs, compare all ship. Missing only
  the panels whose backend does not exist yet (auth users, policies, roles, API docs).

## Workstreams, priority, and dependencies

Priority: **P0** = critical path to "token-based multi-tenant access"; **P1** =
platform; **P2** = ecosystem.

| WS | Name | Repo | Priority | Depends on | Spec |
|----|------|------|----------|-----------|------|
| A | Auth core (credentials & sessions) | `unidb` | **P0** | — | [`121`](121_auth_core.md) |
| B | RLS ↔ token binding (claims/roles) | `unidb` | **P0** | — | [`122`](122_rls_token_binding.md) |
| C | Auto-generated data API (PostgREST-style) | `unidb` | **P0** (C1) | — | [`123`](123_auto_rest_api.md) |
| D | External identity (OAuth, OTP, MFA, email) | `unidb` | P1 | A | [`127`](127_totp_mfa.md) (D4), [`128`](128_oauth_social_login.md) (D1); D2/D3/D5 file when started |
| E | Realtime authorization (per-subscriber RLS) | `unidb` | **P0** (E1) | B | file when started |
| F | Storage authorization (per-object RLS) | `unidb-storage` | P1 | B | file when started |
| G | Studio panels (auth/policies/roles/API-docs) | `unidb-studio` | P1 | A,B,C | studio `docs/` |
| H | Client SDK (`unidb-js`) | new repo | P1 | A,C | file when started |
| I | Hardening & ops (rate-limit, vault, migrations…) | `unidb` | P0 (I1) | — | file when started |

### Item detail

- **A — Auth core:** A1 credential store (argon2id) · A2 real password login · A3
  signup · A4 refresh tokens + sessions + revocation · A5 production issuer (un-gate
  dev-only) · A6 asymmetric JWT (RS256/ES256) + JWKS + rotation. See 121.
- **B — RLS↔token:** B1 `auth.uid()` · B2 `auth.jwt()->>'claim'` · B3 built-in
  `anon`/`authenticated`/`service_role` · B4 role-scoped policies (`... TO <role>`)
  · B5 column-level security (item 112, shipped 2026-07-31). See 122. Workstream
  B is now fully shipped.
- **C — Auto API:** C1 `/rest/v1/<table>?col=eq.val` (P0) · C2 embedded FK
  expansion · C3 OpenAPI/API-docs · **C4 GraphQL — SHIPPED 2026-08-01**
  (`POST /graphql`, `async_graphql::dynamic`, catalog-rebuilt per request;
  scalar/FK-forward/FK-reverse/`edges`/`near` fields all resolve through the
  identical enforced `/sql` path; see 130). Workstream C is now fully shipped.
  See 123.
- **D — External identity:** **D1 OAuth/social — SHIPPED 2026-07-31**
  (provider-agnostic OAuth 2.0 Authorization Code + PKCE, Google + GitHub:
  `GET /auth/oauth/<provider>/authorize` → `.../callback` resolves
  `(provider, provider_user_id)` to a unidb user — create-only, never
  auto-link-by-email — and issues a session via the existing
  `issue_token_pair` path; tested with a local mock provider, no real
  network/secrets; see `128_oauth_social_login.md`) · D2 magic-link/
  email-OTP · D3 phone-OTP · **D4 MFA/TOTP — SHIPPED 2026-07-31**
  (self-contained RFC 6238 TOTP: enroll → confirm (mints recovery codes) →
  login gate (`mfa_required` + single-use challenge) → challenge redeem
  (reuses the existing session-issuance path) → disable (code or superuser
  bypass); see `127_totp_mfa.md`) · D5 email flows (confirm/reset/invite).
- **E — Realtime auth:** E1 per-subscriber RLS filtering on SSE (P0) · E2
  broadcast + presence · E3 channel authz by token. SSE + events inspector already
  ship — only the authorization layer is new.
- **F — Storage auth:** F1 per-object authorization — **SHIPPED 2026-07-31**
  (owner + public-bucket read exemption + superuser/`service_role` bypass,
  audited; see `125_storage_per_object_authz.md`, correction above — F3
  "bucket public/private" did **not** already ship as this row originally
  claimed; F1 built it). F2 signed URLs already shipped (item 23/31) — do not
  rebuild. A richer storage policy-DDL surface remains a follow-up.
- **G — Studio panels:** G1 Authentication panel (users CRUD/invite/ban/reset) ·
  G2 Policies editor · G3 Roles/grants UI · G4 API-docs panel.
- **H — SDK:** H1 JS/TS SDK (auth+data+realtime) · H2 session persistence + auto-refresh.
- **I — Hardening/ops:** I1 auth rate-limit/brute-force — **SHIPPED** (in-memory
  fixed-window limiter, `src/server/rate_limit.rs`, over `POST /auth/login`,
  `/auth/signup`, `/auth/refresh`; keyed by client IP + route + optional
  username; `UNIDB_AUTH_RATE_LIMIT`/`UNIDB_AUTH_RATE_WINDOW_SECS`; see
  `docs/REST_API.md`'s "Auth rate limiting" section) · I2 CAPTCHA · **I3
  secrets vault — SHIPPED 2026-08-01** (encrypt-at-rest config secrets:
  AES-256-GCM `src/vault.rs` keyed by `UNIDB_MASTER_KEY`, a `SecretStore`
  persisted alongside `roles.json`'s credentials/sessions/MFA, the
  `unidb-vault` CLI, and the OAuth client-secret seam item 128 left wired
  through it — vault-first, env-fallback; see `129_secrets_vault.md`) · **I4
  migrations tooling — SHIPPED 2026-07-31** (Supabase-style
  forward-only `.sql` migrations, `Engine::apply_migrations` + `unidb-migrate`
  CLI, `schema_migrations` tracking table with checksum drift detection; see
  `126_sql_schema_migrations.md` and `docs/SCHEMA_MIGRATIONS.md`) · I5
  connection pooling · I6 management API · I7 edge functions.

## Parallel execution plan

**Wave 1 — start now, independent (specs 121/122/123 ready):**
A · B · C · I1 · G1/G2 UI scaffolding against the A/B contracts.

**Wave 2 — unlocks once A + B land:** D (needs A) · E1 (needs B) · F1 (needs B) ·
H (needs A+C) · G full wiring.

**Wave 3 — polish:** ~~C4 (GraphQL)~~ SHIPPED 2026-08-01 (see 130) · G3/G4 · I2–I7.

**File-contention note for parallel work.** A and B both touch `handlers.rs` and
`logical.rs` lightly. Assign **A** the login/session routes + `authz` credential
store, and **B** the `Expr`/`apply_rls`/executor claim-context path, so merges
stay clean. C, E, F, I, G touch mostly disjoint files.

## Definition of done (this milestone as a whole)

A multi-tenant app can: sign a user up with a password → log in and receive a
verifiable token → have that token's `auth.uid()`/role automatically scope every
query, realtime subscription, and storage object via RLS — with no per-request
app-side authorization code — administered from unidb-studio. Each workstream
carries its own acceptance in its spec; each shipped unit records metrics in
`PROGRESS.md` per §6.
