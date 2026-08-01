# Database webhooks (outbound HTTP on row change)

**Type:** Improvement
**Status:** SHIPPED (2026-08-01, PR #244) — outbound HTTP on INSERT/UPDATE/DELETE via a background delivery worker (durable `__webhooks__` consumer over poll_events; only active while ≥1 webhook registered, so vacuum_events horizon is unaffected); CDC envelope + hand-rolled RFC-4231-tested HMAC-SHA256 `X-Unidb-Signature`; bounded-retry-then-skip (dead endpoint can't wedge the stream); superuser admin routes, vault-first secret, redacted. Strictly downstream of commit — crash 54/54.

> Wave-2 free-roadmap item (`137`). Supabase "Database Webhooks" POST a change
> event to an operator-configured HTTP endpoint on INSERT/UPDATE/DELETE. unidb
> already has the hard part — a **WAL-derived, durable, ordered event stream**
> with consumer offsets (M4 / item 29 CDC envelope). This adds an outbound HTTP
> **delivery worker** on top. **No storage-engine change** — the worker is
> strictly *downstream* of commit (it consumes already-committed events via the
> existing `poll_events` durable-consumer path), so ACID is unaffected by
> construction and the crash harness stays 54/54.
>
> **Free/self-hostable:** the target is the operator's own HTTP endpoint. No paid
> third-party.

## Design (v1)

**Registration (control-plane, superuser).** A webhook =
`(id, target_url, table_pattern, events: [insert|update|delete], signing_secret?,
enabled, headers?)`. Stored in the existing control-plane store (`roles.json`-
style, `#[serde(default)]`, no FORMAT_VERSION bump). `table_pattern` = exact
table or `*`. Admin API (superuser-gated, mirroring `/realtime/policies`):
- `POST /webhooks` (create/upsert) · `GET /webhooks` (list, secrets redacted) ·
  `DELETE /webhooks/{id}`. Plus `Engine`/`EngineHandle` methods.
- Signing secret **vault-first** (`webhook.<id>.secret`) then the registration
  body — same posture as OAuth/SMTP/CAPTCHA secrets.

**Delivery worker (background, `server`/runtime-gated).** One worker owns a
**durable consumer** on `__events__` (the existing at-least-once mechanism):
- Poll new committed events; for each, match every enabled webhook by
  `table_pattern` + `events`; POST the **canonical CDC envelope** (item 29:
  `{table, op, before, after, xid, seq, ts}`) as JSON to `target_url`.
- **Signature:** `X-Unidb-Signature: sha256=<HMAC(secret, body)>` when a secret
  is set (reuse the `hmac`+`sha2` deps already vendored for MFA/tokens).
- **Retry:** bounded exponential backoff (e.g. 5 attempts) per delivery; on
  final failure, log + increment a `unidb_webhook_delivery_failures_total` metric
  and **advance past it** (do not wedge the stream — a dead endpoint must not
  block others). Successful/exhausted delivery advances the durable offset
  (at-least-once; document that a crash mid-flight can re-deliver).
- Timeouts + a per-attempt deadline; never block the engine/commit path.

## Correctness / security
- Strictly downstream of commit — reads via `poll_events` (committed events
  only), writes only its own consumer offset. No WAL/heap/MVCC change.
- Secrets redacted from `Debug`/audit/list; never logged. Signature lets the
  receiver verify authenticity.
- **SSRF note:** the target URL is operator-configured by a superuser (not
  end-user input), so SSRF is out of the trust model for v1 — documented; an
  allowlist is a follow-up if this ever accepts non-admin input.
- Delivery is best-effort at-least-once; ordering per-table follows the event
  stream. A slow/dead endpoint is isolated (bounded retries, then skip).

## Acceptance
- Register a webhook on `orders` for `[insert]`; INSERT a row → the configured
  endpoint receives one POST with the CDC envelope + a valid HMAC signature
  (test with a local mock HTTP receiver — NO real network).
- `update`/`delete` events deliver with correct before/after images; a webhook
  scoped to `[insert]` does NOT receive updates; a `*` pattern matches all tables.
- A failing endpoint is retried then skipped (metric increments) without blocking
  a second, healthy webhook.
- Secrets never appear in `GET /webhooks` or logs.
- Superuser-only admin routes (non-superuser → 403).
- New `tests/item141_webhooks.rs` (`#![cfg(feature = "server")]` first line),
  local mock receiver.
- **Crash 54/54**; `cargo test --no-run` (no features) + `clippy --all-features
  --all-targets -D warnings` + `fmt` clean.
- `docs/REST_API.md` (webhook admin routes + envelope + signature header + env),
  `README.md`, `137` Wave-2 line, this Status flipped on merge.

## Non-goals (v1)
- Exactly-once delivery (at-least-once + idempotency key is the contract).
- A UI (studio panel is a separate item), per-webhook transform/filter
  expressions beyond table+op, and SSRF allowlisting (documented follow-up).
