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
- [backlog/](backlog/) — saved plans for future work. Start with
  [`roadmap.md`](backlog/roadmap.md) — the consolidated future roadmap,
  positioning decision, parallel-worktree lane map (Core/SQL/Surface), and
  per-session decision log. Individual plans are marked `NOT STARTED`/
  `PAUSED` while pending and updated (or removed in favor of the
  `PROGRESS.md` entry) once the work ships — e.g.
  `phase2_sql_capability_expansion.md` (paused, not started) and
  `group_commit_and_read_concurrency.md` (group commit + read-only fsync
  skip + buffer-pool force-WAL-on-evict + concurrent reads — merged to
  `main` via PRs #2–#4). These are durable, git-tracked references — unlike
  Claude Code's own ephemeral plan-mode file, which gets overwritten by the
  next plan.

For design decisions, milestone status, and architecture, see the
project-root `CLAUDE.md`, `PROGRESS.md`, and `MEMORY.md`.
