# unidb — Roadmap & Scaling Plan

> The single forward-looking plan: positioning, what's shipped, the honest
> gap to a real database, and the phased path to close it. Correctness and
> performance are gates, not features. Distills `CLAUDE.md` / `MEMORY.md` /
> `PROGRESS.md` — when it disagrees with them, they win.
> **Last updated: 2026-07-22** (refresh: §4.1's stale "next up" list replaced
> with a pointer to `backlog_index.md`'s live Next-up section; §8 bridging note
> on where post-item-15 decisions are recorded). Supersedes the earlier per-milestone backlog
> docs (now shipped and recorded in `PROGRESS.md`, or folded into a phase below).
> **Backlog file naming + lifecycle:** see `CONVENTIONS.md`. The `phase<N>_`
> prefix is reserved for the numbered phases *in this file*; other efforts
> (Milestone / Improvement / Performance) use a plain descriptive slug.

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
| **Phase 1** | **ACID hardening (complete):** full-page-writes (torn-page) · fsync-failure handling · `alloc_page` chunked growth + configurable pool + real FSM · isolation correctness (RC re-eval + SSI) · auto-checkpoint |
| **Phase 2** | **Real data model (complete):** DECIMAL/TIMESTAMP/FLOAT/UUID/BYTEA/DATE/TIME · ALTER/DROP/TRUNCATE + request-level DDL rollback · SERIAL · prepared statements + `$n` bind params |
| **Phase 3** | **Multi-model durable storage:** P3.a durable B-Tree · P3.b durable full-text + edge index (CSR retired) · P3.d chunked/streamed large objects — all shipped (no rebuild on open; big files without OOM). P3.c on-disk vector — spike complete (IVF-Flat, recall@10=1.0 at nprobe=4; **production wiring is the one remaining follow-up PR**). |

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

**Per-phase detailed specs** (checkpoints, files, gates, locked-decision impact):
[`phase1_acid_hardening.md`](phase1_acid_hardening.md) ·
[`phase2_data_model.md`](phase2_data_model.md) ·
[`phase3_durable_storage.md`](phase3_durable_storage.md) ·
[`phase4_query_power.md`](phase4_query_power.md) ·
[`phase5_concurrency.md`](phase5_concurrency.md) ·
[`phase6_ops_ha.md`](phase6_ops_ha.md).

**Phases 1–6 are all shipped** (see `PROGRESS.md` / `MEMORY.md`), plus the
commit-time-fsync default (PR #24) and the Postgres baseline comparison (PR #25).
The §2/§3 tables below predate the Phase 4–6 completions and are kept for
historical shape — `MEMORY.md` is authoritative for current state.

### 4.1 Next up — how the queue is tracked

_(Refreshed 2026-07-22: this section used to carry its own ranked table of
post-Phase-6 items — durable FSM, autovacuum, CRUD perf — all long since
shipped, see `backlog_index.md` rows 09/10/13. The list is not duplicated here
anymore.)_ The **live ranked "Next up" list lives in
[`backlog_index.md`](backlog_index.md)** — its registry is the at-a-glance
pending/completed tracker, and its Next-up section is reordered freely as
priorities shift (priority is not the ID). Items are filed one-per-file as
`NN_<slug>.md` per [`CONVENTIONS.md`](CONVENTIONS.md); this roadmap stays the
strategic frame (positioning, phases, lanes, parked list), not the queue.

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

### 2026-07-22 — where the log moved (bridging note)
From item 15 onward, decisions and session outcomes are recorded **per-item in
the numbered backlog files** (`docs/backlog/NN_<slug>.md`, indexed in
`backlog_index.md`) **and in `PROGRESS.md`**, not appended here. The entries
below are the pre-numbered-backlog record (through 2026-07-10) and are kept
as-is for history.

### 2026-07-10 — CRUD-perf Phase A + Phase B filed (write + scan path)
- Multi-model CRUD-stress report (`benches/decompose.rs` Table 3/3.1) vs
  matched-durability PG 18.4 surfaced large single-model losses: UPDATE 0.11×,
  DELETE 0.20×, filtered SELECT 0.15×, COUNT scan ~8×. Each op is one
  `begin…commit` (one fsync/op) ⇒ the gap is **CPU + WAL volume, not durability**.
- Root causes traced to lines: **RC2** `apply_durable_index_writes`
  (`executor.rs:178`) re-indexes unchanged columns, one full-page `WAL_INDEX`/row
  (the 0.11× UPDATE killer); **RC1** `matching_rows` (`executor.rs:1109`) full-scans
  for UPDATE/DELETE, never uses the index; **RC3** whole-row `Vec<Literal>` decode +
  per-row re-snapshot; **RC4** COUNT decodes columns it discards.
- Filed as **[`crud_performance.md`](crud_performance.md)**
  (Core-lane, two PRs). Phase A = write path (A1 skip-unchanged-index is the
  recommended first commit — largest single win), Phase B = decode pushdown.
  No §3 decision reopened; INSERT (fsync-bound, at parity) explicitly out of scope.
  **To be run in a later session.**

### 2026-07-10 — Durable on-disk FSM + catalog page-list shipped; index/heap write-concurrency filed
- Shipped the durable FSM (branch `durable-fsm`, PR #29): the SQL page directory +
  free-space map move off the catalog blob into a per-table durable `DiskBTree`
  keyed `page_id → free_bytes` (`TableDef.fsm_meta`), O(1) open, atomic heap grow,
  vacuum-durable reclamation — closing the O(heap-pages) `HeapFull` ceiling.
  Crash harness 26→28 (P27/P28). B-accept: marginal SQL-insert cost goes from
  rising-then-`HeapFull` (65→108→173 µs/row, dies ~876 pages) to flat ~17–28 µs/row
  past 2,000 pages. `durable_fsm_catalog_pagelist.md` → SHIPPED.
- **Concurrency finding (measured, honest):** concurrent SQL-insert throughput is
  **flat ~1,250 commits/s at 8 writers across every table size (0 → 7,500 pages)**,
  at Postgres parity — bounded by the group-commit fsync, *not* by the page-list
  write the FSM removed. So the FSM makes high-scale concurrent SQL writes
  *possible and flat* (before dies at ~876 pages) but does **not** raise the
  per-commit ceiling. Raising it needs page/index-layer concurrency → filed
  **`docs/backlog/index_write_concurrency.md`** (latch-coupled "crabbing" B-tree
  descent + spread the heap insertion target across writers). The durable-throughput
  edge remains the one-commit multi-model write, not a faster single-table commit.

### 2026-07-09 — Postgres baseline comparison shipped; two hardening items filed
- Ran the standard-vs-standard fitness check (unidb vs PostgreSQL 18.4, both as
  shipped, both durability lenses; PR #25, `pg_baseline_comparison.md`). Verdict:
  a **solid, SQLite-class-and-then-some foundation** — parity on durable commits
  at matched F_FULLFSYNC durability, ~5× win on embedded point reads, concurrent
  writes scale on both raw and SQL paths (refuting a filed prediction). Lens-1's
  ~40× Postgres lead is a macOS `open_datasync` durability illusion.
- Two honest gaps surfaced → filed as §4.1 Next-up items (both Core-lane, own PRs):
  (1) **durable FSM + O(1) table-page representation** — the SQL bulk-load
  `HeapFull` at ~145k rows is an O(heap-pages) catalog-blob cap (not the "lazy FSM"
  the first pass claimed; corrected inline in `PROGRESS.md`/`MEMORY.md`),
  `durable_fsm_catalog_pagelist.md`; (2) **autovacuum** — closes the churn/bloat
  automation gap, `autovacuum.md`. Neither reopens a §3 decision.

### 2026-07-08 — Phase 3 P3.d (large-object storage) shipped
- Large objects are stored **out-of-line, chunked (~7 KiB), and streamed**: a
  blob is a sequence of chunk rows in a `__lobs__` system table indexed by a
  durable `DiskBTree` on `lob_id` (reuses P3.a) — so it is atomic with the
  caller's transaction, crash-recovered, and vacuum-reclaimable with **zero new
  storage format**. `Engine::put_large_object`/`read_large_object`/
  `delete_large_object` hold one chunk at a time (multi-GB without OOM). Crash
  point **P16** (harness 17→18). Deferred: transparent BYTEA-toast + streaming
  REST routes. **Phase 3 is effectively complete** — the only remaining item is
  the P3.c on-disk-vector *production* wiring (the spike is done).

### 2026-07-08 — Phase 3 P3.c (on-disk vector) spike complete
- Spiked the durable on-disk vector index (blueprint mandates "spike first,
  validate recall before committing"). **Chose on-disk IVF-Flat** for v1: its
  cell posting lists are a durable `DiskBTree` (reuses P3.a), centroids in
  bounded RAM; DiskANN/Vamana parked as a higher-recall option behind the same
  interface. Recall validated (`benches/vector_recall.rs`): **recall@10 = 1.000
  at nprobe=4** vs. ground truth, 4 KB RAM, 24 ms build (in-RAM HNSW: 30 s build
  for 1,200 vectors). Spike surfaced + fixed a real `DiskBTree` duplicate-key bug
  (a run straddling a leaf boundary under-returned) that affected P3.a/P3.b too.
  Prototype `src/disk_vector.rs`; findings `docs/design/p3c_vector_spike.md`.
  **Production wiring is a follow-up PR** (not rushed, per the blueprint).

### 2026-07-08 — Phase 3 P3.b (durable full-text + edge index; CSR retired) shipped
- Full-text (inverted) and the edge-adjacency index (`__edges__.from_id`) are
  now durable `DiskBTree`s — reusing P3.a's `WAL_INDEX` machinery, **no new
  format version**. Both read from disk on open; `rebuild_edge_index` and the
  full-text rebuild are gone. New Rust-API read path `Engine::search_fulltext`.
- **CSR retired** (decision, recorded here): the M7 CSR index was consulted by
  no read path after its own M7 traversal wiring was reverted, and adjacency is
  now served durably by the edge index, so its rebuild-on-open + warm-keeping
  were removed. The async index worker now serves only the vector (Hnsw) index
  (P3.c will make that durable too and retire the worker). Not a locked (§3 D-)
  decision reversal — a dead-code retirement, documented for evidence.
- Crash harness 15 → **17** (P14 durable full-text, P15 durable edge index).

### 2026-07-08 — Phase 3 P3.a (durable B-Tree) shipped
- Started Phase 3 (the moat) on the Core lane branch `durable-storage`. First
  checkpoint P3.a: the B-Tree secondary index is now a durable on-disk B+tree
  (`DiskBTree`) — node pages in the shared page store, buffer-pool-managed,
  WAL-logged as full node-page images (new redo-only `WAL_INDEX`), crash-
  recovered, and **no longer rebuilt on `Engine::open`** (removed from
  `rebuild_secondary_indexes`; moved off the async worker to the synchronous
  write/read path). A stable per-index meta page (id in `ColumnDef.index_root`)
  points at the root, so a root split never rewrites the catalog. Crash harness
  14 → **15** (P13: total data-file loss recovered from the WAL). `FORMAT_VERSION`
  4 → **5** (D9). No locked decision reversed (D1/D4/D5/D9 strengthened). See
  `PROGRESS.md` → P3.a for the open-cost benchmark. P3.b–d pending.

### 2026-07-08 — Phase 1 (ACID & storage foundation) COMPLETE
- All five checkpoints shipped on the `acid-hardening` Core lane, one PR each:
  P1.a full-page-writes (#6), P1.b fsync-failure handling (#7), P1.c
  `alloc_page` chunked growth + configurable pool + real FSM (#8), P1.d
  isolation correctness — RC re-evaluation + SSI (#10), P1.e auto-checkpoint.
- Closed every Tier-0 correctness hole (torn-page, fsync, isolation) plus the
  Tier-3 `alloc_page`/pool/FSM growth blocker and manual-checkpoint WAL-bloat.
  Crash harness 11→**14** (P11 torn-page, P12 fsync-failure); `FORMAT_VERSION`
  3→4; no locked decision reversed (D1/D5/D9/D10–D12/D3 completed/strengthened).
  Per-checkpoint benchmarks in `PROGRESS.md`. The feature-freeze gate is closed;
  Phases 2/3/4 may proceed.

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
