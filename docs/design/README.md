# Design documents

Consolidated architecture/design documentation for unidb. For the rules
and locked decisions themselves see `CLAUDE.md`; for the running
implementation state see `MEMORY.md`; for per-milestone benchmark ledgers
see `PROGRESS.md`. Documents here *distill* those sources into a readable
architecture reference — when they disagree, `CLAUDE.md`/`PROGRESS.md`
win.

- [`engine_design.md`](engine_design.md) — the engine as shipped through
  **M0–M8** (storage core, MVCC + SQL, vector/full-text indexes, graph +
  CSR, event queue, REST server, B-Tree index, Rust attach client).
  Includes a documented correction: M7's CSR graph index was originally
  wired into live traversal with a bug, found and fixed during M8's merge
  verification (§7.3).
