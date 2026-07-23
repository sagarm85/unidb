# unidb Documentation

> The map of everything under `docs/`, organized by reader. For design
> decisions, milestone status, and running state, the project-root
> `CLAUDE.md`, `PROGRESS.md`, and `MEMORY.md` remain authoritative — every
> document below is a distillation and yields to them on disagreement.
> _(Restructured 2026-07-22; previously this index omitted `ops_runbook.md`
> and `positioning.md` entirely and misdescribed the performance reports as
> git-ignored.)_

## For application builders

- [engine_access_guide.md](engine_access_guide.md) — **the Application
  Builder's Guide** (Milestone 18 + item 30): one task-oriented document for
  building an app *on* the engine — connect (embed/attach/server) → query (the
  SQL surface + the honest not-supported list) → bind `$n` params → introspect
  via the `information_schema.*` / `unidb_catalog.*` system catalog (grant-
  filtered per caller since item 111) → map types → page → handle errors, plus
  a "schema explorer in 30 lines" recipe. Start here to build around unidb
  "like Postgres." (Application-domain walkthroughs live with the application,
  e.g. `unidb-studio`, not in the engine docs.)
- [sql/sql_reference.md](sql/sql_reference.md) — **the SQL command reference**:
  the supported SQL surface, one command per anchored section, each with
  syntax and a runnable example — DDL, DML, queries, vector
  (`NEAR`/`VECTOR(n)`), full-text (`MATCH`), the Cypher graph subset, and
  security/RLS (`GRANT`, `CREATE POLICY`, `current_user`). Notes the correct
  entry point per family (`execute_sql` vs `execute_sql_as` vs
  `execute_cypher`) and a compatibility-at-a-glance table. Representative
  examples are executed against the engine by
  `examples/verify_sql_reference.rs`; open gaps are tracked in
  [`backlog/19_sql_surface_gaps.md`](backlog/19_sql_surface_gaps.md).
- [REST_API.md](REST_API.md) — full HTTP route reference for the optional
  `unidb-server` binary (M5): every route's payload and response shape, auth
  model, error codes, and known limitations. Also documents the Rust attach
  client (M8).

## For operators

- [ops_runbook.md](ops_runbook.md) — **the operations runbook**: data
  directory layout, WAL/checkpoint/log configuration env vars, users/roles,
  backups and PITR (LSN- and time-based), vacuum/bloat management, log
  rotation and retention, and metrics to watch.
- [performance/](performance/) — the **committed measurement record**: dated
  benchmark reports written by `scripts/report.sh` (CRUD decompose vs
  Postgres, the multi-model W0→W4 ladder, concurrency matrices) plus durable
  tuning references. See [`performance/README.md`](performance/README.md) for
  the file families and **which report is the current authoritative
  baseline**. The retired FFS/ffsdb head-to-head (`fssdb/`, removed
  2026-07-12) lives in git history; its conclusions fed the M6–M8 milestone
  set recorded in `PROGRESS.md`.

## For engine engineers

- [design/](design/) — consolidated architecture/design reference, indexed by
  [`design/design_index.md`](design/design_index.md):
  - [`design/engine_design.md`](design/engine_design.md) — the single
    consolidated engine-design document, distilling
    `CLAUDE.md`/`MEMORY.md`/`PROGRESS.md`.
  - [`design/processing-engines/`](design/processing-engines/00_engines_index.md)
    — the detailed per-engine reference collection (12 documents: storage,
    WAL/recovery, transactions, SQL, indexing, vector, graph, events,
    parallelism, server/replication, roadmap), written from the shipped code;
    carries an engine-state stamp noting what it incorporates.
  - [`design/how_unidb_stores_data.md`](design/how_unidb_stores_data.md) — a
    byte-level, diagram-heavy walkthrough of one order moving through the real
    engine (schema, WAL, buffer pool, MVCC, vector search, background
    workers), making the explicit case for why one engine beats a
    stitched-together multi-system stack.
  - Two shareable PDF artifacts (regenerated from adjacent `.html` sources via
    `render_pdf.mjs`): `unidb_design_architecture.pdf` (engineer-facing, keeps
    internal `D1`–`D13`/item codes) and `unidb_engine_architecture.pdf`
    (end-user product guide, no internal codes).
- [backlog/](backlog/) — the work ledger:
  - [`backlog/backlog_index.md`](backlog/backlog_index.md) — the **single
    at-a-glance registry** of every numbered effort (pending vs completed) and
    the live ranked **Next up** list.
  - [`backlog/roadmap.md`](backlog/roadmap.md) — the durable forward plan:
    positioning, the honest gap-to-a-real-database inventory, the 6-phase
    ACID-first scaling plan, the lane model, and the early decision log
    (per-item decisions now live in the item files + `PROGRESS.md`).
  - [`backlog/CONVENTIONS.md`](backlog/CONVENTIONS.md) — naming/lifecycle
    rules for backlog files.
- [history/](history/) — verbatim archives of older `MEMORY.md` /
  `PROGRESS.md` entries (rolled out 2026-07-22 to keep the per-session working
  set small; policy in `CLAUDE.md` §0.4). Headings are preserved exactly, so
  any `see PROGRESS.md "…"` reference resolves here by grep;
  `scripts/lint_docs.sh` enforces that. Grep these — never read them linearly.

## Positioning

- [positioning.md](positioning.md) — the honest competitive positioning: what
  unidb is (one engine, four models, one commit), what it deliberately is not,
  and where it stands against the replaced multi-system stack.
