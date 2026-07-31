# OAuth 2.0 social login (Google + GitHub)

**Type:** Improvement
**Status:** SHIPPED (2026-07-31)

> Workstream D1 (item 120's roadmap: "D — External identity (OAuth, OTP,
> MFA, email)"). Provider-agnostic OAuth 2.0 Authorization Code + PKCE
> social login for the auth core shipped in items 121/122/127 — buildable
> and fully testable with **no real provider secrets** via a mock OAuth
> provider in tests; real Google/GitHub credentials are runtime config only.

## What shipped

A user can "Sign in with Google/GitHub": the app redirects to the provider,
the provider redirects back with a code, unidb exchanges it, links/creates a
unidb identity, and issues a normal unidb session:

1. **Authorize** — `GET /auth/oauth/<provider>/authorize` (public): mints a
   fresh CSRF `state` token + PKCE (RFC 7636) `code_verifier`, persists them
   server-side (hash-only for `state`, single-use, 10-minute TTL — same
   posture as the item-121/127 refresh-token session / MFA challenge
   stores), derives the PKCE `code_challenge` (`S256`), and redirects
   (`302`) to the provider's authorize URL with the standard
   `client_id`/`redirect_uri`/`scope`/`state`/`code_challenge`/
   `code_challenge_method=S256`/`response_type=code` query parameters.
2. **Callback** — `GET /auth/oauth/<provider>/callback?code=&state=`
   (public): validates + single-use-consumes `state`, exchanges `code` for
   a provider access token (server-to-server `POST`, with the matching PKCE
   verifier), fetches the provider's userinfo (id + email), resolves
   `(provider, provider_user_id)` to a unidb user (create-or-reuse), and
   issues a real unidb session via the **same `issue_token_pair` helper**
   every other login path (password, signup, MFA challenge) uses. Returns
   `{token, access_token, refresh_token, expires_in}` in the response body
   (a redirect-with-tokens-in-the-URL alternative was considered and
   rejected — a JSON body avoids leaking live credentials into browser
   history / proxy access logs / the `Referer` header of whatever the
   client navigates to next).
3. Google and GitHub are the two recognized provider names (with real-world
   default authorize/token/userinfo URLs + scope baked in), but the flow
   itself has nothing provider-specific beyond those three URLs and a scope
   string — `OAuthConfig::from_providers` accepts any provider name
   directly (used by the test suite to avoid needing `UNIDB_OAUTH_*` env
   vars at all).

## Config

Per provider (`<PROVIDER>` = `GOOGLE` or `GITHUB`):
- `UNIDB_OAUTH_<PROVIDER>_CLIENT_ID` / `_CLIENT_SECRET` / `_REDIRECT_URI`
  (all three **required**) — a provider missing any of these is simply
  absent from the running config: **both its routes return `404`**,
  indistinguishable from a non-existent route, same posture as
  `UNIDB_DEV_LOGIN`/`UNIDB_ALLOW_SIGNUP` being unset. The server works
  safely with **zero** providers configured (the default).
- `UNIDB_OAUTH_<PROVIDER>_AUTHORIZE_URL` / `_TOKEN_URL` / `_USERINFO_URL` /
  `_SCOPE` (optional) — override the real Google/GitHub endpoint defaults;
  this is exactly the seam the test suite uses to point a provider at a
  local mock with zero real network.
- OAuth login also needs a signing key configured (`UNIDB_JWT_SIGNING_KEY`
  or `UNIDB_DEV_LOGIN=1`) to actually hand back a session, same requirement
  as `POST /auth/signup`/`refresh`.

## Identity store

`(provider, provider_user_id) -> unidb username` persists in `roles.json`
(`AuthState::oauth_identities`, the same control-plane store as
credentials/sessions/MFA), `#[serde(default)]` so an existing `roles.json`
loads unchanged — no `FORMAT_VERSION` bump. No secret material lives there
(just ids and the resolved username), so — unlike
credentials/sessions/MFA — it is **not** redacted from `AuthState`'s manual
`Debug` impl (proven by test:
`auth_state_debug_redacts_oauth_state_but_not_identities` in
`src/authz/mod.rs`). No provider access token is ever persisted — it is
used in-flight to fetch userinfo, then discarded.

**Identity-linking rule — create, never auto-link by email (the "pick one,
document it" decision):** a returning identity always resolves via the
`(provider, provider_user_id)` map, never by matching a claimed `email`
against an existing local account. Auto-linking by email would let anyone
who controls *any* OAuth identity sharing an email string (a provider that
allows a self-set/unverified email, or a simply spoofed claim) silently take
over an existing password-protected unidb account — a real account-takeover
surface, not a hypothetical one. First login for a new identity creates a
fresh **non-superuser, no-password** account named
`oauth_<provider>_<provider_user_id>` (disambiguated with a short random
suffix in the vanishingly unlikely case that exact name is already taken by
an unrelated user). A future D5 (email flows) can offer an explicit,
user-initiated "link this OAuth identity to my existing account" action once
verified email exists; auto-linking on the claim alone never will.

`RoleStore::oauth_link_or_create` does the whole check-then-create sequence
under a **single lock acquisition**, closing the check-then-act race a
two-call version would have between two concurrent first-time logins for the
same brand-new identity (the item-122 TOCTOU lesson referenced in its own
doc comment).

## CSRF state + PKCE

- `state`: 256-bit CSPRNG, only its SHA-256 hash persisted (same "hash-only"
  posture as refresh tokens / MFA challenges). Single-use (marked `used` the
  moment it's redeemed), 10-minute TTL, and **pinned to the provider it was
  minted for** — a `state` issued for `google` is rejected against the
  `github` callback route, proven by test
  (`callback_rejects_state_issued_for_a_different_provider`).
- PKCE `code_verifier`: 256-bit CSPRNG, hex-encoded (64 chars — within RFC
  7636's charset and 43–128 length bound with no extra encoding step
  needed). Never sent to the client; persisted server-side alongside the
  state record and redeemed exactly once at token-exchange time. The
  derived `code_challenge` (`S256` = base64url-no-pad of the SHA-256
  digest) is the only PKCE value that ever reaches the browser/provider.
  Verified against RFC 7636 Appendix B's official worked example in
  `src/server/oauth.rs`'s unit tests.

## Secret handling (I3 seam)

The client secret is read from config/env behind a small accessor —
`OAuthProviderConfig::client_secret()` → `ClientSecret::expose()` in
`src/server/oauth.rs` — never logged, never `Debug`-printed (proven by test:
`provider_config_debug_never_leaks_the_client_secret`), never returned in
any HTTP response, and never present in the browser-visible authorize URL
(proven by test: `build_authorize_url_includes_pkce_and_state` asserts the
secret string is absent from the built URL). `ClientSecret::expose` carries
a `// I3: route through the vault when available` marker — the exact seam
for the next item (I3, secrets vault) to slot in vault-backed
decrypt-on-read without touching any call site in `oauth.rs` or
`handlers.rs`.

## HTTP client

`reqwest` (rustls-tls) is promoted from a dev-dependency to an optional,
`server`-feature-gated **runtime** dependency (`Cargo.toml`) — the existing
dev-dependency declaration is kept (used by `benches/decompose.rs`'s
Postgres-baseline HTTP calls, which don't build with `--features server`).
`OAuthConfig` holds one shared `reqwest::Client` (10 s timeout,
`build_http_client`) reused across every token-exchange/userinfo call.
Provider failures are split into two clean HTTP outcomes, never a hang or a
generic 500:
- **Provider unreachable / erroring / unparseable response** →
  `502 OAUTH_PROVIDER_UNAVAILABLE`.
- **Provider reached and explicitly rejected the request** (bad
  code/verifier/redirect_uri, 4xx) → `401 OAUTH_TOKEN_EXCHANGE_FAILED`.

## Testing without real providers

A tiny local axum "mock OAuth provider" (`tests/item128_oauth.rs`) stands in
for Google/GitHub: `GET /authorize` (no-op), `POST /token` (canned access
token, or a canned `400 invalid_grant` when a test toggles
`fail_token_exchange`), `GET /userinfo` (canned `{id, email}`, gated on the
`Authorization: Bearer <token>` header matching). Both the mock provider and
the unidb-under-test server bind to `127.0.0.1:0` — no outbound network
reaches anywhere real. 8 integration tests:
- `authorize_then_callback_issues_a_session_and_links_identity` (a)
- `second_login_with_same_identity_reuses_the_same_user` (b)
- `callback_rejects_unknown_and_replayed_state` +
  `callback_rejects_state_issued_for_a_different_provider` (c)
- `unconfigured_provider_404s` (d)
- `provider_token_exchange_failure_returns_clean_error_no_session` +
  `provider_unreachable_returns_502_no_session` +
  `provider_denied_consent_returns_400_no_session` (e, plus two bonus
  failure-path variants)

Plus 8 unit tests in `src/authz/mod.rs` (state round-trip, single-use,
wrong-provider rejection, link-or-create first/second login, distinct
providers are independent identities, `DROP USER` clears the identity link,
`Debug` redaction) and 3 in `src/server/oauth.rs` (PKCE `S256` against the
RFC 7636 worked example, authorize-URL construction never leaks the secret,
`Debug` redaction).

## Back-compat

Password/MFA login paths are unchanged — OAuth is purely additive (two new
public routes, one new `AppState` field defaulting to "no providers
configured"). Every existing item 100/121/122/127 auth test passes
unchanged (verified: `server_auth`, `item100_auth_endpoints`,
`item121_auth_core`, `item121_a5_a6_issuer_jwks`, `item122_auth_uid_jwt`,
`item122_b3_b4_roles_policies`, `item127_mfa`, `server_rate_limit`,
`item103_authz_bypass`).

## Lane / scope

`src/authz/mod.rs` (OAuth CSRF-state store + identity-link store — core, not
server-feature-gated, same posture as the item-121 credential store and
item-127 MFA state), `src/lib.rs` (`Engine` delegate methods),
`src/server/engine_handle.rs` (`EngineHandle` async wrappers),
`src/server/oauth.rs` (new — provider config, PKCE, HTTP client calls),
`src/server/dto.rs` (new `OAuthCallbackQuery` DTO), `src/server/handlers.rs`
(2 new handlers), `src/server/router.rs` (2 new public routes),
`src/server/mod.rs` (`AppState::oauth` field + `with_oauth` builder),
`src/bin/unidb-server.rs` (`UNIDB_OAUTH_*` env wiring), `Cargo.toml`
(`reqwest` promoted to an optional runtime dependency). No RLS/executor/
storage/migrations/MFA-core touched — control-plane + HTTP only, no WAL/
MVCC/storage-format change. Crash harness stays 54/54 (unaffected — nothing
here touches the page store).

No `PROGRESS.md` entry (task scope for this item explicitly excluded it,
mirroring items 124/125/126/127's precedent — "no `PROGRESS.md` entry, see
the file for verification detail").
