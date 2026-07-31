# TOTP-based multi-factor authentication (MFA)

**Type:** Improvement
**Status:** SHIPPED (→ no `PROGRESS.md` entry — task scope excluded it, see below)

> Workstream D4 (item 120's roadmap: "D — External identity (OAuth, OTP,
> MFA, email)"). Self-contained RFC 6238 TOTP second factor for the auth
> core shipped in items 121/122 — no external provider, no secrets beyond
> what this server generates and stores itself.

## What shipped

A user can enroll a TOTP authenticator (Google Authenticator / 1Password /
Authy / …) and, once enrolled, login requires a valid 6-digit code as a
second factor:

1. **Enroll** — `POST /auth/mfa/enroll` (authenticated): generates a
   per-user random 160-bit TOTP secret (base32-encoded), stores it
   **pending**, returns `{secret, otpauth_url}`. MFA is not yet enabled.
2. **Confirm** — `POST /auth/mfa/verify` (authenticated) with a live
   6-digit `code`: verifies against the pending secret (±1 step / ±30 s
   clock-skew tolerance); on success flips MFA to **enabled** and returns
   8 one-time recovery codes (plaintext shown once; only SHA-256 hashes
   persisted).
3. **MFA-gated login** — `POST /auth/login` with a correct password for a
   user with MFA enabled does **not** issue a session: it returns
   `{mfa_required: true, challenge, expires_in}` (a 5-minute single-use
   opaque challenge token, hash-only persisted — same posture as a refresh
   token). `POST /auth/mfa/challenge` (public, rate-limited) redeems
   `{challenge, code}` — code may be a live TOTP code **or** a one-time
   recovery code — and, on success, mints the real
   `{access_token, refresh_token, expires_in}` session via the exact same
   `issue_token_pair` path every other login uses.
4. **Disable** — `POST /auth/mfa/disable` (authenticated): requires a
   current valid code, unless the caller is a superuser (bypass, no code
   needed — emergency recovery path).
5. `GET /auth/whoami` gained an `mfa_enabled: bool` field — never the
   secret or recovery codes.

## Crypto / correctness

- HOTP (RFC 4226) = HMAC-SHA1 over the big-endian counter, dynamic
  truncation, mod 10^6. Verified against the **official RFC 4226 Appendix D
  test vectors** in `src/authz/mod.rs`'s unit tests (not just
  self-consistency).
- TOTP (RFC 6238) = HOTP at `counter = unix_time / 30`. ±1 step tolerance
  either side of "now" (the Google-Authenticator-compatible default).
- Base32 (RFC 4648) encode/decode hand-rolled (~20 lines each way) rather
  than pulling in a crate — same "hand-roll the small thing" call the
  DER/JWK reader in `src/server/auth.rs` already makes.
- **Replay protection is a forward-only ratchet**: the most recently
  *successfully verified* TOTP step is remembered per user
  (`last_used_step`); any step at or before it is rejected outright. A
  direct, non-obvious consequence proven out during test-writing: because
  the acceptance window is only 3 steps wide (`current-1..=current+1`) and
  the ratchet only moves forward, **at most 3 fresh TOTP verifications are
  possible within one static 30 s window** — a 4th attempt in the same
  window always fails until real time advances the window. Recovery codes
  are a wholly separate credential space and are not subject to this
  budget — the integration test suite uses a recovery code where a test
  needed more than the TOTP budget allowed within one fast-running window,
  which is itself the intended, correct behavior (a MFA login flow in
  practice spans much more than a few hundred milliseconds, so this budget
  is never actually binding for a real user — it only became visible
  because HTTP round-trips inside a test complete in milliseconds).
- Constant-time comparison for both the 6-digit code and the recovery-code
  hash — no timing oracle.
- Crate choice: `hmac` + `sha1` (both already resolved in `Cargo.lock` as
  transitive deps of the existing `jsonwebtoken`/`ring` chain via
  `rfc6979` — no new supply-chain surface), per the task's "small,
  well-maintained crate" guidance, rather than `totp-rs` (which would have
  pulled in its own, independent base32/HMAC stack).

## Storage / redaction (proof of no leak)

- `AuthState` (control-plane `roles.json`, same file as credentials/
  sessions) gained `mfa: BTreeMap<String, MfaRecord>` (secret +
  recovery-code hashes + replay-ratchet state) and
  `mfa_challenges: BTreeMap<String, MfaChallengeRec>` (hash-keyed,
  single-use login challenges).
- `AuthState`'s existing manual `Debug` impl (which already redacted
  `credentials`/`sessions` to counts) was extended to redact `mfa`/
  `mfa_challenges` the same way.
- `MfaEnrollResponse`/`MfaVerifyResponse`/`MfaVerifyRequest`/
  `MfaDisableRequest`/`MfaChallengeRequest`/`AuthLoginOutcome` (the new DTOs
  in `src/server/dto.rs`) all carry manual `Debug` impls redacting the
  secret / codes / challenge, mirroring `AuthLoginResponse`'s existing
  posture for the access/refresh tokens.
- Proven by test, not just asserted: `mfa_secret_never_appears_in_whoami_or_roles_json`
  (`tests/item127_mfa.rs`) and `auth_state_debug_never_leaks_mfa_secret_or_recovery_codes`
  (`src/authz/mod.rs`'s unit tests) grep the actual whoami response body,
  the actual `Debug`-formatted `AuthState`, and the actual persisted
  `roles.json` for the secret/recovery-code plaintext.
- The base32 TOTP secret itself **is** persisted in the clear in
  `roles.json` — this is correct and unavoidable for TOTP (unlike a
  password, the server must be able to recompute the code to verify a
  login, so it cannot only store a one-way hash of the secret). Only the
  recovery codes get the hash-only treatment (a recovery code, unlike the
  TOTP secret, is never recomputed — only compared — so hashing it costs
  nothing and closes off a real leak surface).

## Back-compat

A user without MFA enabled logs in exactly as before — the MFA check is a
conditional branch inside `post_auth_login` that is a complete no-op for
such a user; every existing item 121/122 auth test
(`item121_auth_core.rs`, `item100_auth_endpoints.rs`,
`item121_a5_a6_issuer_jwks.rs`, `item122_auth_uid_jwt.rs`,
`item122_b3_b4_roles_policies.rs`, `server_auth.rs`, `server_authz.rs`,
`server_rate_limit.rs`, `item103_authz_bypass.rs`) passes unchanged.

## Lane / scope

`src/authz/mod.rs` (MFA state + TOTP/base32/HOTP + verify helpers,
core — not server-feature-gated, same posture as the A1 credential store),
`src/lib.rs` (`Engine` delegate methods), `src/server/engine_handle.rs`
(`EngineHandle` async wrappers), `src/server/dto.rs` (new DTOs),
`src/server/handlers.rs` (`post_auth_login`'s MFA branch + 4 new
handlers), `src/server/router.rs` (routes), `Cargo.toml` (`hmac`/`sha1`).
No RLS/executor/storage/migrations touched — control-plane only, no WAL/
MVCC/storage-format change. Crash harness stays 54/54 (unaffected —
nothing here touches the page store).

No `PROGRESS.md` entry — task scope for this item explicitly excluded it
(mirrors items 124/125/126's precedent of "no PROGRESS.md entry, see the
file for verification detail").
