# CAPTCHA / bot protection on auth endpoints

**Type:** Improvement
**Status:** SHIPPED (→ no `PROGRESS.md` entry — task scope excluded it, matching
items 124–130's precedent)

> Workstream I2 (item 120's roadmap: "I — Hardening & ops"). Complements I1's
> volume-based rate limiter (`src/server/rate_limit.rs`) with a third-party
> "was this a human" check on the two credential auth mutation routes.

## What shipped

`POST /auth/login` and `POST /auth/signup` can each require a verified
CAPTCHA token before any credential work runs — a client-supplied
`captcha_token` field, verified server-to-server against a provider's
"siteverify" endpoint before the password is checked (login) or the user is
created (signup).

- **Provider-agnostic** (`src/server/captcha.rs`): Cloudflare Turnstile,
  hCaptcha, and Google reCAPTCHA v2/v3 all speak the identical wire shape —
  `POST secret + response (+ remoteip)` form-encoded, `{"success": bool}`
  JSON back — so one HTTP call (`verify_with_provider`) serves all three;
  only the default siteverify URL differs per provider name. Turnstile ships
  as the default and is the only provider exercised by tests (mocked).
- **Config:** `UNIDB_CAPTCHA_PROTECT` (default **empty = disabled** — the
  back-compat-preserving default), a comma-separated list of `login`/
  `signup`; `UNIDB_CAPTCHA_PROVIDER` (default `turnstile`); `UNIDB_
  CAPTCHA_SECRET` (env fallback); `UNIDB_CAPTCHA_VERIFY_URL` (override, the
  seam tests point at a local mock — no real network/secret needed).
- **Vault-first secret resolution** (item 129, I3): the CAPTCHA secret
  resolves from the vault entry `captcha.secret`
  (`unidb-vault set captcha.secret`) first, falling back to
  `UNIDB_CAPTCHA_SECRET` — identical order/semantics to OAuth's client
  secret (`oauth::resolve_client_secret`).
- **Wired into the handlers** (`handlers.rs::post_auth_login`/
  `post_auth_signup`): the gate runs *after* the route's own enable/
  signing-key checks but *before* password verification / user creation, so
  a failed CAPTCHA check never touches the credential path or creates an
  account. `DTO` fields `AuthLoginRequest`/`AuthSignupRequest.captcha_token`
  are optional (`#[serde(default)]`) and redacted in `Debug`, same posture
  as `password`.
- **Failure contract:** missing/empty token on a protected endpoint ->
  `400 CAPTCHA_TOKEN_REQUIRED`. Bad/expired token, explicit provider
  rejection, or a misconfigured/unreachable verifier -> uniform
  `403 CAPTCHA_FAILED` (no oracle on *why*, mirroring
  `401 INVALID_CREDENTIALS`'s existing uniformity). Fail-closed: nothing
  short of an explicit `{"success": true}` lets a protected request through.
- **Back-compat:** default config protects nothing — every pre-existing
  `item100_auth_endpoints`/`item121_auth_core`/`server_rate_limit` test
  passes unchanged, with zero `captcha_token` in any request body.

## Testing (no real key, no real network)

A local mock axum "siteverify" server (`tests/item131_captcha.rs`,
127.0.0.1:0) returns `{success: true}` only for one canned token; a test
`CaptchaConfig` points `verify_url` at it via the same override seam
`UNIDB_CAPTCHA_VERIFY_URL` gives a real deployment. 5 tests:

1. `signup_and_login_with_valid_token_succeed_when_captcha_enabled`
2. `signup_rejects_missing_token_and_creates_no_user` (proves no orphaned
   account via a subsequent login attempt)
3. `login_rejects_bad_or_missing_token_with_no_session`
4. `captcha_disabled_by_default_is_back_compat`
5. `secret_resolves_from_vault_when_stored_else_falls_back_to_env`

Plus 3 unit tests in `src/server/captcha.rs` (disabled-by-default,
protect-list parsing, default-URL-per-provider).

## Verification

`cargo test --no-run` clean; `cargo build --all-features` clean;
`cargo clippy --all-features --all-targets -- -D warnings` clean;
`cargo fmt --all` clean; crash harness 54/54; `tests/item131_captcha.rs`
5/5; regression `item100_auth_endpoints` 9/9, `item121_auth_core` 16/16,
`server_rate_limit` 5/5, `item127_mfa` 7/7, `item128_oauth` 8/8,
`item129_vault` 3/3 — all unchanged/green.
