# Supabase parity — free / self-hostable roadmap

**Type:** Milestone
**Status:** IN PROGRESS (roadmap; individual items get their own `NN_` when started)

> Authoritative roadmap for reaching **100% of the Supabase feature set that needs
> NO paid third-party subscription** — everything self-hostable or talking only to
> infra the operator already runs (their own S3/MinIO, their own webhook endpoint,
> their own SMTP). Supersedes the interim checklist in `134_supabase_parity_
> followups.md` (kept for history). Filed 2026-08-01 at the user's direction:
> "document the free list … copy all 100% features feasible … no SMS/phone."
>
> **The number is a stable ID; each feature below gets its own numbered `NN_`
> spec when work starts.** Metrics live in `PROGRESS.md`; status flips here.

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

- **138 — Email transport + templates** (IN PROGRESS — transport + templates +
  password reset + magic link shipped; status flips to DONE on merge). Pluggable
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
- **REST upsert + count + `Prefer` headers** — `on_conflict`/`resolution=
  merge-duplicates`, `count=exact` + `Content-Range`, `return=representation/
  minimal`. PostgREST-parity clients expect these.
- **Realtime channel authorization** — RLS-style per-topic allow/deny for
  broadcast/presence (the item-132 follow-up).
- **unidb-js SDK completion** — storage client module, GraphQL client (queries +
  mutations), broadcast/presence helpers, npm publish + CI. *(Separate repo,
  parallelizable — no engine build contention.)*

## Wave 2 — the compute layer (one foundation unlocks four)

- **Stored functions / procedures** (SQL-body v1; a plpgsql-analog later) → then:
  - **RPC** (`POST /rest/v1/rpc/<fn>`)
  - **Triggers** (BEFORE/AFTER row)
  - **Auth hooks** (custom access-token / before-user-created / MFA hooks)
- **Database webhooks** — outbound HTTP POST to the operator's endpoint on row
  change, built on the existing event stream (retries, signing secret from vault).
- **GraphQL subscriptions** — over the realtime layer; inherit per-subscriber RLS.
- **Auth admin API** — full user CRUD/list/ban/pagination; **identity linking**
  (attach OAuth to an existing user by verified email); **anonymous sign-in**.
- **More OAuth providers** — generalize item-128's core (Apple, Azure, GitLab,
  Discord, …); registering apps is free.
- **Leaked-password protection** — HaveIBeenPwned Pwned-Passwords range API (free,
  no key, k-anonymity).

## Wave 3 — breadth & polish

- Scheduled jobs (cron-analog) · user-defined & materialized views ·
  enums/domains/custom types · JWT signing-key rotation.
- Storage: resumable **TUS uploads** · **image transformations** (local lib) ·
  full storage policy language · S3-compatible API surface.
- **SAML / enterprise SSO** (no paid dependency our side; large).
- Backups/PITR user-facing UX (engine has `restore_to_time`).
- Other-language SDKs (Python/Dart/Swift/Kotlin/Go) · studio advisors ·
  studio panels for the new surfaces (studio repo).

## Guardrails (every item)
Plan-time / control-plane only where possible; **crash harness stays 54/54**;
no ACID/perf regression; `clippy --all-features --all-targets` + `cargo test
--no-run` (no features) + fmt gates; new server tests carry `#![cfg(feature =
"server")]`. Each item ships as its own verified PR with docs updated (§9).
