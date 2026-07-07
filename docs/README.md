# unidb Documentation

- [design/](design/) — consolidated architecture/design reference
  (`engine_design.md`), distilling `CLAUDE.md`/`MEMORY.md`/`PROGRESS.md`
  into one readable document. Kept current milestone-by-milestone; when it
  disagrees with `CLAUDE.md`/`PROGRESS.md`, those win.
- [REST_API.md](REST_API.md) — full HTTP route reference for the optional
  `unidb-server` binary (M5): every route's payload and response shape,
  auth model, error codes, and known limitations. Also documents the
  Rust attach client (M8).
- [backlog/](backlog/) — saved plans for future work. Entries are marked
  `NOT STARTED`/`PAUSED` while pending and updated (or removed in favor of
  the `PROGRESS.md` entry) once the work ships — e.g.
  `phase2_sql_capability_expansion.md` (paused, not started). These are
  durable, git-tracked references — unlike Claude Code's own ephemeral
  plan-mode file, which gets overwritten by the next plan.

For design decisions, milestone status, and architecture, see the
project-root `CLAUDE.md`, `PROGRESS.md`, and `MEMORY.md`.
