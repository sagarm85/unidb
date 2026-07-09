# Where unidb stands — vs. relational databases and Supabase

> Honest, feature-wise positioning. Written 2026-07-09, reflecting **M0–M11 +
> Phases 1–5 shipped** (Phase 6 — ops/HA — in progress). This doc follows the
> project's anti-overclaiming ethos (`CLAUDE.md` §6): it states the wins *and*
> the gaps plainly.

## What unidb is (and isn't)

unidb is a **database engine** — embedded (SQLite-shaped), single-primary, with
an *optional* REST server — that unifies four data models over **one page store,
one WAL, one buffer pool, one transaction manager**: relational CRUD, vector
search, graph edges, and a WAL-derived event queue.

It is **not** a platform. So the comparisons below are asymmetric on purpose:

- **vs. Postgres** is engine-to-engine.
- **vs. Supabase** is really *unidb + its small REST server* vs. *Postgres + a
  whole product layer* (SDKs, dashboard, hosted auth, edge functions, hosting).

Mental model: **"SQLite for AI-native, multi-model apps"** — not a Postgres or
Supabase replacement, but a uniquely capable engine for the one thing they
can't do atomically (see [The moat](#the-moat)).

## vs. relational databases (Postgres as the yardstick)

| Area | unidb | Postgres | Verdict |
|---|---|---|---|
| ACID / WAL / crash recovery | ARIES steal+no-force, full-page-writes, fsync-fault handling, auto-checkpoint, crash-injection harness | mature, decades-hardened | ✅ real parity in *design* (not hardening) |
| MVCC / isolation | RC + RR/SI + Serializable (SSI) | same set | ✅ parity |
| Concurrency | concurrent writers, lock manager + wait-for-graph deadlock detection, group commit; scales 3.68×/8 cores on **raw CRUD** | scales broadly incl. indexed writes | ⚠️ only raw CRUD scales; SQL/graph/LOB writes serialize |
| SQL surface | practical subset: joins (hash/merge/index-nested-loop), aggregates, GROUP BY/HAVING, ORDER BY, DISTINCT, LIMIT/OFFSET, subqueries (scalar/IN/EXISTS, correlated), CTEs, cost-based optimizer + stats, EXPLAIN/ANALYZE | full ANSI + window functions, triggers, views, stored procedures, PL/pgSQL | ❌ big gap — no window functions, triggers, views, procedures |
| Types | INT, TEXT, BOOL, JSON, DECIMAL, TIMESTAMP, VECTOR | vast type system + extensions | ⚠️ core types only |
| Constraints | NOT NULL, UNIQUE, CHECK, FOREIGN KEY | same + more | ✅ the essentials |
| Indexes | B-Tree, full-text (inverted), vector (HNSW/IVF), graph edge — all **durable, crash-recovered, O(1) open** | B-Tree, GIN, GiST, BRIN, … | ⚠️ fewer kinds, but multi-model |
| Security | RLS, verify-only JWT | roles/GRANT, RLS, SCRAM, TLS, SSO | ❌ roles/GRANT/full auth in progress (P6.e/f) |
| Replication / HA / PITR | **in progress (Phase 6)** | streaming + logical replication, PITR | ❌ not shipped yet |
| Ecosystem (drivers, tooling, extensions) | REST + Rust attach client | enormous | ❌ not close |
| Single-model CRUD throughput | Postgres-class *at best*; expects to lose | — | ❌ **conceded by charter** (§1) |

## vs. Supabase (Postgres + platform)

| Capability | unidb | Supabase |
|---|---|---|
| Auto REST API | ✅ hand-built REST server | ✅ PostgREST (richer filtering/embedding) |
| Realtime / subscribe | ✅ SSE + WAL-derived event queue | ✅ Realtime (WAL → websocket, row-level) |
| Vector search | ✅ **native**, transactional (HNSW/IVF) | ✅ pgvector |
| Auth | ⚠️ JWT *verify-only* — no user management | ✅ GoTrue: signup, OAuth, magic links, MFA |
| Storage / large objects | ✅ in-engine chunked LOBs | ✅ S3-backed Storage |
| Row-level security | ✅ | ✅ (Postgres RLS) |
| Client SDKs (JS/Python/Flutter/Swift) | ❌ REST + Rust only | ✅ first-class SDKs (its core DX) |
| Dashboard / Studio / migrations UI | ❌ | ✅ |
| Edge Functions / serverless | ❌ (non-goal) | ✅ |
| Hosting / cloud control plane | ❌ (explicit non-goal) | ✅ (the whole product) |

## The moat

**Relational + vector + graph + event-queue writes in ONE ACID transaction, in
one embedded engine.** "Save the row, its embedding, a graph edge, and emit an
event" is **one WAL append and one commit** — not 3–4 systems with no shared
transaction.

- **Postgres** needs pgvector + Apache AGE (or recursive CTEs) + an external
  queue — with *no shared transaction* across them.
- **Supabase** bundles pgvector + Realtime, but a vector store + graph + queue
  still cannot be made atomic together.

Add the **embedded-or-server deployment flexibility** (run it in-process like
SQLite, *or* as a server with read replicas once Phase 6 lands) and that
combination is what neither offers cleanly.

## Honest gaps & maturity caveat

- **Wins:** multi-model atomicity; native durable vector/graph indexes;
  embedded+server flexibility; O(1) open.
- **Loses today:** SQL completeness (no window functions/triggers/views/
  procedures); roles/auth platform; replication/PITR (Phase 6, in progress); the
  entire driver/tooling/extension ecosystem; raw single-model throughput
  (deliberately conceded).
- **Maturity — the big one:** unidb is a genuinely rigorous, correctness-first
  engine (real ACID, MVCC, deadlock detection, concurrent writers, crash-tested),
  but it is **early-stage, not battle-hardened.** Postgres has ~30 years and
  Supabase a large platform team behind it. unidb is "strong prototype → early
  product." Treat it accordingly.

## Bottom line

Reach for unidb when the app is **AI-native and multi-model** — rows + embeddings
+ relationships + events that must change together atomically, ideally embedded.
Reach for Postgres/Supabase when you need SQL completeness, a mature ecosystem,
managed hosting, or a batteries-included app platform. Phase 6 (replicas, PITR,
roles, observability) is what closes the biggest "can I run this in production"
gaps.
