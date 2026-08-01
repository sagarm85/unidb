# JWT signing-key rotation

**Type:** Improvement
**Status:** IN PROGRESS

> Wave-3 free-roadmap item (`137`). Rotating the JWT signing key today
> invalidates every outstanding access token at once (they no longer verify).
> Supabase supports key rotation with a grace window so live tokens keep working
> until they expire. This adds a **previous-key** grace window (HS256) + a `kid`
> header, so an operator can rotate the signing key without a mass logout.
> Control-plane / auth-layer only — no storage-engine change, crash 54/54.

## Current state (verified)
`JwtConfig` (item 121 A5/A6, `src/server/auth.rs`): **issuance is HS256-only**
(`UNIDB_JWT_SIGNING_KEY`); verification is HS256 (shared secret) OR asymmetric
RS256/ES256 verify-only (`UNIDB_JWT_PUBLIC_KEY` + `GET /.well-known/jwks.json`).

## Design (v1 — HS256 rotation grace window)
- **`kid` in the issued token header.** Tokens issued under the current signing
  key carry a `kid` (a short stable id derived from the key, e.g. a truncated
  hash — NOT the key itself). Purely informational for selecting the verify key;
  never leaks the secret.
- **Previous key accepted for verification only.** New env
  `UNIDB_JWT_SIGNING_KEY_PREVIOUS` (HS256): verification tries the **current**
  key first, then the **previous** key. Issuance ALWAYS uses the current key
  only. So the rotation procedure is: set `..._PREVIOUS` = old key, set
  `..._SIGNING_KEY` = new key, restart → tokens signed with the old key still
  verify (grace) until they expire; new tokens use the new key. After the max
  token TTL has elapsed, drop `..._PREVIOUS`.
- **Asymmetric side:** when verifying asymmetrically, allow **multiple public
  keys** and expose them all in the JWKS document (each with its `kid`), so a
  verifier picks by `kid`. `UNIDB_JWT_PUBLIC_KEY_PREVIOUS` (PEM) is the analogous
  grace key. (Asymmetric *issuance* stays out of scope — verify-only, as today.)

## Correctness / security
- The `kid` is derived from the key via a one-way hash (truncated); the secret is
  never exposed (assert in a test that no secret material appears in the header /
  JWKS — extend item-121 A6's "HS256 secret never leaks" test).
- Verification order current→previous; a token that matches neither → the same
  `401` as today (no downgrade, no weaker check). A malformed/missing `kid` still
  verifies by trying the keys in order (kid is a hint, not a trust boundary).
- Previous key is **verify-only** — never used to issue. No storage/WAL/MVCC
  change; this is all in `JwtConfig`.

## Acceptance
- Issue a token under key A; rotate (current=B, previous=A); the **A-signed token
  still verifies** (grace) and a **new token is B-signed** (its `kid` differs);
  a token signed with an unrelated key C fails `401`.
- Drop the previous key → the A-signed token no longer verifies (grace ended).
- Issued tokens carry a `kid`; the JWKS document (asymmetric mode) lists every
  configured public key with its `kid`; **no secret material** appears anywhere
  (test-asserted).
- New `tests/item146_jwt_rotation.rs` (`#![cfg(feature = "server")]` first line).
- Every pre-existing `item121_auth_core`/`item121_a5_a6_issuer_jwks` test
  unchanged.
- **Crash 54/54**; `cargo test --no-run` (no features) + `clippy --all-features
  --all-targets -D warnings` + `fmt` clean.
- `docs/REST_API.md` (rotation env vars + `kid` + the rotation procedure +
  JWKS multi-key), `README.md`, `137` Wave-3 line, this Status flipped on merge.

## Non-goals (v1)
- Automatic/scheduled rotation (operator-driven via env + restart is v1).
- Local asymmetric (RS256/ES256) issuance (still verify-only).
- More than one previous key (a single grace key covers one rotation at a time).
