# unidb vs Supabase — feature-parity tracker

> **Living document — keep it true.** One row per feature on Supabase's
> public feature list (supabase.com/features), sorted into four tables by
> unidb's status. **Update protocol:** whenever a parity-relevant item ships
> (or a HELD decision is made), move/edit the affected row(s) in the same PR,
> stamp the "Last verified" line below, and keep the summary counts honest.
> This file is part of the §9 pre-push staleness checklist (`CLAUDE.md`).
>
> Detailed plan/status per item: [`docs/backlog/137_supabase_parity_free_roadmap.md`](docs/backlog/137_supabase_parity_free_roadmap.md)
> (the authoritative roadmap; this file is the at-a-glance status matrix).
> Ledger entry: `PROGRESS.md` "Supabase-parity BaaS layer — items 120–133 +
> free-parity continuation 135–146".
>
> **Last verified:** 2026-08-03, on branch `feat/149-row-triggers`
> (item 149 shipping in this PR; 150 = upsert, 147 = PR #253, 148 = PR #254;
> previous stamps 2026-08-02/03). Feature list source: the
> supabase.com/features catalog as of early 2026 (page not re-fetchable from
> every environment — re-check it when updating).

**Summary:** ~40 catalog features → **22 done · 9 partial · 4 achieved
differently · 14 not done** (compute cluster: stored functions v1 + RPC =
item 147, enums + domains = item 148, upsert = item 150, row triggers =
item 149; auth hooks next; the remaining ❌ bulk is the deliberate
paid-third-party exclusions).

Legend: ✅ done · 🟡 partial · 🔁 done differently · ❌ not done
(HELD = awaiting explicit user/design go-ahead; Excluded = deliberate
non-goal, see the exclusions list in `docs/backlog/137_…`).

---

## Table 1 — ✅ Done (Supabase-equivalent or better)

| Supabase feature | How unidb implements it (scope) |
|---|---|
| Auto-generated REST API (PostgREST) | `/rest/v1/<table>` CRUD with filters, FK embedding + per-embed filter/order/limit, `Prefer: count=exact` → `Content-Range`, `return=representation\|minimal`, OpenAPI at `GET /rest/v1/` — all through the RLS/grant-enforced SQL path (items 123/136/139) |
| Row Level Security | Per-op `CREATE POLICY … FOR <op> TO <role>` with `auth.uid()` / `auth.jwt() ->> 'claim'` substitution, built-in `anon`/`authenticated`/`service_role` roles, plus column-level grants (items 122/112) |
| Magic link + password reset | `POST /auth/magiclink` / `/auth/recover` + verify routes; no account enumeration; single-use hash-only short-TTL tokens over a pluggable SMTP/dev-log transport (item 138) |
| Social login (OAuth) | Google, GitHub, Apple, Azure/Microsoft, GitLab, Discord, Facebook — Auth-Code + PKCE (items 128/143) |
| CAPTCHA / bot protection | Turnstile-style verification on auth endpoints (item 131) |
| Leaked-password protection | HIBP Pwned-Passwords k-anonymity check at every password-set point; opt-in `UNIDB_PASSWORD_HIBP_CHECK`, fail-open on outage (item 143) |
| Auth rate limiting | Per-IP on login/signup/refresh, `429 RATE_LIMITED` + `Retry-After` (item 121/I1) |
| JWT + JWKS + key rotation | HS256/RS256/ES256, `/.well-known/jwks.json`, `kid` headers, previous-key verify-only grace window — rotation without mass logout (items 121/146) |
| Admin user API | Superuser `/auth/admin/users` — paginated list/get/create/update/delete, per-user ban (`403 USER_BANNED`, sessions revoked), `app_metadata`/`user_metadata`, last-superuser lockout guards (item 142) |
| Database webhooks | Outbound POST of the CDC envelope on INSERT/UPDATE/DELETE; HMAC `X-Unidb-Signature`, durable consumer, bounded retry-then-skip (item 141) |
| Realtime: Broadcast | In-memory client↔client pub/sub over SSE, JWT-gated (item 132) |
| Realtime: Presence | Per-topic who's-online + per-client state (item 132) |
| Realtime authorization | Role-based topic-glob allow/deny policies, audited `service_role` bypass, opt-in fail-closed mode (item 140) |
| Cron / scheduled jobs (pg_cron) | `/cron/jobs` with 5-field cron expressions; SQL runs through the normal executor under a `run_as` principal so RLS/grants apply; no-overlap, no-backfill (item 144) |
| RPC (`/rest/v1/rpc/<fn>`) | `POST /rest/v1/rpc/{fn}` calling registered stored functions: named or positional JSON args → `$n` binds, all body statements in ONE atomic transaction, **invoker semantics by default** (caller's RLS/grants apply; explicit `run_as` definer-analog) (item 147). PostgREST's `GET` variant not offered (v1 non-goal) |
| Upsert (`INSERT … ON CONFLICT`) | `DO NOTHING` / `DO UPDATE SET … [WHERE …]` with `EXCLUDED.*` on a PK/UNIQUE conflict target; update arm routes through the existing UPDATE machinery (HOT/index/FK shared); RLS fail-closed both arms; REST `on_conflict=` + `Prefer: resolution=merge-duplicates\|ignore-duplicates` (item 150). Composite targets/`ON CONSTRAINT`/MERGE = non-goals |
| Database triggers | `CREATE TRIGGER … {BEFORE\|AFTER} {INSERT\|UPDATE\|DELETE} … EXECUTE FUNCTION` — fires an item-147 zero-param stored function per row, in the SAME transaction as the write (an `AFTER` trigger's audit row commits atomically, no outbox); name-order firing; errors veto; **no cascading** (a statement fired from inside a trigger body fires no triggers of its own — a deliberate v1 divergence from Postgres); trigger body always runs as the embedded/superuser identity (the `SECURITY DEFINER` problem, solved by always being on) (item 149). `FOR EACH STATEMENT`/`WHEN`/`INSTEAD OF`/`NEW`-modification-in-`BEFORE` = non-goals |
| Migrations | `unidb-migrate` CLI — forward-only SQL files, `schema_migrations` tracking table, checksum drift detection (item 126) |
| Secrets (Vault) | Encrypt-at-rest AES-256-GCM store keyed from `UNIDB_MASTER_KEY` (item 129) |
| Vector storage + search (pgvector) | Native on-disk HNSW, `NEAR` operator inside SQL `WHERE` — crash-recovered, MVCC/RLS-consistent; recall@10 0.90, ~482 µs vs pgvector's ~380 µs at 10k×dim128 (items 63/106) |
| Full-text search | Inverted index + `MATCH` operator |
| Observability | Prometheus `/metrics`, audit log, slow-query log, per-chokepoint latency histograms |

## Table 2 — 🟡 Partially done

| Supabase feature | What exists vs what's missing |
|---|---|
| Email/password login | Works (argon2id, signup gate, refresh-token sessions) — but no `users.email` column yet (email is looked up as username) and no email-confirmation / email-OTP / email-change flows (item-138 follow-ups) |
| MFA | TOTP (RFC 6238) done (item 127); phone/SMS MFA excluded by decision (paid gateway) |
| GraphQL API | Schema-derived reads + insert/update/delete mutations with per-field grant enforcement (items 130/133); subscriptions HELD on the WebSocket-vs-SSE design call |
| File storage | Buckets (public/private), per-object owner authz, presigned URLs, S3/MinIO/memory backends (item 125) — full Supabase-style storage *policy language* still open |
| Backups / PITR | Engine has online base backup + WAL archiving + restore by timestamp/LSN (PITR is a paid add-on at Supabase) — user-facing UX/tooling polish still open |
| Dashboard (Studio) | `unidb-studio` exists as a separate repo; panels for the newest surfaces (webhooks, cron, admin API, dev-inbox) pending |
| Client libraries | JS/TS done ([`unidb-js`](https://github.com/sagarm85/unidb-js), supabase-js-shaped) + Rust (`unidb-attach`); Python/Dart/Swift/Kotlin open |
| Database functions / stored procedures | SQL-body v1 done (item 147): superuser-registered control-plane functions (`/functions`), callable via RPC with atomic multi-statement execution — but no SQL-callable `SELECT fn()`, no plpgsql-analog. (Now also usable by triggers — item 149, see the Done table.) |
| Enums / domains / custom types | Enums + domains done (item 148): `CREATE TYPE … AS ENUM` / `CREATE DOMAIN … [CHECK (VALUE …)]` as catalog named types desugaring to the existing CHECK machinery (NULL-correct, drop-protected while referenced); v1 stores enums as TEXT (text-collation ordering, no `ALTER TYPE ADD VALUE`); composite/custom record types still deferred |

## Table 3 — 🔁 Achieved in a different way

| Supabase feature | unidb's approach and why it differs |
|---|---|
| Postgres database | Own Rust engine: ARIES WAL, MVCC (RC/RR/SSI), B-tree indexes, joins/aggregates/CTEs/window functions, cost-based optimizer. No Postgres wire protocol — access is embedded-library or REST, not `psql`/ORM connection strings. Trade: loses the Postgres ecosystem, gains single-file embedding + the four-model atomic commit |
| Realtime: database changes | Per-subscriber **RLS-filtered** change stream over SSE instead of Phoenix/WebSockets; DELETE events filtered on the before-image, fail-closed (E1) |
| Postgres extensions (pgvector, pg_cron, …) | No extension mechanism — the equivalents (vector index, cron, full-text, CDC) are native engine features, not plugins |
| Read replicas | Engine ships WAL-streaming replicas + `promote()` failover (Phase 6); the *managed-platform* replica provisioning is a locked non-goal |

## Table 4 — ❌ Not done

| Supabase feature | Status / reason |
|---|---|
| Auth hooks | Open — unblocked by item 147's functions; next phase (control-plane) |
| GraphQL subscriptions | HELD — WebSocket-vs-SSE decision pending |
| SAML / enterprise SSO | HELD — large item, no paid dependency, awaiting go-ahead |
| Resumable uploads (TUS) | HELD |
| Image transformations | HELD |
| S3-compatible storage API surface | Open, unstarted |
| Anonymous sign-in | Open, unstarted |
| Identity linking | Open, unstarted |
| Views / materialized views | Open, unstarted |
| Composite / custom record types | Deferred — a row-encoding format decision needing its own spec; enums + domains shipped separately (item 148, see Partial table) |
| Phone/SMS login + voice OTP | Excluded by decision — needs a paid SMS gateway |
| Automatic embeddings | Excluded — needs an embedding model/API; composable via webhooks + an operator-run worker |
| Edge Functions (Deno) | Locked non-goal (I7); partially substituted by cron + webhooks + future RPC |
| CDN / Smart CDN | Excluded — inherently third-party |
| Branching · connection pooling · orgs/projects · custom domains · SOC2 | Cloud-platform features, out of scope for a self-hosted/embedded engine |

---

## Beyond Supabase (unidb-only, no row on their list)

- **Graph model** — edge records + adjacency index + Cypher subset.
- **Transactional event queue** — WAL-derived durable stream, consumer
  offsets, replay, Debezium/Supabase-format CDC adapters.
- **Four-model atomic commit** — row + vector + graph edge + event in one
  fsync; the multi-system dual-write tax Supabase's stack cannot avoid.
- **Single-file embedded deployment** — the engine runs in-process with no
  server at all.

## Honest caveats (read before quoting this file)

- Parity here means **API-surface parity**, not battle-tested parity —
  Supabase inherits Postgres's decades of hardening; unidb's known gaps are
  tracked in `MEMORY.md` (Known issues) and `docs/backlog/backlog_index.md`.
- The BaaS/HTTP layer has had **no load benchmark** of its own, and the §6
  replaced-stack headline measurement (Table 4.1, `MM_REPLACED_STACK=1`) is
  still unmeasured — per `CLAUDE.md` §6, don't headline unproven numbers.
