# Auth hardening: leaked-password check + more OAuth provider presets

**Type:** Improvement
**Status:** SHIPPED (2026-08-01, PR #246) — HIBP k-anonymity leaked-password check (opt-in `UNIDB_PASSWORD_HIBP_CHECK`, fail-open on outage, `422 PASSWORD_COMPROMISED`) at signup/admin-create/patch/reset; +5 OAuth presets (apple/azure/gitlab/discord/facebook — Apple userinfo via id_token documented as a gap). item143 6/6, crash 54/54.

> Wave-2 free-roadmap item (`137`) — two small, self-contained, **free**
> auth-hardening additions bundled into one PR. Control-plane only, no
> storage-engine change, crash harness stays 54/54.

## 1. Leaked-password protection (HaveIBeenPwned)
Supabase can reject passwords found in known breaches. HIBP's **Pwned Passwords
range API is free, needs no key, and is privacy-preserving (k-anonymity)**: send
only the first 5 hex chars of the SHA-1 of the password; the API returns all
suffixes in that bucket; match locally. No password ever leaves the server.

- **Where enforced:** every password-set point — `POST /auth/signup`, admin
  `POST/PATCH /auth/admin/users` (item 142), and `POST /auth/verify` (password
  reset, item 138). If the password's hash-suffix is in the breach set → reject
  with `422 PASSWORD_COMPROMISED` (a clear, non-generic code) before storing.
- **Opt-in:** `UNIDB_PASSWORD_HIBP_CHECK` (default **off** — it needs an outbound
  call and some deployments are offline/air-gapped; on = enforce). When on but
  the HIBP endpoint is unreachable, **fail OPEN with a warning** (don't lock
  users out of signup because an external service is down) — documented; a
  fail-closed toggle is a possible follow-up.
- `UNIDB_PASSWORD_HIBP_URL` override so tests point at a local mock (no real
  network). SHA-1 here is used ONLY as HIBP's bucketing key, never for storage
  (passwords are still argon2id) — document that explicitly so it's not mistaken
  for weak hashing.

## 2. More OAuth provider presets
Item 128's OAuth is already provider-agnostic (any provider works via
`UNIDB_OAUTH_<P>_AUTHORIZE_URL`/`_TOKEN_URL`/`_USERINFO_URL`/`_SCOPE`), and
`UserInfo::provider_user_id` already normalizes `sub`/`id` (OIDC + GitHub). This
just adds **recognized presets** so operators supply only client id/secret:
- Extend `default_urls()` + the preset loop (currently `["google","github"]`)
  with **Apple, Microsoft/Azure AD, GitLab, Discord, Facebook** — their
  authorize/token/userinfo URLs + default scope. All use `sub` or `id`, so no
  new extraction code. A provider still needs its `_CLIENT_ID`/`_CLIENT_SECRET`/
  `_REDIRECT_URI` (or vault secret) to activate; unconfigured presets stay 404.
- Any provider whose userinfo genuinely needs a non-`sub`/`id` field is out of
  scope here (documented) — the env-override path still lets an operator wire it.

## Correctness / security
- HIBP: only the SHA-1 prefix leaves the server (k-anonymity); the response is
  matched locally; the full password/hash is never transmitted. argon2id storage
  unchanged. Fail-open-with-warning on outage (documented).
- OAuth presets: pure config additions; the existing single-use/TTL/PKCE/
  state-pinning flow is untouched. Secrets stay vault-first + redacted.
- No new SQL, no storage-engine change.

## Acceptance
- With `UNIDB_PASSWORD_HIBP_CHECK=1` + a mock HIBP returning the test password's
  suffix: signup / admin-create / password-reset with that password →
  `422 PASSWORD_COMPROMISED`; a non-breached password succeeds; with the check
  off, both succeed (back-compat). HIBP endpoint unreachable + check on → signup
  still succeeds (fail-open) + a warning is logged.
- A newly-preset provider (e.g. `discord`) with a local mock authorize/token/
  userinfo + configured client id/secret completes the flow and issues a session;
  an unconfigured preset is 404.
- New `tests/item143_auth_hardening.rs` (`#![cfg(feature = "server")]` first line),
  local mocks only (no real network / secrets).
- Every pre-existing `item121`/`item128`/`item138`/`item142` auth test unchanged.
- **Crash 54/54**; `cargo test --no-run` (no features) + `clippy --all-features
  --all-targets -D warnings` + `fmt` clean.
- `docs/REST_API.md` (HIBP env + `PASSWORD_COMPROMISED` code + the new provider
  presets list), `README.md`, `137` Wave-2 line, this Status flipped on merge.

## Non-goals (v1)
- A password-strength/complexity policy engine (HIBP is breach-check only).
- Fail-closed HIBP mode; per-provider non-standard userinfo mapping.
