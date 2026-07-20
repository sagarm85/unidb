**Type:** Improvement
**Status:** ✅ SHIPPED 2026-07-20 — `feat/item-24-rls-hardening-login` (same PR as R-a/R-b).
See PROGRESS.md "Item 100".

# Item 100 — Dev login + whoami endpoints (username-only identity)

## Problem

There is no way for a user to **log in and obtain a token**. JWT is verify-only
(P6.e / Milestone 18): the server validates a `sub` claim but never issues one —
tokens are minted externally (`scripts/gen_jwt.sh`). `POST /auth/preview` runs
SQL *as* a named role but does not authenticate anyone or return a session.

The user's request: "different users log in with username, no password for now,
then RLS filters their data." Item 24 already delivers the RLS half (SELECT/
UPDATE/DELETE policies + `current_user`); the missing half is a login that puts
a username into the request identity so `current_user` resolves per user.

## Locked-decision note (must be respected)

Milestone 18 explicitly kept JWT **issuance OUT** ("verify-only, one locked
decision"). This item does NOT reopen that for production — it adds a
**dev/demo-only** issuer gated behind an env flag, so the production posture
("tokens come from your IdP") is unchanged. Record this against the M18 decision
in PROGRESS.md; do not enable by default.

## Scope

### `POST /auth/login`  (gated behind `UNIDB_DEV_LOGIN=1`)
- Body `{ "username": "alice" }`. Passwordless = **identification, not
  authentication** — acceptable for dev/demo only.
- Validate the user exists in the catalog (`unidb_catalog.users`); 404 if not.
- Issue a short-lived (e.g. 1 h) signed JWT with `sub=alice`, same key/verify
  path the server already trusts — so the existing `require_jwt` middleware and
  `current_user` substitution work with zero change downstream.
- When the flag is off: route returns 404/disabled (no accidental prod issuer).
- Password/OAuth later is purely additive (same JWT shape) — non-goal now.

### `GET /auth/whoami`  (always on)
- From the request's verified JWT, return
  `{ user, roles, is_superuser, privileges: [{table, ops}] }` — a direct read
  over the existing `has_privilege` / role catalog. This is the user's
  "way to check permission" and the natural client-side gate before a query.

## Why this keeps performance intact

Pure server/auth surface — no engine, executor, storage, or WAL path touched.
`login` is one catalog lookup + one JWT sign (~µs); `whoami` is catalog reads.
Zero effect on any Table 3/4 number.

## Acceptance

- [ ] `POST /auth/login` issues a token that the existing verify middleware
      accepts and that makes `current_user` resolve to the username end-to-end
      (login → SELECT on an RLS table → see only own rows).
- [ ] Flag off ⇒ endpoint disabled; a test asserts no token is issued.
- [ ] `GET /auth/whoami` returns correct roles + per-table privileges for a
      logged-in user and for the superuser.
- [ ] `docs/REST_API.md` documents both routes incl. the dev-only gate; M18
      locked-decision note recorded in PROGRESS.md.
