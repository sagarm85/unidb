# unidb Documentation

- [design/](design/) — consolidated architecture/design reference
  (`engine_design.md`), distilling `CLAUDE.md`/`MEMORY.md`/`PROGRESS.md`
  into one readable document. Kept current milestone-by-milestone; when it
  disagrees with `CLAUDE.md`/`PROGRESS.md`, those win.
- [REST_API.md](REST_API.md) — full HTTP route reference for the optional
  `unidb-server` binary (M5): every route's payload and response shape,
  auth model, error codes, and known limitations. Also documents the
  Rust attach client (M8).
- [performance/](performance/) — benchmark evaluations against external
  systems. `fssdb/` holds the head-to-head comparison against the FFS
  database's published evals (<https://ffsdb.com/evals>), unidb's fresh
  `cargo bench` numbers, and a same-machine Postgres + pgvector run — with
  the architectural caveats that make the ratios meaningful (raw index
  primitives vs a durable transactional engine).
- [backlog/](backlog/) — [`roadmap.md`](backlog/roadmap.md) is the **single
  forward plan**: positioning, the honest gap-to-a-real-database inventory,
  the 6-phase ACID-first scaling plan, the parallel-worktree lane model
  (Core/SQL/Ops), how to start now, and the decision log. Shipped
  per-milestone plans have been retired here — their record lives in
  `PROGRESS.md`. This is a durable, git-tracked reference — unlike Claude
  Code's own ephemeral plan-mode file, which gets overwritten by the next plan.

For design decisions, milestone status, and architecture, see the
project-root `CLAUDE.md`, `PROGRESS.md`, and `MEMORY.md`.
