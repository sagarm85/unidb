# MEMORY archive — 2026-08 roll-up

> Entries moved verbatim from MEMORY.md on 2026-08-03 (50 KB threshold; CLAUDE.md §0.4). Newest-first order preserved within each block. Grep, never read linearly.

## Current-status entries (rolled 2026-08-03)

- **2026-08-01 (session close) — items 132 (realtime broadcast/presence) + 133 (GraphQL mutations) SHIPPED; all remaining Supabase-parity work consolidated in backlog item 134 for a fresh session.**
  Two follow-ups to the 120–131 core shipped + merged this session: **133** (PR #235) — a GraphQL
  `Mutation` root (`insert_/update_/delete_<t>`) routed through the same enforced
  `execute_sql_params_as_principal` path as `/rest/v1`/`/sql` (RLS + `WITH CHECK` + column grants
  inherited, parity-tested, requested-projection-only); **132** (PR #236) — in-memory Broadcast +
  Presence over four JWT-gated SSE/POST routes, zero engine/WAL/heap/catalog touch. Both verified
  independently: **crash 54/54**, clippy `--all-targets`, plain `cargo test --no-run`, targeted +
  regression suites all green. **(Superseded pointer — item 134 is now SUPERSEDED by
  [`docs/backlog/137_supabase_parity_free_roadmap.md`](docs/backlog/137_supabase_parity_free_roadmap.md);
  see the top current-status entry for what shipped since and what remains.)** Original note read:
  everything still open toward Supabase parity is in `134_supabase_parity_followups.md`
  — (A) quick correctness follow-ups (named-superuser `WITH CHECK` INSERT quirk, GraphQL bulk
  insert, presence `track` orphan gap), (B) realtime channel-authz + GraphQL subscriptions,
  (C) email transport → magic-link/reset/confirm, more OAuth, storage transforms, DB webhooks,
  scheduled jobs, SDK breadth, (D) cross-repo: unidb-studio panels + unidb-js SDK. Out of scope:
  I6 per-project, I7 edge functions, distributed.

- **2026-08-01 — Supabase-parity BaaS layer COMPLETE (items 120–131) — 12 PRs merged (#222–#233); unidb-js SDK v0.1.0 shipped.**
  A full Backend-as-a-Service layer now sits on the engine, built entirely at plan-time /
  control-plane (no WAL/MVCC/heap/on-disk-format change) so **ACID + perf are intact by
  construction — crash harness 54/54 on every merge.** Shipped:
  - **Auth core (121, A):** argon2id password login/signup (`UNIDB_ALLOW_SIGNUP`), refresh-token
    sessions (256-bit opaque, only SHA-256 hash persisted, rotate-on-refresh, logout revoke),
    production JWT issuer (`UNIDB_JWT_SIGNING_KEY`), asymmetric verify RS256/ES256 +
    `GET /.well-known/jwks.json`. TOTP MFA (D4/127, RFC 6238/4226) + OAuth 2.0 Google/GitHub
    (D1/128, Auth-Code + PKCE).
  - **RLS ↔ token (122, B):** `auth.uid()`, `auth.jwt() ->> 'claim'` in policies (substituted at
    injection time, fail-closed); built-in `anon`/`authenticated`/`service_role` roles;
    `CREATE POLICY … FOR <op> TO <role>`; column-level grants (B5/112, error-not-mask, plan-time
    enforced, policy-column exemption).
  - **Auto API (123, C):** PostgREST-style `/rest/v1/<table>` CRUD+filters (C1, injection-safe
    `$n` binds through the existing RLS/grant path), embedded FK expansion (C2,
    `select=id,customer(name)`), `GET /rest/v1/` OpenAPI (C3), and a schema-derived GraphQL
    endpoint with FK + graph-edge + vector-`near` fields (C4/130, per-field grant correctness),
    now read+write via a `Mutation` root (`insert_/update_/delete_<t>`, item 133, same
    enforced-SQL-path/RETURNING-projection-grant correctness).
  - **Realtime (E1):** per-subscriber RLS-filtered SSE on `/events/subscribe` (service_role
    bypass audited, DELETE filters on the before-image, fail-closed).
  - **Storage (F1/125):** per-object authorization on `/storage/*` (public/private buckets,
    owner/superuser/service_role rules, presign gated, fail-closed).
  - **Ops/hardening:** auth rate-limiting (I1, `429 RATE_LIMITED` + `Retry-After`), encrypt-at-
    rest secrets vault (I3/129, AES-256-GCM, key from `UNIDB_MASTER_KEY`), CAPTCHA/Turnstile on
    auth endpoints (I2/131), forward-only SQL schema migrations + `unidb-migrate` bin with
    checksum drift detection (I4/126), studio-unblocker DDL (124: `ALTER USER … PASSWORD`,
    session list/revoke, policy `target_roles`, signup-binary fix).
  - **unidb-js SDK v0.1.0** (`sagarm85/unidb-js`, separate repo): supabase-js-shaped
    `createClient()` with auth / data (`/rest/v1`) / realtime (SSE); `tsc` strict, 30/30 unit
    tests; ships its own `CLAUDE.md`/`MEMORY.md` for a future session.
  Held per user decision: email/magic-link (D2/D5), SMS (D3). Parked: I5, I7, I6 (per-project
  control plane). Needs the user at deploy time: provider secrets (Google/GitHub/Turnstile/SMTP);
  drive the studio session (build G4 + wire live G1/G2); npm-publish the SDK.

- **2026-07-31 — Architecture-review session: items 117 + report-HTML renderer shipped; item 118 (async-HNSW crash hole) planned next.**
  Fresh senior-architect review of `report_20260728_102745.md` + the async-HNSW/CRUD/scan
  code surfaced a ranked findings list. Two units shipped to main:
  - **Item 117 (PR #218, `c134889`) — HOT UPDATE on PK/UNIQUE tables when the key is unchanged.**
    Root cause: `hot_eligible` gated on `!has_unique`, so the mere existence of a PK/UNIQUE
    index forced every PK'd-table UPDATE onto the slow per-row loop + a redundant per-row PK
    B-tree re-check. Fix mirrors item-53's FK gate: `has_unique_in_set` (HOT/enforce_unique/
    phantom-lock only when a unique/PK col is actually in SET). Safety verified — unchanged
    unique entry resolves via the same `get_visible` HOT-chain walk as secondary indexes.
    **Docker Table-5 cert `report_20260730_124355.md`: UPDATE bulk 0.19×→3.04×, unidb absolute
    138,840→1,130,955 rec/s (+8.1×)** (item-108 caveat: PG absolute drifted, so ~1.5× vs
    baseline PG — still a win, in the Table-3 HOT band). Correctness intact. `can_batch_non_hot`
    left conservative (`!has_unique`) as a follow-up.
  - **Report HTML renderer (PR #217, `71e74b1`).** `scripts/render_report.py` — content-agnostic
    Markdown→styled-HTML; `report.sh` auto-drops a `report_<ts>.html` sibling (git-ignored,
    derived; `.md` stays authoritative). Non-fatal.
  - **Next: item 118 — async-HNSW crash-durability hole (plan approved).** On the served/
    `open_arc` path a committed vector row whose HNSW insert is still in the in-memory
    `sync_channel(4096)` at crash time is lost from the index forever (WAL redo restores only
    what the worker durably applied; no reconciliation). Approved approach: **background,
    crash-gated reconciliation** — an `hnsw_dirty` clean-shutdown flag keeps clean reopens O(1);
    after an unclean shutdown a background thread diffs each index's `node_index` (via
    `validate()`) against the committed heap and re-enqueues only the missing tail through the
    worker. Add `DiskHnswIndex::contains(rid)` (insert is NOT idempotent) + a real crash-
    convergence test. Also folds in the worker's swallowed-insert-error leak.

  Other review findings still queued: report-honesty edits (label the O(1) COUNT(*) row; add
  `W_total`/replaced-stack columns — the moat is unmeasured in the baseline), scan-at-scale
  ceiling (~17M rec/s = per-row TEXT `String` alloc + serial result concat), UPDATE decode-
  pushdown (`cols/row`=8 on HOT UPDATE), and HNSW parallelism (single worker; 16.69 ms drain at
  100k = the Table-4 moat collapse to 0.01×).

- **2026-07-28 — Fresh FULL Docker bench on current main `7c064f1` → new authoritative baseline.**
  `docs/performance/report_20260728_102745.md` (all tables, 83m 38s, RSS 521 MiB, environment
  canary quiet — trustworthy). First full-report capture of items 115/116: **SELECT filtered
  0.58→0.77×** (the #210 one-shot warm-path fix landing in the full CRUD table exactly as its
  cert predicted). Other CRUD vs PG: INSERT per-row 0.57×, UPDATE HOT 1.19×, UPDATE non-HOT
  0.64×, GROUP BY 1.01×, COUNT(*) 49.6×, DELETE selected 1.91×, DELETE all 4.12×. Unified-commit
  moat intact: W4/W0 13.56× at 100k; Table 4 four-model atomic txn vs replaced stack's four
  round-trips. **NOT in this report:** item 106 Unit 3 NEAR win (gate 630→482 µs) — report.sh
  never measures NEAR (standing Linux NEAR-spot-check gap); certified only by native
  `perf_item106`. Promoted as authoritative in `docs/performance/README.md`; supersedes the
  07-23 `0324dc5` baseline.


## Session-log entries (rolled 2026-08-03)

### 2026-08-01 — item 146: JWT signing-key rotation grace window implemented

`JwtConfig` (`src/server/auth.rs`) now holds an ordered key list (current,
then optional previous) instead of a single key: issued tokens carry a `kid`
(one-way truncated SHA-256 of the key, never the key itself);
`UNIDB_JWT_SIGNING_KEY_PREVIOUS` (HS256) / `UNIDB_JWT_PUBLIC_KEY_PREVIOUS`
(asymmetric, both keys listed in JWKS) are accepted verify-only so rotating
the current key doesn't mass-invalidate outstanding tokens. All 7 gates
green (crash 54/54, new `item146_jwt_rotation` 8/8, `item121_a5_a6`/
`item121_auth_core` unchanged). Committed to branch, not merged.

### 2026-08-01 — item 142: Auth admin API (user management) implemented

See the Current-status entry above for the full design writeup. Summary: consolidated
superuser-only `/auth/admin/users*` REST surface (list+pagination/get/create/update/
delete), reusing existing `CreateUser`/`DropUser`/`set_password`/
`revoke_all_sessions_for_user` machinery end to end — no new SQL-input string building.
New per-user control-plane state (`banned` + `app_metadata`/`user_metadata`) lives in
`src/authz/mod.rs`'s `AuthState.user_extra` map, `#[serde(default)]`, no FORMAT_VERSION
bump. Ban enforcement lands at `POST /auth/login`/`/auth/refresh`/`/auth/verify`/
`/auth/magiclink/verify` (`403 USER_BANNED`), leaving `/auth/recover`/`/auth/magiclink`'s
no-enumeration `200` contract untouched. Went beyond the literal spec in one place, on
purpose: the last-superuser self-lockout guard lives inside `RoleStore::apply`'s shared
`DropUser` branch (not duplicated into the REST handler), so plain `DROP USER` SQL is
protected too, not just the new `DELETE` route; the same reasoning extended the guard to
`PATCH superuser:false` (demotion is the identical lockout risk as deletion) — an addition
beyond the backlog doc's literal ask, flagged here rather than silently shipped. All 7
gates green: build/clippy(`--all-features --all-targets -D warnings`)/fmt clean, plain
`cargo test --no-run` (no features) unaffected, new `tests/item142_auth_admin.rs` 12/12,
pre-existing `item121_auth_core` 16/16 unchanged, crash 54/54. `scripts/lint_backlog.sh`
and `scripts/lint_docs.sh` both clean. Docs updated: `docs/REST_API.md` (new section +
2 error-code table rows), `README.md` (bullet + curl example), `137`'s Wave-2 "Auth admin
API" line (marked IN PROGRESS, what shipped vs. still-open identity-linking/anonymous-
sign-in follow-ups). `142`'s own Status left IN PROGRESS per orchestrator convention.
Committed to the current branch, not merged/PR'd.

### 2026-08-01 — item 139: `/rest/v1` count + `Prefer` response controls implemented

Built on `src/server/rest_resource.rs` (item 123/C1's home). Added `parse_prefer` (case-
insensitive, comma-separated, repeatable-header `Prefer` parsing; unknown tokens ignored per
PostgREST posture) and `with_prefer_headers`/`build_content_range` for the response side. **GET**:
when `count=exact` is present, a second `SELECT COUNT(*) FROM <table> [WHERE …]` reuses the exact
same `filters`/`append_where` as the main query and runs through the same `run_stmt` (RLS/grants
apply); `Content-Range` is computed from `offset`/returned-row-count/total and attached via
`Response` (handler return type changed `Json<JsonValue>` -> `Response`, using
`axum::response::IntoResponse`; router.rs needed no change — axum's method-router combinators
don't require matching signatures across verbs). **POST/PATCH/DELETE**: `return=representation`
appends `RETURNING *` to the generated statement(s) (same technique `graphql.rs`'s
`insert_/update_/delete_<t>` mutations already use) and a new `merge_rows` helper (mirrors the
existing `merge_counts`) folds the `in.(...)`-expansion's per-statement `RETURNING` rows into one
result; `return=minimal` short-circuits to an empty `201`(POST)/`204`(PATCH,DELETE). No `Prefer`
header takes every pre-139 code path unchanged — verified by dedicated regression tests plus the
full untouched `server_rest` 30/30. New `tests/item139_rest_count_prefer.rs` (16 tests): exact
`Content-Range` math (incl. `offset`, zero-row `*/0`), **RLS parity** (alice sees 3/10 rows,
`/rest/v1` count matches a direct `SELECT COUNT(*)` as alice — not the unfiltered 10), unknown-
`Prefer`-token tolerance, `return=` on all three mutation verbs incl. the `in.(...)`-expansion
merge case. All 7 gates green: build/clippy/fmt clean, `cargo test --no-run` (no features) clean,
item139 16/16, server_rest 30/30 unchanged, crash 54/54. Docs: `docs/REST_API.md` new "Response
controls — `Prefer` header" section (documents the *pre-existing* no-`Prefer` default explicitly,
since it differs from real PostgREST's 201/Location default — a deliberate "don't silently
change it" call per the backlog spec), `README.md` bullet, `137`'s Wave-1 line split into 139
(done) + upsert (still separately out of scope — no `ON CONFLICT` in the SQL engine). Left `139`'s
Status as IN PROGRESS per orchestrator convention (flips to SHIPPED on merge).

### 2026-08-01 — item 135: `unidb-server-full` wiring fixes (memory storage + ConnectInfo), studio-reported

The studio session, doing live verification, reported two real binary-specific bugs in
`unidb-server-full` (the plain `unidb-server` was already correct). Verified both in source,
fixed both (item 135, PR #238): (1) `try_init_storage` unconditionally built `S3ObjectStore`,
so `STORAGE_BACKEND=memory` demanded S3 creds and never activated → now selects the store by
`cfg.backend` (`Memory` → `MemoryObjectStore`); (2) served with `axum::serve(listener, router)`
instead of `into_make_service_with_connect_info::<SocketAddr>()`, so the item-121 rate-limiter's
`ConnectInfo` extractor 500'd every `POST /auth/{login,signup,refresh}` → now wired like the
plain binary. Also fixed a pre-existing `clippy::manual_ignore_case_cmp` in the `UNIDB_DEV_LOGIN`
parse — **caught because the main-crate `clippy --all-features --all-targets` gate does NOT cover
the separate workspace-member binaries** (follow-up filed to add them to the gate). **Empirically
proven on the live binary** (no committed subprocess harness — not the repo's pattern): booted with
`STORAGE_BACKEND=memory` → log `"storage service ready","backend":"memory"`; `POST /auth/login`
returned 422 (Json body validation), not 500 (past the `ConnectInfo` extractor). Crash 54/54.
**Lesson:** workspace-member binaries (`unidb-server-full`, etc.) escape the main clippy gate —
run `clippy -p <bin>` on them; binary-only wiring (serve/connect-info, env-var reads) isn't
exercised by the library-level `TestServer` and needs a live/binary check.

### 2026-08-01 — item 133 (GraphQL mutations) implemented, committed to branch

Added a `Mutation` root to the C4 GraphQL schema (`src/server/graphql.rs`):
`insert_<t>(values: JSON!): <T>` / `update_<t>(<filter args>, set: JSON!):
[<T>!]` / `delete_<t>(<filter args>): [<T>!]` per eligible table (same
eligibility filter as the query side; a schema with zero eligible tables
stays query-only — an empty GraphQL `Object` type is invalid, so `Mutation`
is only registered when there's at least one field for it). Every resolver
builds one `INSERT`/`UPDATE`/`DELETE ... RETURNING <requested sub-fields>`
statement and runs it through the exact same `run_stmt`/`run_stmts` ->
`authorize_sql_as_principal` + `execute_sql_params_as_principal` path the
query side, `/rest/v1`, and `/sql` already share — no new write path, no
engine change. `RETURNING`'s column list turned out to already be
authorized exactly like a `SELECT` projection (`Engine::check_returning` in
`lib.rs`, pre-existing item-19 machinery) — the RETURNING-parity property
came for free, not something this item had to build. Widened three more
`rest_resource.rs` helpers to `pub(super)` for reuse: `build_assignments`,
`extract_single_in`/`InFilter`, `run_stmts`. New `tests/
item133_graphql_mutations.rs` (7 tests): insert/update/delete end-to-end
returning the requested projection, `WITH CHECK`/RLS rejection parity with
`/sql` (insert + update), and column-grant (RETURNING projection) denial
parity with `/sql`, including a same-statement `/sql` comparison in each
parity test. **Trap found while writing the WITH CHECK test:** a named
`SUPERUSER` principal (e.g. `root` over the HTTP server) is *not* exempted
from a table's own RLS `WITH CHECK` policy by the per-row INSERT check in
`sql/executor.rs::exec_insert` — that check only bypasses for the embedded
(`current_user: None`) caller or an explicit `service_role` claim, not for
`is_effective_superuser` generally (unlike the plan-level `apply_rls` skip,
which does cover named superusers). Not a bug introduced by this item and
out of scope to fix here — noted as a possible pre-existing RLS-superuser
inconsistency worth a future look; the test was adjusted to have the
policy's own owner insert the seed row instead of `root`. All 7 verification
gates green (crash 54/54, server_graphql/server_rest/server_authz all still
pass unchanged). Docs updated: `docs/REST_API.md` C4 section (mutations
documented in full), `README.md` GraphQL bullet, `docs/backlog/
130_graphql_api.md` deferred-note + non-goals pointer to 133. Committed to
the branch (not merged/PR'd — orchestrator flips `133_graphql_mutations.md`
Status to SHIPPED on merge).

### 2026-08-01 — Supabase-parity build drained: 12 PRs merged (#222–#233), autonomous overnight

User authorized autonomous overnight completion + auto-merge of verified PRs, with a
per-merge status report. Drove the entire approved queue to `main`, each PR self-verified
before merge (crash 54/54 + targeted suites + `cargo test --no-run` no-features +
`clippy --all-targets` + fmt): **#222** auth loop (121 A1–A4 + 122 B1–B4), **#223** (121
A5/A6 + I1 + 112/B5 + 123/C1), **#224** E1 realtime RLS, **#225** studio-unblocker (124),
**#226** F1 storage authz (125, salvaged from a stalled agent), **#227** C2 FK-embed (123),
**#228** I4 migrations (126) + a plain-`cargo test` fix, **#229** D4 MFA (127), **#230** D1
OAuth (128), **#231** I3 vault (129), **#232** C4 GraphQL (130), **#233** I2 CAPTCHA (131).
All work is plan-time / control-plane only → ACID + perf untouched by construction (the
crash harness stayed 54/54 through every merge). Separately authored + pushed **unidb-js**
SDK v0.1.0 to its own repo (TS/ESM; auth/data/realtime; 30/30 unit tests) with its own
`CLAUDE.md`/`MEMORY.md`. **Process lessons:** strictly sequential engine builds (usable disk
~38 GiB can't hold two concurrent ~30 GiB `--all-features` Rust builds — `cargo clean`
between every from-clean gate); every new server test needs `#![cfg(feature = "server")]`
or it breaks plain `cargo test`; check a delegated agent's output-file mtime and take over if
it is idle >30 min with no live process (the F1 salvage). Held: email/magic-link (D2/D5), SMS
(D3). Parked: I5/I7/I6. See the 2026-08-01 Current-status entry for the full feature map.

### 2026-07-31 — F1 storage per-object authz (item 125) — salvaged from a stalled agent, verified

The F1 implementing agent went **idle ~5h** (output frozen, no cargo/monitor process, no
completion notification — it hung on its background-test wait). It had left 18 files of complete,
uncommitted work. I took over: dropped its stray `disk_check.txt`, `cargo clean`, and verified the
tree myself — it compiled and passed. **Item 125 / F1** (`8840cdb`): per-object storage authz.
Step-0 caught that bucket public/private never actually shipped and NO caller identity reached
`/storage/*` (every bucket was public to any authed caller). Built: principal threaded into the
storage service; object metadata gains owner (serde-default) + bucket `is_public`; reads gated
(public open, private→owner, superuser/service_role bypass audited, list filtered, presign gated
on the read rule); writes/deletes owner-or-bypass; fail closed. Reuses existing auth machinery.
Verified: crash 54/54, storage_authz_f1 5/5, unidb-storage crate green, clippy/fmt clean. Richer
storage policy-DDL = documented follow-up. **Lesson:** an agent that "waits for a background test
monitor" can hang indefinitely and never notify — check agent output-file mtime vs now; if idle
>~30min with no process, take over and verify the tree directly rather than waiting.

