# Dev-inbox read route (GET /auth/dev-inbox)

**Type:** Improvement
**Status:** SHIPPED (2026-08-01, PR #248) — `GET/DELETE /auth/dev-inbox` reads/clears the LogTransport dev-inbox JSONL; double-gated (404 unless log transport active, checked before 403 non-superuser, so route existence isn't leaked). Unblocks the studio email-preview panel. item145 5/5, crash 54/54.

> Studio-flagged gap on item 138. The `LogTransport` (dev email transport) writes
> every "sent" email to a dev-inbox JSONL file (`UNIDB_EMAIL_DEV_FILE`, default
> `<data dir>/email-dev-inbox.jsonl`), but there is **no route to read it back**,
> so a studio/dev has to grep the server's filesystem to get a reset/magic-link
> token. This adds a read route so the studio can render a live email preview
> (Supabase's Inbucket/Mailpit equivalent). Control-plane only, dev-only — no
> storage-engine change, crash 54/54.

## Design (v1)

- **`GET /auth/dev-inbox?limit=`** — returns the dev-inbox entries (newest first,
  default limit e.g. 50): `[{to, subject, text_body, html_body?, ts}]`.
- **`DELETE /auth/dev-inbox`** — clears the dev inbox (truncate the file).
- **Availability gate — dev transport ONLY.** The route exists only when the
  active email transport is `LogTransport` (i.e. NOT real SMTP). When SMTP is
  configured (real delivery), the route returns **404** (there is no dev inbox,
  and we must never expose real recipients' mail). Mirror the `route_disabled`
  (404) posture used for dev-login/signup.
- **Superuser-only.** The dev inbox contains live password-reset / magic-link
  **tokens** — it is as sensitive as those links. Gate behind superuser (403 for
  anyone else), same as the other admin routes. Document loudly that this is a
  dev/testing aid, never for production (it's only reachable in log-transport
  mode anyway).

## Correctness / security
- Reads the `UNIDB_EMAIL_DEV_FILE` JSONL the `LogTransport` already writes — no
  new state, no storage change. Malformed lines are skipped, not fatal.
- 404 whenever SMTP (real) transport is active — the dev inbox must be
  unreachable in a real-delivery deployment. Superuser-gated otherwise.
- The route may return tokens/links (that's its purpose in dev) — so the
  superuser + dev-transport double gate is the safety boundary; documented.

## Acceptance
- With the log transport: `POST /auth/recover` for a user → `GET /auth/dev-inbox`
  (as superuser) returns an entry containing that email's reset link/token;
  redeeming that token via `/auth/verify` works. `DELETE /auth/dev-inbox` empties
  it.
- Non-superuser → 403. With SMTP transport configured → `GET/DELETE
  /auth/dev-inbox` → 404.
- New `tests/item145_dev_inbox.rs` (`#![cfg(feature = "server")]` first line),
  log transport, no network.
- Every pre-existing item138 test unchanged.
- **Crash 54/54**; `cargo test --no-run` (no features) + `clippy --all-features
  --all-targets -D warnings` + `fmt` clean.
- `docs/REST_API.md` (the route + dev-only/superuser gating), `README.md`, this
  Status flipped on merge.

## Non-goals
- A general mail-viewer UI (studio's job), search, or pagination beyond `limit`.
- Any behavior when real SMTP is active (deliberately 404).
