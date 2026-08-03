# Supabase parity — free / self-hostable roadmap

**Type:** Milestone
**Status:** IN PROGRESS (roadmap; individual items get their own `NN_` when started).
**2026-08-02 checkpoint:** every numbered item filed from this roadmap (138–146)
is SHIPPED & merged to `main` (PRs #241–#249; ledger: PROGRESS.md
"Supabase-parity BaaS layer — items 120–133 + free-parity continuation
135–146"). Still open below: the HELD engine-core compute cluster (stored
functions → RPC/triggers/upsert), GraphQL subscriptions (HELD), storage
TUS/image transforms (HELD), SAML (HELD), views/enums, identity linking,
anonymous sign-in, backups/PITR UX, other-language SDKs, studio panels.

> Authoritative roadmap for reaching **100% of the Supabase feature set that needs
> NO paid third-party subscription** — everything self-hostable or talking only to
> infra the operator already runs (their own S3/MinIO, their own webhook endpoint,
> their own SMTP). Supersedes the interim checklist in `134_supabase_parity_
> followups.md` (kept for history). Filed 2026-08-01 at the user's direction:
> "document the free list … copy all 100% features feasible … no SMS/phone."
>
> **The number is a stable ID; each feature below gets its own numbered `NN_`
> spec when work starts.** Metrics live in `PROGRESS.md`; status flips here.
> The at-a-glance status matrix vs Supabase's public feature list is
> [`SUPABASE_PARITY.md`](../../SUPABASE_PARITY.md) (repo root) — update it in
> the same PR as any status flip here (CLAUDE.md §9).

## Explicitly EXCLUDED — requires a paid / third-party service (do NOT build)
- **SMS / phone OTP · phone MFA · voice-call validation** — needs an SMS/voice
  gateway (Twilio/Vonage/SNS). No free path. *(User excluded.)*
- **AI embedding *generation*** — needs a hosted embeddings API (or a heavy local
  model). NOTE: vector **storage + search is already shipped and free** — only
  auto-*creating* vectors from text needs this.
- **CDN / smart-CDN delivery** — a CDN is third-party. NOTE: image
  **transformation** itself (below) is free; only edge delivery is paid.
- Architectural non-goals (locked): multi-project/orgs (I6), Edge Functions (I7),
  hosted control plane / management API, read replicas, connection pooling,
  custom domains — cloud-platform, not engine, features.

## Already shipped (parity or ahead)
Auth core (password/JWT/refresh/sessions, MFA-TOTP, OAuth, CAPTCHA, rate-limit,
vault) · RLS + roles + column grants · migrations · auto REST (+ FK embed +
embed filter/order) · GraphQL (read + mutations) · realtime (changes + broadcast
+ presence) · storage per-object authz + presign + buckets · full-text search ·
**vector search (native HNSW)** · **graph edges/Cypher** · **transactional event
queue** · unidb-js SDK v0.1. *(The last three are beyond Supabase.)*

---

## Wave 1 — highest ROI, unblock clusters

- **138 — Email transport + templates** (DONE — PR #241 merged 2026-08-01;
  email OTP / email confirmation / email change + a real `users.email` column
  remain fast follow-ups). Pluggable
  `EmailTransport` (SMTP via `lettre` + a dev/log transport), template system
  with `{{link}}`/`{{code}}`/`{{user}}`/`{{site_url}}` substitution.
  Provider-agnostic — self-host SMTP / free tier / dev-log; engine never
  forces a paid provider. **Unlocks 5 flows — 2 landed this PR:**
  password reset (`POST /auth/recover`/`/auth/verify`) and magic link
  (`POST /auth/magiclink`/`/auth/magiclink/verify`), both no-account-
  enumeration by design. Email OTP, email confirmation, and email change are
  the same machinery and remain fast follow-ups (also: no `users.email`
  column exists yet — `email` is currently looked up as a username directly,
  see `src/server/email.rs`'s module doc — a real column is part of that
  follow-up).
- **139 — `/rest/v1` count + `Prefer` response controls** (DONE — PR #242
  merged 2026-08-01): `Prefer: count=exact` ->
  `Content-Range` (RLS-scoped exact count, zero extra cost when unused),
  `Prefer: return=representation|minimal` on `POST`/`PATCH`/`DELETE`. REST-
  layer only, no SQL-engine change. **Upsert is explicitly NOT included** —
  `on_conflict`/`resolution=merge-duplicates` needs `INSERT … ON CONFLICT`,
  which the SQL engine doesn't support; filed as a separate future engine
  feature (see `docs/backlog/139_rest_count_prefer.md`'s note).
- **140 — Realtime channel authorization** (DONE — PR #243 merged
  2026-08-01): RLS-style per-topic allow/deny for
  broadcast/presence (the item-132 follow-up). Role-based, topic-glob
  `(topic_pattern, operation, roles)` policies in the control-plane store,
  enforced at connect/publish time on all four routes; audited
  `service_role`/superuser bypass; opt-in fail-closed via
  `UNIDB_REALTIME_REQUIRE_AUTHZ`; superuser-only `/realtime/policies` admin
  surface. Control-plane only, crash 54/54 — see
  `docs/backlog/140_realtime_channel_authorization.md`.
- **unidb-js SDK completion** (DONE 2026-08-01) — storage client module,
  GraphQL client (queries + mutations), broadcast/presence helpers, npm
  publish + CI workflows — all shipped to `sagarm85/unidb-js` (55/55 tests).
  *(Separate repo; actual npm publish needs the user's npm credentials.)*

## Wave 2 — the compute layer (one foundation unlocks four)

- **Stored functions / procedures** (DONE — item 147, PR #253 merged
  2026-08-03: SQL-body v1 control-plane functions + the RPC route in one
  item, no engine change; a plpgsql-analog later) → then:
  - **RPC** (`POST /rest/v1/rpc/<fn>`) — DONE, shipped in item 147 (PR #253)
  - **Triggers** (BEFORE/AFTER row) — IN PROGRESS, item 149
    (`149_row_triggers.md`; implemented after 150)
  - **Upsert** (`INSERT … ON CONFLICT`) — DONE, item 150 (PR #257 merged
    2026-08-03; includes the PostgREST `resolution=merge-duplicates` wiring
    139 excluded, and the latent HOT-chain MVCC fix)
  - **Auth hooks** (custom access-token / before-user-created / MFA hooks)
    — after 147 (consumes its functions)
- **Database webhooks** — outbound HTTP POST to the operator's endpoint on row
  change, built on the existing event stream (retries, signing secret from vault).
  **DONE** (item 141, PR #244 merged 2026-08-01): superuser `/webhooks` admin API (create/list/delete),
  background delivery worker over the existing durable-consumer event stream,
  `X-Unidb-Signature` HMAC, bounded exponential-backoff retry with per-delivery
  failure isolation — see `docs/backlog/141_database_webhooks.md`.
- **GraphQL subscriptions** (HELD — WebSocket-vs-SSE design call pending with
  the user) — over the realtime layer; inherit per-subscriber RLS.
- **Auth admin API** (DONE — item 142, PR #245 merged 2026-08-01): full
  user CRUD/list/ban/pagination shipped —
  `GET/POST/PATCH/DELETE /auth/admin/users*`, superuser-only, reusing the
  existing `CreateUser`/`DropUser`/`set_password`/`revoke_all_sessions_for_user`
  machinery; new per-user `banned` (enforced at login/refresh/email-verify
  with `403 USER_BANNED`, revokes sessions) and split
  `app_metadata`/`user_metadata`; last-superuser self-lockout guard on
  delete/demote. Control-plane only, crash 54/54 — see
  `docs/backlog/142_auth_admin_api.md`. **Still open:** identity linking
  (attach OAuth to an existing user by verified email); **anonymous
  sign-in** — both out of scope for 142, tracked here as follow-ups.
- **More OAuth providers + leaked-password protection** (DONE — item 143,
  PR #246 merged 2026-08-01): generalized item-128's preset table with five more built-in
  providers (Apple, Azure/Microsoft, GitLab, Discord, Facebook — all
  `sub`/`id`, no flow change; Apple's lack of a REST userinfo endpoint is a
  documented known gap, not silently claimed working), plus an opt-in
  (`UNIDB_PASSWORD_HIBP_CHECK`) HaveIBeenPwned Pwned-Passwords range-API
  leaked-password check (free, no key, k-anonymity — only a SHA-1 prefix
  ever leaves the server) enforced at signup / admin create-patch /
  password-reset, fail-open with a warning on an HIBP outage. Control-plane
  only, crash 54/54 — see
  `docs/backlog/143_auth_hardening_hibp_oauth_presets.md`.

## Wave 3 — breadth & polish

- **Scheduled jobs (cron-analog)** (DONE — item 144, PR #247 merged
  2026-08-01): Supabase/`pg_cron` parity — superuser
  `/cron/jobs` admin API (upsert/list/delete), a standard 5-field cron
  expression validated at registration (`400 INVALID_CRON_SCHEDULE`,
  hand-rolled parser, no heavy dep), a background scheduler that runs due
  jobs' SQL via the existing `execute_sql` path under an optional `run_as`
  principal (RLS/grants apply exactly as if called directly), no overlap
  (skips a tick if the previous run is still in flight, never stacks), no
  missed-tick backfill, per-job in-memory status +
  `unidb_cron_runs_total`/`unidb_cron_failures_total` metrics. Control-plane
  only, crash 54/54 — see `docs/backlog/144_scheduled_jobs.md`. **Note:**
  144's own doc labels itself a "Wave-2" item, but it is filed here under
  Wave 3 in this roadmap — flagged rather than silently reconciled either
  way; the classification doesn't change what shipped.
  User-defined & materialized views remain unstarted. **Enums/domains: DONE**
  (item 148, PR #254 merged 2026-08-03 — catalog-registered named types
  desugaring to the existing CHECK machinery; composite/custom types stay
  deferred with the row-encoding decision).
- **JWT signing-key rotation** (DONE — item 146, PR #249 merged 2026-08-01;
  the studio-flagged dev-inbox read route also shipped as item 145, PR #248):
  a `kid` header (one-way truncated hash of the
  signing key, never the key itself) on every issued token;
  `UNIDB_JWT_SIGNING_KEY_PREVIOUS` (HS256) accepted verify-only so rotating
  `UNIDB_JWT_SIGNING_KEY` doesn't mass-invalidate outstanding tokens — old
  tokens verify during the grace window, new tokens sign under the current
  key only; `UNIDB_JWT_PUBLIC_KEY_PREVIOUS` is the asymmetric analog,
  listing every configured public key in the JWKS document under its own
  `kid`. Control-plane only (`JwtConfig`), crash 54/54 — see
  `docs/backlog/146_jwt_key_rotation.md`.
- Storage: resumable **TUS uploads** · **image transformations** (local lib) ·
  full storage policy language · S3-compatible API surface. (HELD — awaiting
  user go-ahead.)
- **SAML / enterprise SSO** (no paid dependency our side; large). (HELD —
  awaiting user go-ahead.)
- Backups/PITR user-facing UX (engine has `restore_to_time`).
- Other-language SDKs (Python/Dart/Swift/Kotlin/Go) · studio advisors ·
  studio panels for the new surfaces (studio repo).

## Guardrails (every item)
Plan-time / control-plane only where possible; **crash harness stays 54/54**;
no ACID/perf regression; `clippy --all-features --all-targets` + `cargo test
--no-run` (no features) + fmt gates; new server tests carry `#![cfg(feature =
"server")]`. Each item ships as its own verified PR with docs updated (§9).
