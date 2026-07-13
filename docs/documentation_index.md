# unidb Documentation

- [design/](design/) — consolidated architecture/design reference
  (`engine_design.md`), distilling `CLAUDE.md`/`MEMORY.md`/`PROGRESS.md`
  into one readable document. Kept current milestone-by-milestone; when it
  disagrees with `CLAUDE.md`/`PROGRESS.md`, those win. Also holds
  `unidb_engine_architecture.pdf` (added 2026-07-13) — the shareable PDF
  architecture reference with diagrams, flows, the measured performance
  ledger, and the Postgres/Supabase-alignment future-scope plan; regenerate
  it from the adjacent `.html` source (see `design/design_index.md`).
- [engine_access_guide.md](engine_access_guide.md) — **the Application
  Builder's Guide** (Milestone 18 + item 30): one task-oriented document for
  building an app *on* the engine — connect (embed/attach/server) → query (the
  SQL surface + the honest not-supported list, including `LIKE`/`ILIKE` and
  `MATCH` added in item 30) → bind `$n` params → introspect via the
  `information_schema.*` / `unidb_catalog.*` system catalog → map types → page →
  handle errors, plus a "schema explorer in 30 lines" recipe. Start here to
  build around unidb "like Postgres." (Application-domain walkthroughs live with
  the application, e.g. `unidb-studio`, not in the engine docs.)
- [REST_API.md](REST_API.md) — full HTTP route reference for the optional
  `unidb-server` binary (M5): every route's payload and response shape,
  auth model, error codes, and known limitations. Also documents the
  Rust attach client (M8).
- [performance/](performance/) — benchmark evaluations against external
  systems (plus the git-ignored dated reports `scripts/report.sh` writes).
  The retired FFS/ffsdb head-to-head comparison (`fssdb/`, removed
  2026-07-12) lives in git history; its conclusions fed the M6–M8 milestone
  set recorded in `PROGRESS.md`.
- [backlog/](backlog/) — [`roadmap.md`](backlog/roadmap.md) is the **single
  forward plan**: positioning, the honest gap-to-a-real-database inventory,
  the 6-phase ACID-first scaling plan, the parallel-worktree lane model
  (Core/SQL/Ops), how to start now, and the decision log. Shipped
  per-milestone plans have been retired here — their record lives in
  `PROGRESS.md`. This is a durable, git-tracked reference — unlike Claude
  Code's own ephemeral plan-mode file, which gets overwritten by the next plan.

For design decisions, milestone status, and architecture, see the
project-root `CLAUDE.md`, `PROGRESS.md`, and `MEMORY.md`.
