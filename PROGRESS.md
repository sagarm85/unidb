# PROGRESS.md

> Milestone completion ledger. One entry per milestone, written when the
> milestone's PR is raised. Each entry records the benchmark **and memory**
> metrics for that milestone. Append newest at the bottom.
>
> Rules & decisions: `CLAUDE.md`. Current working state: `MEMORY.md`.
> Stamp every entry with the **actual current system date**.

---

## How to fill an entry

Copy the template, fill every field, link the PR. The metrics table is
**required** — a milestone is not "done" without recorded throughput + peak
memory (see `CLAUDE.md` §6).

### Entry template

```
## Mx — <name>   [status]   <date>

**PR:** #<n> — <link>
**Summary:** <2–3 sentences on what shipped>

**Benchmarks** (release build, <machine/spec>):

| Workload                     | Throughput (ops/s) | p50 (µs) | p99 (µs) | Peak RSS | Baseline (<what>) |
|------------------------------|--------------------|----------|----------|----------|-------------------|
| <e.g. single-table INSERT>   |                    |          |          |          |                   |
| <e.g. point SELECT by key>   |                    |          |          |          |                   |
| <e.g. UPDATE by key>         |                    |          |          |          |                   |

**Crash harness:** <points covered> — all green / notes
**What changed:** <bullets>
**Known limitations / tech debt:** <bullets>
**Deferred to later milestones:** <bullets>
**Locked-decision changes (if any):** <decision id + human sign-off, or "none">
```

---

## Milestones

## Entry index (all milestones & items, oldest → newest)

> Roll-up 2026-08-03: entries dated 2026-07-20 → 2026-07-24 were moved
> verbatim into the same archive (headings intact, greppable) when this
> file crossed the 120 KB threshold; the live tail starts at 2026-07-31.
> Full ledger entries dated before 2026-07-20 were rolled into
> [`docs/history/PROGRESS_ARCHIVE_2026-07.md`](docs/history/PROGRESS_ARCHIVE_2026-07.md)
> on 2026-07-22 — verbatim, headings intact, greppable. Nothing was deleted.

| entry | date | where |
|---|---|---|
| M0 — Storage core   [DONE]   2026-07-06 | 2026-07-06 | archive |
| M1 — MVCC + CRUD   [DONE]   2026-07-06 | 2026-07-06 | archive |
| M2 — Vector & Text search   [DONE]   2026-07-06 | 2026-07-06 | archive |
| M3 — Graph   [DONE]   2026-07-06 | 2026-07-06 | archive |
| M4 — Event queue   [DONE]   2026-07-06 | 2026-07-06 | archive |
| Bug fix (found during M5): xid reuse after checkpoint   2026-07-06 | 2026-07-06 | archive |
| M5 — API / server   [DONE]   2026-07-07 | 2026-07-07 | archive |
| M6 — B-Tree secondary index   [DONE]   2026-07-07 | 2026-07-07 | archive |
| M7 — CSR (Compressed Sparse Row) graph index   [DONE]   2026-07-07 | 2026-07-07 | archive |
| M8 — Attach client (Rust, blocking `reqwest`)   [DONE]   2026-07-07 | 2026-07-07 | archive |
| Performance: group commit + read-only fsync skip   [PROTOTYPE — branch `m9-group-commit`]   2026-07-08 | 2026-07-08 | archive |
| M11 — SQL Constraints   [SQL lane — landing]   2026-07-08 | 2026-07-08 | archive |
| Track D — Semantic search (cosine metric + embedding CLI) — 2026-07-08 | 2026-07-08 | archive |
| M10 — Heap vacuum / MVCC garbage collection   [DONE]   2026-07-08 | 2026-07-08 | archive |
| Phase 1 — ACID & storage foundation (Core lane, `acid-hardening`) | — | archive |
| Phase 1 complete | — | archive |
| P2.a — DECIMAL + TIMESTAMP   [SQL lane — Phase 2 — landing]   2026-07-08 | 2026-07-08 | archive |
| P2.b — FLOAT / UUID / BYTEA / DATE / TIME   [SQL lane — Phase 2 — landing]   2026-07-08 | 2026-07-08 | archive |
| P2.c — ALTER / DROP / TRUNCATE + transactional DDL   [SQL lane — Phase 2 — landing]   2026-07-08 | 2026-07-08 | archive |
| P2.d — sequences / SERIAL   [SQL lane — Phase 2 — landing]   2026-07-08 | 2026-07-08 | archive |
| P2.e — prepared statements + bind parameters   [SQL lane — Phase 2 — landing]   2026-07-08 | 2026-07-08 | archive |
| Phase 3 — Multi-model durable storage (Core lane, `durable-storage`) | — | archive |
| Phase 4 — Query power (SQL lane)   [DONE]   2026-07-09 | 2026-07-09 | archive |
| Phase 5 — Concurrency & performance   [COMPLETE]   2026-07-09 | 2026-07-09 | archive |
| Phase 6 — Operations & HA   [IN PROGRESS]   started 2026-07-09 | 2026-07-09 | archive |
| Commit-time WAL fsync — group-committed force-log-at-commit as default   [LANDING]   2026-07-09 | 2026-07-09 | archive |
| Postgres baseline comparison — standard design vs standard default   [DONE]   2026-07-09 | 2026-07-09 | archive |
| Autovacuum — auto-triggered background MVCC vacuum   [done]   2026-07-09 | 2026-07-09 | archive |
| Durable on-disk FSM + catalog page-list (branch `durable-fsm`, 2026-07-10) | 2026-07-10 | archive |
| Index & heap write concurrency (0a + 0c + Item A)   [SHIPPED]   2026-07-10 | 2026-07-10 | archive |
| Docker fair-fsync report + Table 3 remark & Table 3.1 bulk stress   [tooling]   2026-07-10 | 2026-07-10 | archive |
| CRUD performance — Phase A (write path)   [SHIPPED]   2026-07-10 | 2026-07-10 | archive |
| CRUD performance — Phase B (read path)   [SHIPPED]   2026-07-10 | 2026-07-10 | archive |
| Milestone P — parallel scan workers   [SHIPPED]   2026-07-10 | 2026-07-10 | archive |
| Milestone P follow-up — parallel filtered SELECT   [SHIPPED]   2026-07-11 | 2026-07-11 | archive |
| Parallel worker governance (item 15)   [SHIPPED]   2026-07-11 | 2026-07-11 | archive |
| REST API enrichment (item 12) — transaction sessions & full-surface coverage   [SHIPPED]   2026-07-11 | 2026-07-11 | archive |
| Cross-domain headline — unidb (1 atomic commit) vs the replaced stack (item 17)   [SHIPPED]   2026-07-11 | 2026-07-11 | archive |
| MVCC visibility anomaly under concurrent SQL writes (backlog item 16)   [DONE]   2026-07-12 | 2026-07-12 | archive |
| UNIDB_CONCURRENT_SQL_WRITES default-ON flip (backlog item 11 follow-up)   [SHIPPED]   2026-07-13 | 2026-07-13 | archive |
| Observability metrics enrichment (item 21)   [SHIPPED]   2026-07-13 | 2026-07-13 | archive |
| Engine access & introspection contract (Milestone 18)   [SHIPPED]   2026-07-13 | 2026-07-13 | archive |
| Logs surface — JSON structured logs, correlation ids, bounded /logs tail (backlog item 22)   [SHIPPED]   2026-07-13 | 2026-07-13 | archive |
| Events / realtime dispatcher (Milestone 20)   [SHIPPED]   2026-07-13 | 2026-07-13 | archive |
| Object storage service (item 23)   [SHIPPED]   2026-07-13 | 2026-07-13 | archive |
| Event queue at scale — seq index + push (item 26)   [SHIPPED]   2026-07-13 | 2026-07-13 | archive |
| Per-table vacuum accounting, cost throttle (backlog item 27) [SHIPPED] 2026-07-13 | 2026-07-13 | archive |
| Replication time-PITR + logical replication (item 28)   [SHIPPED]   2026-07-13 | 2026-07-13 | archive |
| Subscription CDC — canonical envelope, before/after, format adapters, lag observability (item 29)   [SHIPPED]   2026-07-13 | 2026-07-13 | archive |
| Multi-page catalog (item 25) — 2026-07-13 | 2026-07-13 | archive |
| Studio API readiness (item 30) — 2026-07-14 | 2026-07-14 | archive |
| Item 31 — Storage HTTP routes (2026-07-14) | 2026-07-14 | archive |
| Item 32 — Bulk Load HTTP API (2026-07-14) | 2026-07-14 | archive |
| Bulk load HTTP API (item 32)   [SHIPPED]   2026-07-14 | 2026-07-14 | archive |
| Item 33 — CDC Management API (2026-07-14) | 2026-07-14 | archive |
| Item 35 — Unique-index enforcement (2026-07-14) | 2026-07-14 | archive |
| Item 36 — FK row-level enforcement   [SHIPPED]   2026-07-14 | 2026-07-14 | archive |
| Default buffer-pool capacity raised 4096 -> 65536 frames (2026-07-14) | 2026-07-14 | archive |
| Item 40 — B-tree index sort-then-bulk-load backfill   [SHIPPED]   2026-07-15 | 2026-07-15 | archive |
| Item 41 — NEAR() vec_distance virtual column   [SHIPPED]   2026-07-14 | 2026-07-14 | archive |
| Item 42 — Bench harness buffer-pool fix (2026-07-15) | 2026-07-15 | archive |
| Item 39 — PK/FK relational-integrity stress bench, Table 5 (2026-07-15) | 2026-07-15 | archive |
| Item 43 — A3 gate: size-aware scan-vs-index selectivity   [PR open, needs perf validation]   2026-07-15 | 2026-07-15 | archive |
| Items 46 + 48 — GROUP BY decode pushdown + DELETE all O(1) fast path | — | archive |
| Items 47 + 44 — UPDATE B-tree in-place RowId patch + DELETE batched WAL mini-txn | — | archive |
| Items 47 + 44 — UPDATE B-tree in-place RowId patch + DELETE batched WAL mini-txn | — | archive |
| Item 49 — Bench harness Postgres connect-timeout fix (report.sh "indefinite hang") | — | archive |
| Item 50 — `DiskBTree::patch_many` infinite loop (critical, found verifying item 49) | — | archive |
| Bench hygiene — calibrated Docker baseline (2026-07-16) | 2026-07-16 | archive |
| Item 51 — SELECT JOIN: hash join + predicate pushdown   [PHASE A DONE — Phase B pending]   2026-07-16 | 2026-07-16 | archive |
| Item 52 — UPDATE/DELETE predicate-only decode pushdown (Phase B)   [STEP 1 DONE — Step 2 no-op]   2026-07-16 | 2026-07-16 | archive |
| Item 53 — FK UPDATE: skip child-side constraint re-check when FK column not in SET | — | archive |
| Item 54 — SELECT filtered: arena alloc for row data (item 45 Lever 3)   [SHIPPED]   2026-07-16 | 2026-07-16 | archive |
| Item 56 Step 1 — Parallel GROUP BY partial aggregation   [SHIPPED]   2026-07-16 | 2026-07-16 | archive |
| Item 56 Step 3 — WAL_XMAX_BATCH DELETE WAL framing   [SHIPPED]   2026-07-17 | 2026-07-17 | archive |
| Item 56 Step 4 — Logical B-tree index INSERT WAL   [SHIPPED]   2026-07-17 | 2026-07-17 | archive |
| D4 sign-off — HOT-equivalent UPDATE   [SIGNED OFF]   2026-07-17 | 2026-07-17 | archive |
| Item 58 — HOT-equivalent UPDATE   [SHIPPED]   2026-07-17 | 2026-07-17 | archive |
| Item 59 — SELECT filtered optimisations   [SHIPPED]   2026-07-17 | 2026-07-17 | archive |
| Item 60 — Event queue serde_json replacement   [SHIPPED]   2026-07-17 | 2026-07-17 | archive |
| Item 62 — IVF-Flat scale validation   [SHIPPED]   2026-07-17   PR #145 | 2026-07-17 | archive |
| Item 63 — On-disk HNSW vector index   [SHIPPED]   2026-07-17 | 2026-07-17 | archive |
| Item 63 — M2 Closing Docker Bench   [HONEST REGRESSION RECORDED]   2026-07-18 | 2026-07-18 | archive |
| Item 65 — HNSW incremental insert: per-insert NodeCache (2026-07-18) | 2026-07-18 | archive |
| Item 65 — Docker bench correction: NodeCache 100k regression + size gate (2026-07-18) | 2026-07-18 | archive |
| Item 66 — Parallel DELETE scan (2026-07-18) | 2026-07-18 | archive |
| Item 71 — Cross-page HOT chains (2026-07-18) | 2026-07-18 | archive |
| Item 74 — Batch mini-txn HOT UPDATE (2026-07-18) | 2026-07-18 | archive |
| Items 75–84 — DELETE + UPDATE perf sprint (2026-07-19) | 2026-07-19 | archive |
| Items 72 + 73 — HNSW Query Latency: L0 Cache + Vector Hot Cache | — | archive |
| Item 85 — Production-default concurrency hang fix (2026-07-19) | 2026-07-19 | archive |
| Item 24 Z1+Z3+Z5 — SQL authz DDL, JWT grant enforcement, catalog relations (2026-07-19) | 2026-07-19 | archive |
| Item 91 — M4 event-source architecture decision (2026-07-19) | 2026-07-19 | archive |
| Wave 1 CRUD — CRC boundary, fill-page cursor, WAL sealer, B-tree batch, lock elision (Items 86–90) (2026-07-19) | 2026-07-19 | archive |
| Item 92 — HNSW query next tier: zero-copy cache hits, SIMD distance, CREATE INDEX prefetch (2026-07-19) | 2026-07-19 | archive |
| Item 96 — Query plan cache (2026-07-19) | 2026-07-19 | archive |
| Item 97 — O(1) COUNT(*) via catalog row_count (2026-07-19) | 2026-07-19 | archive |
| Item 98 — Streaming-accumulation batch INSERT (2026-07-19) | 2026-07-19 | archive |
| Item 99 — POST /batch-sql: N statements in one HTTP round-trip (2026-07-19) | 2026-07-19 | archive |
| Item 24 R-a + R-b — UPDATE WITH CHECK enforcement + bootstrap observability (2026-07-20) | 2026-07-20 | archive |
| Item 100 — GET /auth/meta + POST /auth/login + GET /auth/whoami (2026-07-20) | 2026-07-20 | archive |
| Item 101 — Group-commit dwell window in WAL (2026-07-20) | 2026-07-20 | archive |
| Item 102-A — Index-only scan: key-col projection (2026-07-20) | 2026-07-20 | archive |
| Item 94 — NEAR lightweight snapshot for standalone queries (2026-07-20) | 2026-07-20 | archive |
| Item 102-B — Covering index: INCLUDE columns in B-tree leaf (2026-07-20) | 2026-07-20 | archive |
| Items 67 / 51 / 68 / 69 — Async HNSW, Hash join, Hint bits, Fill-factor (2026-07-20) | 2026-07-20 | archive |
| Item 95 — Graph adjacency cache: hot-hub lazy warm cache (2026-07-20) | 2026-07-20 | archive |
| Item 103 — AuthZ v2: superuser RLS bypass (2026-07-20) | 2026-07-20 | archive |
| Item 93 — HNSW L0 arena layout: zero-copy beam search (2026-07-20) | 2026-07-20 | archive |
| Item 19 (partial) — SQL surface gaps: G1 + G3 + routing fixes (2026-07-20) | 2026-07-20 | archive |
| Item 19 G2-cast — CAST expressions and explicit type conversion (2026-07-20) | 2026-07-20 | archive |
| Item 19 G7 — Window functions (whole-partition frame) (2026-07-20) | 2026-07-20 | archive |
| Item 19 G2-join — FULL OUTER JOIN (2026-07-20) | 2026-07-20 | archive |
| Item 19 G-NATURAL — NATURAL JOIN (2026-07-20) | 2026-07-20 | archive |
| Item 104 — Catalog sync dedup: remove double-fsync per INSERT (2026-07-20) | 2026-07-20 | archive |
| Item 70 — Sequential scan read-ahead (madvise WILLNEED)   [SHIPPED]   2026-07-20 | 2026-07-20 | archive |
| Item 38 — Parameter type coercion   [SHIPPED]   2026-07-20 | 2026-07-20 | archive |
| Item 19 — IN(subquery) / EXISTS / scalar subquery predicates (2026-07-20) | 2026-07-20 | archive |
| Item 105 — Selective bench runs + baseline carry-forward   [SHIPPED]   2026-07-21 | 2026-07-21 | archive |
| Item 92 — Vector query Levers 5+7 (Arc snapshots + vector slab)   [SHIPPED]   2026-07-21 | 2026-07-21 | archive |
| Consolidated Docker bench — validation-debt run   [RECORDED]   2026-07-21 | 2026-07-21 | archive |
| Item 108 — CRUD ratio drift: RESOLVED as environment, no unidb regression   [SHIPPED]   2026-07-21 | 2026-07-21 | archive |
| Item 107 — Async HNSW on the commit path: wiring + freshness gauge   [SHIPPED]   2026-07-22 | 2026-07-22 | archive |
| Item 109 — Page-cached B-tree candidate resolution   [SHIPPED]   2026-07-22 | 2026-07-22 | archive |
| Item 110 — RLS + LIMIT crash: current_user destroyed in QuerySpec path   [SHIPPED]   2026-07-22 | 2026-07-22 | archive |
| Item 111 — information_schema visibility follows table grants   [SHIPPED]   2026-07-22 | 2026-07-22 | archive |
| Fresh full Docker bench — new MM_BASELINE (post-107, main `0324dc5`)   [RECORDED]   2026-07-23 | 2026-07-23 | archive |
| Bench: PG parallelism sensitivity + session isolation   [SHIPPED]   2026-07-23 | 2026-07-23 | archive |
| Items 115 + 116 — behind-metrics attribution + first levers   [IN PROGRESS]   2026-07-24 | 2026-07-24 | archive |
| Report HTML renderer (auto `.html` sibling)   [SHIPPED]   2026-07-31 | 2026-07-31 | live |
| Item 117 — HOT UPDATE on PK/UNIQUE tables when key unchanged   [SHIPPED]   2026-07-31 | 2026-07-31 | live |
| Supabase-parity BaaS layer — items 120–133 + 135–146 (PRs #222–#249)   [SHIPPED]   2026-08-01 | 2026-08-01 | live |
| Item 147 — Stored SQL functions v1 + RPC   [SHIPPED — merged PR #253]   2026-08-03 | 2026-08-03 | live |
| Item 148 — Enums + domains (named types v1)   [SHIPPED]   2026-08-03 | 2026-08-03 | live |
| Item 150 — Upsert ON CONFLICT + REST resolution (+ HOT-chain MVCC fix)   [SHIPPED]   2026-08-03 | 2026-08-03 | live |

## Report HTML renderer — auto `.html` sibling for every report   [SHIPPED]   2026-07-31

**PR:** #217 — https://github.com/sagarm85/unidb/pull/217 (merged, `71e74b1`)
**Summary:** `scripts/report.sh` now auto-renders a styled, self-contained
`report_<ts>.html` next to every generated `.md`, via a new content-agnostic
`scripts/render_report.py` (pure stdlib). The Markdown stays the source of
truth (git-tracked, consumed by `compare_bench.py`); the HTML is derived +
git-ignored + regenerable. Non-fatal (a render failure only warns).

**What changed:** `render_report.py` (GFM subset → styled HTML; emphasis is
pattern-matched on cell *content* — ratios ending in `×`, unidb/postgres
winner remarks, PASS/FAIL — never on fixed column indices, so relabelled/new
columns render with no code change); `report.sh` wire-in after the report is
finalized; `.gitignore` ignores `docs/performance/*.html`; docs updated.
**Verification:** full report (11 tables, 17 winner pills, escaping) + a
conc_matrix (32 PASS pills) render clean; `bash -n` OK; doc linters green.
**Known limitations:** none functional; presentation layer only.

## Item 117 — HOT UPDATE on PK/UNIQUE tables when the key is unchanged   [SHIPPED]   2026-07-31

**PR:** #218 — https://github.com/sagarm85/unidb/pull/218 (merged, `c134889`)
**Summary:** `hot_eligible` was gated on `!has_unique`, so the mere existence
of a PK/UNIQUE index disabled the HOT fast path — forcing the common "update a
non-indexed column on a PK'd table" case onto the slow per-row loop plus a
redundant O(log n) PK B-tree re-check per row (the key never changes). This was
the root cause of the report's worst CRUD row, Table 5
`UPDATE orders SET status=… WHERE id<N` at **0.19×** vs Postgres. Fix mirrors
item-53's FK gate: `has_unique_in_set` (HOT / `enforce_unique` / phantom lock
only when a unique/PK column is actually in the SET clause);
`set_touches_indexed_col` (already checks `unique_index_root`) remains the
backstop for the PK-in-SET case. `can_batch_non_hot` left conservative on
`!has_unique` as a follow-up.

**Safety (verified):** unique/PK index lookups follow HOT chains via the
identical `DiskBTree::search_eq` → per-candidate `get_visible` path as
secondary indexes, so the unchanged unique entry still resolves to the live
HOT-updated version (SELECT-by-PK, duplicate detection, secondary lookups all
correct). Same pattern already ran for secondary btrees on non-indexed-column
updates.

**Benchmarks** (Docker Table-5 cert `report_20260730_124355.md`, aarch64 · 18
cores · Linux 6.12 linuxkit):

| Workload                          | unidb (rec/s) | postgres (rec/s) | ÷ PG   | Baseline (07-28) |
|-----------------------------------|--------------:|-----------------:|-------:|------------------|
| UPDATE bulk (SET status, PK'd)    | **1,130,955** | 372,123          | 3.04×  | 0.19× (138,840 rec/s) |
| INSERT valid FK (per-row)         | 2,180         | 6,427            | 0.34×  | untouched by 117; VM-noisy |
| SELECT JOIN                       | 1,170,903     | 2,125,436        | 0.55×  | untouched by 117 |

**Trustworthy figure (item-108 hygiene):** unidb's **absolute** UPDATE
throughput **138,840 → 1,130,955 rec/s (+8.1×)**. PG's absolute drifted down
this run (canary), so the 3.04× ratio overstates; vs the baseline PG number
unidb is ~1.5×, in the PK-less Table-3 UPDATE-HOT band (1.19×) — exactly as
predicted since both now take the same HOT path.

**Crash harness:** 54/54 green (incl. p74 batch HOT, cross-page HOT, p58b
index). Full suite 220 passed / 0 failed; clippy + fmt + doc linters clean.
**Correctness:** bad-FK INSERT rejected ✓; referenced-parent DELETE blocked ✓.
**Known limitations / tech debt:** non-HOT *batch* path (`can_batch_non_hot`)
still gated on `!has_unique` — relaxing it needs `Heap::update_many` to
re-point the unique B-tree (separate follow-up).
**Locked-decision changes:** none.

## Supabase-parity BaaS layer — items 120–133 + free-parity continuation 135–146   [SHIPPED]   2026-08-01 (entry backfilled 2026-08-02)

**PRs:** #222–#233 (core items 120–131), #235/#236 (items 133/132), #238–#249
(items 135, 136, 138–146), plus docs-only PRs #234/#237/#240/#250 — **all
merged to `main`** (verified against `origin/main` `a3cd132` on 2026-08-02).
This entry is a backfill: the merges landed 2026-08-01 across two sessions,
but the per-milestone PROGRESS habit was missed — the per-item detail went
only to `MEMORY.md` and the `docs/backlog/NN_*.md` SHIPPED headers. Caught by
the 2026-08-02 docs-staleness sweep. **Process note:** flipping a backlog
file's Status on merge is not a substitute for the ledger entry here.

**Summary:** a Supabase-class Backend-as-a-Service layer over the engine,
built entirely at plan-time / control-plane — no WAL/MVCC/heap/on-disk-format
change, so ACID and engine performance are intact **by construction**. Shipped
across the track: auth core (argon2id login/signup, refresh-token sessions,
JWT + JWKS + key rotation w/ verify-only grace window, TOTP MFA, OAuth
Google/GitHub + Apple/Azure/GitLab/Discord/Facebook presets, magic link +
password reset over pluggable SMTP/dev-log email transport, HIBP
leaked-password check, CAPTCHA, rate limiting, admin user API w/ ban +
app/user metadata, secrets vault) · RLS↔token binding (`auth.uid()`/
`auth.jwt()`, anon/authenticated/service_role, column grants) · auto APIs
(PostgREST-style `/rest/v1` with filters/FK-embed/embed-filter-order/
count=exact/`Prefer`, OpenAPI, GraphQL read + mutations) · realtime
(RLS-filtered SSE changes, broadcast + presence, channel-authz policies) ·
storage per-object authz (buckets, owner rules, presign) · database webhooks
(HMAC-signed, durable consumer, retry-then-skip) · scheduled cron jobs
(`run_as` RLS parity) · forward-only SQL migrations (`unidb-migrate`) ·
dev-inbox route · `unidb-server-full` wiring fixes · **unidb-js SDK**
(separate repo `sagarm85/unidb-js`: auth/data/realtime/storage/GraphQL,
55/55 tests, npm CI/publish workflows).

**Benchmarks:** none recorded for this track, deliberately — every item is
control-plane-only (guardrail: no ACID/perf regression, storage engine
untouched), verified per-PR by **crash harness 54/54** + clippy
`--all-features --all-targets -D warnings` + plain `cargo test --no-run`
(no-features) + fmt + targeted/regression suites. Honest §6 caveats: the
BaaS/HTTP layer itself has had **no load benchmark** (auth throughput, SSE
fan-out, webhook delivery under churn are unmeasured), and the §6
replaced-stack headline column (Table 4.1, `MM_REPLACED_STACK=1`) remains
**unmeasured** — both tracked as follow-ups.

**Known limitations / held work:** engine-core compute cluster HELD for
explicit user/design sign-off (stored functions → RPC / triggers / upsert
`INSERT … ON CONFLICT` — ACID write-path) · GraphQL subscriptions (HELD,
WebSocket-vs-SSE call) · storage TUS uploads + image transforms (HELD) ·
SAML (HELD) · identity linking, anonymous sign-in, views/materialized views,
enums/custom types (unstarted) · no `users.email` column yet (email is
looked up as username — item-138 note) · named-superuser `WITH CHECK` INSERT
quirk (item-133 finding, possible pre-existing RLS-superuser inconsistency).
**Locked-decision changes:** none.

## Item 147 — Stored SQL functions v1 + RPC   [SHIPPED — merged via PR #253]   2026-08-03

**PR:** #253 — https://github.com/sagarm85/unidb/pull/253 (merged, `4355ba9`) | **Type:** Improvement
**Summary:** compute-cluster phase 1 (user go-ahead 2026-08-02 lifted the
HELD). Control-plane `FunctionDef` in `AuthState` (serde-default, no
FORMAT_VERSION bump; cron/webhook persistence pattern) with registration-time
validation; superuser `/functions` admin API (upsert/list/delete); `POST
/rest/v1/rpc/{fn}` — named or positional JSON args → `$n` binds through the
item-38 coercion layer, every body statement executed in **one transaction**
(abort-on-error, atomic), response = last statement's rows in `/sql`'s shape.
**Security:** invoker by default — `run_as: None` means the *calling*
principal (their RLS/grants apply), a deliberate documented divergence from
cron's None-means-admin, because RPC is callable by any authenticated
principal; `run_as: Some(role)` is the explicit definer-analog (registration
is superuser-only, so definer grants are admin-controlled). Implemented by a
Sonnet subagent against the locked spec (`docs/backlog/147_…`); orchestrator
re-ran every gate independently.

**Benchmarks:** none — control-plane only, zero engine/WAL/heap/catalog
change (same §6 posture as the items 120–146 entry above); the standing
BaaS-layer load-bench debt covers this surface too.
**Verification (orchestrator-rerun):** build `--features server` · clippy
`--all-features --all-targets -D warnings` · fmt · plain `cargo test
--no-run` (test file feature-gated) · item147 **8/8** · item144 regression
**9/9** · **crash harness 54/54**.
**Known limitations:** no SQL-callable `SELECT fn()` (plpgsql-analog later);
no `GET /rest/v1/rpc`; no declared param types; not usable by triggers yet.
Pre-existing behavior surfaced, not changed: a **named** superuser doesn't
bypass per-row INSERT `WITH CHECK` (item-24 Z1 bypasses only the embedded
`None` identity and `service_role`) — identical via `/sql` and RPC,
documented in the test (first flagged in the item-133 session).
**Locked-decision changes:** none.

## Item 148 — Enums + domains (named types v1)   [SHIPPED — on `feat/148-enums-domains`, PR raised on push]   2026-08-03

**Branch:** `feat/148-enums-domains` | **Type:** Improvement
**Summary:** `CREATE TYPE <name> AS ENUM (…)` and `CREATE DOMAIN <name> AS
<base> [CHECK (VALUE …)]` as catalog-registered named types
(`docs/backlog/148_enums_domains.md`). Design: **desugar at `CREATE TABLE`/
`ALTER TABLE ADD COLUMN` time** into base type + a synthesized CHECK through
the existing constraint machinery — zero new enforcement code on the write
path; catalog blob gains a serde-default `types` map and `ColumnDef` a
serde-default `type_name` (no FORMAT_VERSION bump, no on-disk tuple change).
Enum CHECK = OR-chain of equalities wrapped `col IS NULL OR (…)` (the
executor's two-valued compare would otherwise reject NULL — caught by the
required NULL test); domain CHECK = whole-word `VALUE`→column substitution
re-parsed through sqlparser; `DROP TYPE/DOMAIN` rejected while referenced
with a deterministic table.column error. sqlparser 0.62.0 AST route
(CreateType/CreateDomain/DropDomain are dialect-unconditional — no pre-parse
hack). Implemented by a Sonnet subagent; orchestrator re-verified all gates.

**Benchmarks:** none — plan-time desugar over existing CHECK enforcement;
no new per-row work beyond what an equivalent hand-written CHECK already
costs (§6 posture as items 120–147).
**Verification (orchestrator-rerun, incl. post-merge with item 147):**
clippy `--all-features --all-targets -D warnings` · fmt · plain `cargo test
--no-run` · item148 **16/16** · item147 **8/8** (post-merge) · constraints
**30/30** · item24_rls_with_check **8/8** · **crash harness 54/54**.
**Known limitations (documented v1 non-goals):** enums stored as TEXT
(text-collation ordering, not declaration order); no `ALTER TYPE … ADD
VALUE`; no composite/custom record types (row-encoding decision, own spec);
`DROP TYPE`/`DROP DOMAIN` interchangeable (shared namespace);
`information_schema`/REST/GraphQL type surfacing = follow-ups.
**Locked-decision changes:** none.

## Item 150 — Upsert `INSERT … ON CONFLICT` + PostgREST resolution wiring   [SHIPPED — on `feat/150-upsert-on-conflict`, PR raised on push]   2026-08-03

**Branch:** `feat/150-upsert-on-conflict` | **Type:** Improvement
**Summary:** `ON CONFLICT [(col)] DO NOTHING | DO UPDATE SET … [WHERE …]`
with `EXCLUDED.*` on a single PK/UNIQUE conflict target
(`docs/backlog/150_upsert_on_conflict.md`). The first deliberate ACID-
write-path extension since M1, built as **routing, not a new write path**:
the conflict probe reuses `enforce_unique`'s phantom-lock-then-snapshot
pattern, and the `DO UPDATE` arm calls `apply_single_row_update` —
extracted verbatim from `exec_update`'s per-row loop — so upsert shares
HOT/non-HOT writes, undo, index maintenance, and FK/UNIQUE re-checks with
plain UPDATE. RLS fail-closed on both arms (update-arm `USING` mismatch =
error, not skip; post-image `WITH CHECK`); column grants = INSERT on
inserted cols + UPDATE on SET targets + SELECT on RHS/WHERE; NULL never
conflicts. REST: `on_conflict=<col>` + `Prefer: resolution=
merge-duplicates|ignore-duplicates` on `POST /rest/v1` — removes item
139's documented exclusion. sqlparser 0.62 native `OnConflict` AST.
Implemented by a Sonnet subagent; orchestrator re-ran every gate.

**Latent MVCC bug found & fixed (the item's biggest yield):** the spec's
required concurrency test exposed a pre-existing, severe read-path bug —
`heap::get_visible_cached`/`get_visible_with_rid` followed at most **one**
HOT-chain hop (a documented-but-false "chains are length 1" assumption).
After ≥2 sequential HOT updates on a PK/UNIQUE-indexed row, the unique
index's candidate under-resolved to "no visible version", letting a
duplicate-key INSERT slip past `enforce_unique` — **two live rows with the
same key, empirically reproduced.** Fix: walk the chain (bounded
`MAX_HOT_CHAIN_HOPS` defensive cap); pure read-path change, no
format/WAL/bufferpool touch, D5 untouched; dedicated regression test
independent of upsert. **Plausibly related to the open item-16 concurrent-
visibility anomaly** (same under/over-resolution family on B-tree-indexed
churn) — a retest of that repro is flagged in MEMORY, not assumed fixed.

**Benchmarks:** none — no new hot-path work for non-upsert statements
(probe only runs when `ON CONFLICT` is present); the HOT-chain walk adds
O(chain-length) page reads where the old code gave up (wrong) after 1 —
correctness over the previous under-read, flagged for the next full bench
report per §0.6.
**Verification (orchestrator-rerun):** clippy `--all-features
--all-targets -D warnings` · fmt · plain `cargo test --no-run` · item150
**20/20** · item150_rest **7/7** · **crash 56/56** (54 pre-existing + 2 new
upsert injection points) · constraints **30/30** · item148 **16/16** ·
item147 **8/8** · item24_rls_with_check **8/8** · server_rest **30/30**.
**Known limitations:** composite conflict targets, `ON CONSTRAINT`, MERGE,
GraphQL upsert = documented non-goals; `EXCLUDED` recognized as a
pseudo-qualifier anywhere in expression context (a real table named
`excluded` would be shadowed — documented in parser.rs).
**Locked-decision changes:** none.

## Item 149 — Row triggers v1 (BEFORE/AFTER, same-transaction)   [SHIPPED — on `feat/149-row-triggers`, PR raised on push]   2026-08-03

**Branch:** `feat/149-row-triggers` (stacked on item 150) | **Type:** Improvement
**Summary:** `CREATE TRIGGER {BEFORE|AFTER} {INSERT|UPDATE|DELETE} ON t
[FOR EACH ROW] EXECUTE FUNCTION fn` + `DROP TRIGGER`
(`docs/backlog/149_row_triggers.md`). Zero-param item-147 functions fire
per row **inside the same transaction** — the unified-commit thesis
applied to compute: an AFTER trigger's audit row commits atomically with
the triggering row, no outbox (crash-proven, p149a/p149b).
`NEW.<col>`/`OLD.<col>` lexically rewritten to synthesized `$n` binds
(string-literal-aware; parsed once per firing statement, rebound per row).
Locked v1 semantics: errors veto; name-order firing; **no cascading**
(nested statements fire no triggers — the entire recursion story; makes
the `updated_at` stamp pattern terminate); BEFORE cannot modify NEW;
superuser-only DDL; body runs as the embedded identity (cron trust
posture). `TriggerDef` catalog-persisted (serde-default, no
FORMAT_VERSION bump); `DROP TABLE` purges triggers;
`DELETE /functions/{name}` rejected while referenced. Fires on plain DML
and upsert's `DO UPDATE` arm (shared `apply_single_row_update`).
Implemented by a Sonnet subagent; orchestrator re-ran every gate.

**Benchmarks:** none run — trigger-free tables take exactly the pre-149
code paths (one catalog check per statement, verified by unchanged
regression suites). Honest §0.6 flags for the next stress pass:
(1) triggered tables lose the UPDATE batch paths + DELETE fast paths
(forced per-row); (2) DML on a triggered table takes the exclusive
catalog lock (conservative — a trigger body's nested statement reuses the
outer `CatalogHandle`), serializing triggered-table writers. Both are
deliberate correctness-first choices to be measured, not assumed cheap.
**Verification (orchestrator-rerun):** clippy `--all-features
--all-targets -D warnings` · fmt · plain `cargo test --no-run` · item149
**20/20** · item150 **20/20** (regression incl. upsert-fires-UPDATE-
triggers) · **crash 58/58** (56 + p149a/p149b audit-atomicity) · item148
**16/16** · item147 **8/8** · constraints **30/30** · server_rest **30/30**.
**Known limitations (documented v1 non-goals):** FOR EACH STATEMENT ·
WHEN · UPDATE OF · INSTEAD OF · NEW-modification in BEFORE ·
cascading/recursion · invoker-mode bodies. Function bodies are re-validated
at each firing (redefinition caught at next fire, not proactively).
**Locked-decision changes:** none.
