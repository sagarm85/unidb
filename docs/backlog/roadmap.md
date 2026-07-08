# unidb — Future Roadmap & Decision Log

> Durable roadmap: the positioning decision, what has shipped, the planned
> tracks, the parallel-worktree lane map, and a per-session record of what
> was decided and achieved. This *distills* `CLAUDE.md` / `MEMORY.md` /
> `PROGRESS.md` — when it disagrees with them, they win.
> **Last updated: 2026-07-08.**

---

## 1. Positioning (the decision)

The charter stays honest, sharpened toward an **AI-native multi-model** identity:

- unidb is **the transactional database for AI-native apps** — relational +
  vector + graph + events in **one commit** — so a team never stitches
  together Postgres + pgvector + a graph DB + Kafka.
- **Performance goal: Postgres-*class* on single-model (match, not beat);
  win decisively on the cross-domain transactional workload and on developer
  experience.** We do **not** chase "beat Postgres/MySQL generally" — that
  reverses `CLAUDE.md` §1, is team/years-scale, and dilutes the moat.
- **Columnar / HTAP (OLAP) is parked as an explicit future Track E**, gated
  on real analytics demand — not pursued now.

**Why (the honest framing):** vector search and graph traversal are
*index-probe* (operational) workloads, not *scan-and-aggregate* (OLAP) — so
"we already have vector + graph" does **not** make unidb analytical. unidb is
OLTP-shaped (row store, WAL, MVCC, single-writer), a deliberate generalist
across four **operational** models. Multi-model breadth and OLAP
specialization are opposite axes; doing both at once is the
maximum-difficulty path.

---

## 2. What's shipped (baseline)

| Milestone | Capability delivered |
|---|---|
| M0 | Single-file paged storage, buffer pool, WAL, ARIES crash recovery, control file |
| M1 | MVCC transactions (RC/RR), SQL subset, catalog, JSON columns, RLS |
| M2 | `VECTOR(n)` type, async HNSW index, `NEAR`, full-text inverted index |
| M3 | Graph edges, edge-list index, Cypher read subset |
| M4 | WAL-derived event queue, durable consumer offsets, `vacuum_events` |
| M5 | REST server + verify-only JWT + SSE subscribe + `/metrics` |
| M6 / M7 / M8 | B-Tree index + index-assisted SELECT · CSR graph index · Rust attach client |
| M9 (perf) | **Group commit + read-only fsync skip + concurrent reads (ReadHandle)** — merged via PRs #2–#4 to `main`, 2026-07-08 |
| Track D | Semantic search: per-index cosine metric (`Metric::{Euclidean,Cosine}`) + `unidb-embed` CLI (embed/search over REST) — `main`, 2026-07-08 |
| M11 | SQL constraints: PK / FK / UNIQUE / NOT NULL / CHECK / DEFAULT — parsed, persisted, enforced on write — landing 2026-07-08 |

---

## 3. The blend roadmap (planned)

| Track | Deliverables | Achieves | Effort* | Phase |
|---|---|---|---|---|
| **A — Engine maturity** | ~~Constraints (PK/FK/UNIQUE/NN/CHECK/DEFAULT)~~ ✅ done (M11) · GC/vacuum (M10) · ~~group commit + concurrent reads~~ ✅ done · buffer pool + real FSM · Phase 2 SQL (joins/agg/ORDER BY) + cost-based optimizer · async replication + PITR/backup | Postgres-**class** correctness, durability, standard SQL, HA; no space leak | ~8–10 remaining | P1→P3 |
| **B — Studio UI** | SQL + Table editor · realtime changes feed · graph explorer ★ · vector playground ★ · metrics/logs · (file upload ⚙, auth ⚙ after engine work) | Visual test/admin console; showcases the multi-model thesis | ~3–4 (+engine deps) | P1→P2 |
| **C — GraphQL** | Catalog-auto-generated schema + edge-traversal resolvers | Typed graph API; framework/AI-friendly | ~2 (after Phase 2 SQL) | P2→P3 |
| **D — Semantic search** | ~~Embedding CLI/client · cosine metric~~ ✅ done · filtered vector search · (optional search UI) | End-to-end AI semantic search on the shipped vector engine | ~0.5 remaining | P1→P2 |
| **E — Columnar / HTAP** *(parked, gated on demand)* | Columnar segment store + vectorized executor — **only if analytics demand appears** | OLAP scan performance; true HTAP | ~5–7 (rewrite/team-scale) | Deferred |

*Effort in rough milestone-units, where 1 unit ≈ what M6/M7/M8 each were.
★ = unidb-only, no Supabase equivalent. ⚙ = needs engine work before the UI panel.

---

## 4. End-state positioning (after A–D; E deferred)

| Dimension | Where we land | Verdict |
|---|---|---|
| Single-model OLTP throughput | Postgres-class, not Postgres-beating | **Match** (concede the sprint, honestly) |
| Cross-domain transaction (row+vector+edge+event, one commit) | Beat the assembled Pg + pgvector + graph DB + Kafka stack | **Beat** |
| Developer experience / AI-native adoption | One store, one commit, one API vs four systems | **Beat** |
| Standard-DB completeness (ACID, MVCC, constraints, GC, optimizer, HA) | Maturity parity via Track A | **Match** |
| Operational simplicity | One node vs four systems | **Beat** |
| OLAP / analytical scans | Conceded unless Track E is built | **Concede** (escape hatch parked) |

**Net end-state:** a mature, Postgres-class, **AI-native multi-model
transactional database** — CRUD + vector + graph + events in one commit —
with a Supabase-style console, REST + GraphQL + attach APIs, and semantic
search. Wins on integration, DX, and cross-domain correctness; honestly
concedes raw single-model speed and OLAP; keeps a columnar/HTAP escape hatch
ready.

---

## 5. Parallel-worktree lane map

Governing rule: **worktree parallelism is safe only across disjoint file
sets.** All storage/txn-core work is a **single serial lane** (one worktree
at a time). Parallelism comes from running that one core lane alongside
genuinely disjoint lanes. Keep the main repo dir **on `main` as the
integration home base**; develop only in sibling worktrees (`../unidb-<name>`,
standard layout — not under `.claude/`).

**Lane → Track → worktree mapping.** Lanes are named **Core / SQL / Surface** so
they don't collide with the Track letters A–E (note especially: the old "Lane E"
is *not* Track E — Track E is columnar/HTAP, parked):

| Lane | = Tracks | Worktree dir(s) |
|---|---|---|
| **Core** | Track A storage slice (M10 vacuum → buffer/FSM) | `../unidb-vacuum` |
| **SQL** | Track A SQL slice (constraints) → Phase 2 SQL + optimizer | `../unidb-constraints` |
| **Surface** | Track B (UI) · Track C (GraphQL) · **Track D (embed/cosine)** | `../unidb-embed`, `../unidb-studio`, `../unidb-graphql` |

| Lane | Owns (files) | Internal order | Parallel-safe with | Conflict watch |
|---|---|---|---|---|
| **Core** *(serial, ONE worktree only)* | `heap` `bufferpool` `wal` `txn` `mvcc` `recovery` `read_handle` · core of `lib.rs` · `tests/crash` | M10 vacuum → buffer-pool/FSM | SQL, Surface | `lib.rs` |
| **SQL** *(query/capability)* | `catalog` `sql/parser` `sql/logical` `sql/executor` | constraints → Phase 2 SQL → optimizer | Core, Surface | `lib.rs` (execute_sql wiring), `sql/executor` |
| **Surface** *(peripheral / new surface, near-zero core overlap)* | new crates/dirs: `studio/`, `unidb-graphql/`, `unidb-embed/` · `server/` additions · small `vector.rs` | UI · GraphQL · embedding CLI + cosine — any order | Core, SQL | almost none |

**Operating rules:**
1. Only ever **one** Core worktree. SQL and Surface never touch `heap`/`bufferpool`/`wal`/`txn`.
2. **`lib.rs` is the #1 conflict source.** Off the core lane, edits must be *additive method insertions*, never restructuring.
3. **Narrative docs (`MEMORY.md`, `PROGRESS.md`, `engine_design.md`) conflict constantly** — each lane appends to its own dated subsection; merge the narrative by hand at land-time.
4. **Land the core lane to `main` frequently** (small, fast-forward-able) so Q and P rebase onto fresh `main` and conflicts stay tiny.

**Worktree setup (run from the main repo dir):**
```bash
git worktree add -b core-vacuum     ../unidb-vacuum       main   # Core lane: M10 (concurrent reads already merged)
git worktree add -b sql-constraints ../unidb-constraints  main   # SQL lane
git worktree add -b surface-embed   ../unidb-embed        main   # Surface lane (Track D: embed/cosine)
# cleanup when a lane lands:  git worktree remove ../unidb-<name>
```

---

## 6. Current status & next actions

- **Core lane:** group-commit / concurrent-reads **DONE** (PRs #2–#4, on `main`).
  **Next: M10 heap vacuum / GC** — plan in [`m10_heap_vacuum_gc.md`](m10_heap_vacuum_gc.md).
  The vacuum horizon must include active `ReadHandle` readers, not just the
  writer's active transactions (build on top of the concurrent-read model).
- **SQL lane:** constraints (PK/FK/UNIQUE/NOT NULL/CHECK/DEFAULT) —
  **implemented as M11 on branch `sql-constraints`, pending hand-merge to
  `main`** (see `PROGRESS.md`'s M11 entry). Parser now maps column options +
  table constraints into new `ColumnConstraints`/`TableConstraints` catalog
  fields; enforced on INSERT/UPDATE. UNIQUE uses a synchronous heap scan, not
  the async B-Tree index (correctness — `Ready` ≠ current, the M7 lesson); FK
  is referenced-table existence only. **Next in the SQL lane: Phase 2 SQL**
  (OR/ORDER BY/LIMIT/aggregates/JOIN) + cost-based optimizer
  (`phase2_sql_capability_expansion.md`).
- **Surface lane:** embedding CLI + cosine (**Track D**) — **DONE**
  (branch `surface-embed`, 2026-07-08; see `PROGRESS.md`'s Track D entry).
  `vector.rs` gained a per-index `Metric::{Euclidean,Cosine}` (cosine =
  `1 - cos`, rebuild on metric change); new `unidb-embed/` crate is a CLI
  (`embed-insert`/`search`) that embeds text via a pluggable HTTP endpoint and
  stores/searches via the `unidb-attach` client. Embedding generation stayed
  client-side (no model deps in the engine). Remaining Track D polish: expose the
  metric through `CREATE INDEX ... USING HNSW <metric>` (SQL lane) and an
  optional search UI (Track B). Still holds Track B (Studio UI) and Track C
  (GraphQL).

---

## 7. Decision & session log (newest first)

### 2026-07-08 — M11 SQL constraints landed (SQL lane, branch `sql-constraints`)
- Constraints (PK/FK/UNIQUE/NOT NULL/CHECK/DEFAULT), column- and table-level,
  now parsed off `CREATE TABLE` (previously `convert_create_table` dropped
  `c.options`), persisted on the catalog (`ColumnConstraints`/`TableConstraints`,
  all `#[serde(default)]` — no `FORMAT_VERSION` bump), and enforced on
  INSERT/UPDATE (DEFAULT → NOT NULL → CHECK → UNIQUE → FK).
- **UNIQUE uses a synchronous heap scan, deliberately NOT the async B-Tree
  index** — `IndexStatus::Ready` ≠ "reflects every write" (the M7 CSR lesson);
  a stale index entry is a false "no conflict." FK is referenced-table
  existence only (no row-level RI / cascades).
- Disjoint from Core/Surface lanes: no storage-core or `lib.rs` changes;
  `server/error.rs` got additive 4xx arms (small cross-lane touch). Rebased
  onto the Track D merge — narrative-doc conflicts (MEMORY/PROGRESS/roadmap)
  resolved keep-both. Full record in `PROGRESS.md`'s M11 entry + `MEMORY.md`.

### 2026-07-08 — Track D shipped (semantic search: cosine metric + embedding CLI)
- Surface lane, worktree `../unidb-embed`, branch `surface-embed`. Only engine
  file touched: `src/vector.rs` (added per-index `Metric::{Euclidean,Cosine}`,
  cosine = `1 - cos`, `set_metric` rebuilds the HNSW graph). New workspace crate
  `unidb-embed/` (CLI: `embed-insert`/`search`) reuses the `unidb-attach` client;
  embedding generation is client-side via a pluggable HTTP endpoint (key via env
  var) — no model/network dep in the engine. `cargo test --workspace` + clippy
  `-D warnings` + fmt clean. Full record in `PROGRESS.md`'s Track D entry.
- Deferred: `CREATE INDEX ... USING HNSW <metric>` wiring is SQL-lane work.

### 2026-07-08 — roadmap consolidation + parallelization
- **Positioning decided (the blend):** AI-native multi-model identity;
  Postgres-*class* not Postgres-beating; columnar/HTAP parked as Track E.
  Explicitly declined to reverse `CLAUDE.md` §1.
- **Placement analysis:** unidb is OLTP-shaped (row store, tuple-at-a-time),
  a deliberate generalist; vector + graph are index-probe workloads, not
  OLAP — so multi-model ≠ analytical.
- **Cost sketch:** Path 0 (GC/vacuum) is the mandatory correctness fix;
  Path B (OLTP-max) aligned & mostly done; Path A (OLAP/HTAP) is
  rewrite/team-scale, deferred.
- **Four-track roadmap** defined (user's M0–M3 → Tracks A–D, + parked E) and
  a **parallel-worktree lane map** (Core / SQL / Surface) with operating rules.
- **Status correction:** the M9 perf line (group commit + concurrent reads)
  is already merged to `main` via PRs #2–#4; the Core lane advances to **M10**.
- Backlog plans written: `m9_python_embedded_bindings.md`,
  `m10_heap_vacuum_gc.md`, this `roadmap.md`.

### 2026-07-07/08 — M6/M7/M8 close-out + doc hygiene
- Merged M8 (attach client) from its worktree; **found & fixed a real M7
  CSR-traversal correctness bug** during merge verification (CSR preferred
  once `Ready` could hide a just-created edge; reverted to `EdgeIndex`).
- Cleaned up stale worktrees/branches (M7 CSR plan-only worktree, M8 worktree).
- Doc-staleness audit: corrected `docs/design/engine_design.md` (M0–M8; CSR
  correction), `docs/README.md`, `m8_attach_client_plan.md` (→ SHIPPED).
- Added a **`CLAUDE.md` §9 rule**: check `README.md` + all `docs/` for
  staleness before every push/PR, not just `PROGRESS.md`/`MEMORY.md`.

---

## 8. Backlog index (durable plans)

- [`roadmap.md`](roadmap.md) — this document.
- [`m10_heap_vacuum_gc.md`](m10_heap_vacuum_gc.md) — heap vacuum / MVCC GC (next in the Core lane).
- [`group_commit_and_read_concurrency.md`](group_commit_and_read_concurrency.md) — the M9 perf line (largely shipped via PRs #2–#4).
- [`phase2_sql_capability_expansion.md`](phase2_sql_capability_expansion.md) — OR / ORDER BY / LIMIT / aggregates / JOIN (SQL lane).
- [`m9_python_embedded_bindings.md`](m9_python_embedded_bindings.md) — PyO3 in-process bindings (future).
- [`m8_attach_client_plan.md`](m8_attach_client_plan.md) — shipped (kept as record).
