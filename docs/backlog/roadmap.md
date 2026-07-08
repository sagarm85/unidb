# unidb — Roadmap & Scaling Plan

> The single forward-looking plan: positioning, what's shipped, the honest
> gap to a real database, and the phased path to close it. Correctness and
> performance are gates, not features. Distills `CLAUDE.md` / `MEMORY.md` /
> `PROGRESS.md` — when it disagrees with them, they win.
> **Last updated: 2026-07-08.** Supersedes the earlier per-milestone backlog
> docs (now shipped and recorded in `PROGRESS.md`, or folded into a phase below).

---

## 1. Positioning (the principles — unchanged)

- unidb is **the transactional database for AI-native apps**: relational +
  vector + graph + events + **big files**, atomic in **one commit**.
- **ACID is non-negotiable.** No change lands if it weakens durability or
  isolation. The crash-injection harness must stay green and grow.
- **Performance is measured, never assumed** — every phase carries a benchmark.
- **Not a Postgres clone.** We build the standard, correct engine core (ARIES
  WAL, MVCC, buffer pool — the right foundation), but the *moat* is the
  multi-model, AI-native story Postgres doesn't have: durable vector search,
  graph, and large-object storage under one atomic commit.
- **Target scale (confirmed): a strong single node + read replicas** — 100s of
  GB, high read throughput, single primary for writes. Fully distributed /
  sharded write scale is parked (see §7), not a near-term goal.

---

## 2. What's shipped (baseline)

| Milestone | Capability |
|---|---|
| M0–M5 | Storage core · MVCC + SQL subset + RLS + JSON · vector (HNSW) + full-text · graph (Cypher subset) · WAL event queue · REST/JWT/SSE/metrics server |
| M6 / M7 / M8 | B-Tree index · CSR graph index · Rust attach client (`unidb-attach`) |
| M9 (perf) | Group commit + read-only fsync skip + concurrent reads (`ReadHandle`) |
| Track D | Semantic search: per-index cosine metric + `unidb-embed` CLI |
| M10 | Heap vacuum / MVCC GC (`Engine::vacuum()`) |
| M11 | SQL constraints (PK / FK / UNIQUE / NOT NULL / CHECK / DEFAULT) |

The core is architecturally correct — it is not a toy. But it is the **small
version**; §3 is the honest gap to production.

---

## 3. The gap to a real database (honest inventory)

Ranked by severity. **The correctness holes (Tier 0) outrank every
scale/feature item** — they pass tests and demos, then silently lose or corrupt
data under load. Fix them first.

**Tier 0 — silent correctness / data-loss (invisible until the worst moment)**
- **No torn-page protection** (no full-page-writes / double-write) — a crash
  mid-8 KiB-page-write leaves a half-written page; CRC detects it, then the page
  is unrecoverable. *The #1 hole.*
- **Isolation not fully correct** — RC concurrent-update re-evaluation
  (EvalPlanQual) aborts instead of re-reading; `SERIALIZABLE` (SSI) is a no-op
  seam, so write-skew is possible.
- **fsync failure handling** (fsyncgate), ordering.

**Tier 1 — can't build real apps (functional)**
- **Data types far too few** — only Int/Text/Bool/JSON/Vector. No **DECIMAL**
  (money), no **DATE/TIME/TIMESTAMP** (time), no FLOAT/UUID/BYTEA.
- **No ALTER / DROP / TRUNCATE** — no schema evolution.
- **No sequences / SERIAL** — no surrogate keys.
- **No prepared statements / bind params** — injection surface + no plan reuse.

**Tier 2 — can't operate it (ops)**
- **No EXPLAIN** — can't diagnose slow queries. No **backups / PITR**. No
  **users / roles / GRANT**. No **connection model** (single writer thread). No
  **query timeouts / cancel / per-query memory limits**.

**Tier 3 — scale/performance (the 4 flags)**
- **Joins unbuilt** (+ need cost-based optimizer + statistics).
- **Indexes rebuilt on open, RAM-bound** — O(data) startup, won't fit at scale.
- **Single writer thread** — one-core write ceiling; needs concurrent writers +
  a real lock manager (shared/exclusive, wait queues, deadlock detection).
- **Manual checkpoint + single-file rewrite-truncate WAL** — needs
  auto-checkpoint + segmented WAL + slots for multiple consumers/replicas.
- Plus: **`alloc_page` re-maps the whole file per page** (`bufferpool.rs`) —
  fine small, fatal at 100s of GB; fixed 256-frame buffer pool; linear-scan FSM.

**Tier 4 — HA & security**
- Replication / failover · TLS · encryption-at-rest · audit log.

---

## 4. The phased plan

| Phase | Goal | Key workstreams | Lane | Gate |
|---|---|---|---|---|
| **1 — ACID & storage foundation** *(freeze features until done)* | Close the silent correctness holes + growth blocker | Full-page-writes · fsync hardening · `alloc_page` remap fix + large configurable buffer pool + real FSM · isolation correctness (RC re-eval + SSI) · auto-checkpoint | **Core (serial)** | New crash points; write-skew tests; no perf regression |
| **2 — Real data model** | Usable for real apps | DECIMAL, DATE/TIME/TIMESTAMP, FLOAT, UUID, BYTEA · ALTER/DROP/TRUNCATE + transactional DDL · sequences/SERIAL · prepared statements + bind params | **SQL (parallel)** | Type round-trips; no injection surface |
| **3 — Multi-model durable storage** *(the moat)* | Kill rebuild-on-open + RAM ceiling; own the AI/big-file story | Durable paged WAL-logged indexes (B-Tree/inverted/CSR) · durable on-disk vector index (DiskANN-style) · **big-file / large-object storage** (out-of-line + streaming) | Core + new lanes | O(1) open regardless of size; RAM bounded; vector recall bench |
| **4 — Query power** | Real SQL + a brain | Joins (hash + merge) · aggregates/GROUP BY/ORDER BY/subqueries · cost-based optimizer + statistics · EXPLAIN | **SQL (parallel)** | Optimizer picks right plans; join benchmarks |
| **5 — Concurrency & performance** | Multiple writers; lift the single-core ceiling | Concurrent writers (buffer-pool latches, concurrent WAL, concurrent txn mgr) · real lock manager (modes, wait queues, deadlock detection) · connection pooling · timeouts/cancel/memory limits | **Core (serial)** | Concurrency stress; throughput scales with cores |
| **6 — Operations & HA** | Deploy for real | Segmented WAL + replication slots + archiving · streaming replication → read replicas + failover · backups + PITR · users/roles/GRANT · TLS + encryption-at-rest + audit · observability | Core (WAL) + Ops | Replica catch-up; failover + restore drills; security review |

**Why this order:** Phase 1 fixes the invisible correctness holes (mandatory
before anything). Phases 2 + 3 run in parallel with it. Phase 4 gives a real
query engine. Phase 5 (biggest perf unlock) depends on 1 + 3 being solid.
Phase 6 delivers the single-node + read-replica target.

---

## 5. Parallel-worktree lane model

Lanes are **file-disjoint** so worktrees never conflict. Keep the main repo
dir on `main` as the integration base; develop only in sibling worktrees
(`../unidb-<name>`).

| Lane | Owns (files) | Runs phases | Notes |
|---|---|---|---|
| **Core** *(serial — ONE worktree)* | `wal` `heap` `page` `bufferpool` `mmap` `mvcc` `txn` `lockmgr` `recovery` `checkpoint` · `tests/crash` | 1 → 3 (indexes) → 5 → 6 (WAL) | Critical path; the storage/txn core |
| **SQL** *(parallel)* | `catalog` `sql/*` | 2 → 4 | Types, DDL, joins, optimizer, EXPLAIN |
| **Ops / Surface** *(parallel)* | `server/*` · new modules (big-file, TLS, observability) | 2/3/6 pieces | Disjoint from Core and SQL |

**Operating rules:** only one Core worktree ever; `lib.rs` edits off the Core
lane are additive-only; each lane appends its own dated subsection to the
narrative docs (merge by hand at land-time); land the Core lane to `main`
frequently so the others rebase cleanly.

---

## 6. How we start now (Phase 1 + Phase 2 launch)

Two lanes launch immediately — both high-value, fully disjoint:

```bash
cd /Users/sagarmahamuni/Development/AI_World/unidb
git checkout main && git pull --ff-only origin main

git worktree add -b acid-hardening ../unidb-acid   main   # Core  — Phase 1
git worktree add -b sql-types      ../unidb-types  main   # SQL   — Phase 2
# optional 3rd, fully disjoint (server/ only): TLS + query timeouts
# git worktree add -b ops-tls      ../unidb-ops    main
```

Full blueprints (checkpoints, files, gates, locked-decision impact):
Core → [`phase1_acid_hardening.md`](phase1_acid_hardening.md) ·
SQL → [`phase2_data_model.md`](phase2_data_model.md).

**First checkpoint per lane:**
- **Core / `acid-hardening` → P1.a Full-page-writes** — log the whole page image
  into the WAL on first modification after a checkpoint; recovery uses it as the
  clean redo base; new crash-injection point corrupts a page mid-write and
  asserts recovery. Files: `wal.rs`, `bufferpool.rs` (first-touch tracking),
  `recovery.rs`, `checkpoint.rs`, `tests/crash`. **Closes the #1 data-loss hole.**
- **SQL / `sql-types` → P2.a DECIMAL + TIMESTAMP** — `ColumnType::{Decimal(p,s),
  Timestamp}`: catalog variants, LE row encoding, parser, `Literal` variants,
  executor coercion + constraint compatibility. Files: `catalog.rs`,
  `sql/parser.rs`, `sql/logical.rs`, `sql/executor.rs`. **Money + time first.**

Each lane opens a PR per phase-checkpoint with its benchmark + crash-harness
status, same discipline as M10.

---

## 7. Parked / deferred (explicitly, not forgotten)

- **Columnar / HTAP (OLAP)** — gated on real analytics demand; opposite axis to
  the multi-model thesis. Not pursued unless a scan-heavy workload appears.
- **Fully distributed / sharded write scale** — reverses `CLAUDE.md` §1
  (single-primary) and strains cross-model atomicity; a separate, multi-year
  project beyond the single-node + read-replica target.
- **S3 / tiered storage** — relevant only past local-disk economics (TBs);
  reverses D6 (single mmap'd file). Behind Phase 6 replication.
- **Python / multi-language embedded clients** (PyO3 etc.) — orthogonal
  developer-experience feature; revisit after the engine is production-solid.

---

## 8. Decision & session log (newest first)

### 2026-07-08 — adopted the ACID-first phased scaling plan; backlog cleaned
- Ran an expert gap analysis: the user's 4 flags (joins, index durability,
  concurrent writers, WAL/checkpoint) + 12 more, tiered. **Key reframe:
  correctness (torn-page, isolation) outranks scale — fix before scaling.**
- Adopted a 6-phase plan (this doc §4), ACID + performance as gates, multi-model
  (vector/graph/big-file) as first-class, single-node + read-replicas as the
  scale target. Distributed/columnar parked (§7).
- Removed shipped/superseded backlog docs (M8/M10/group-commit → `PROGRESS.md`;
  phase2 SQL → Phase 4; Python bindings → §7 parked). This doc is now the single
  forward plan.
- Prior shipped work this cycle: M9 perf (group commit + concurrent reads),
  Track D (semantic search), M10 (vacuum), M11 (constraints) — all merged to
  `main`; REST API doc audited + live CRUD end-to-end verified.
