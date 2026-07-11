# Design documents

Consolidated architecture/design documentation for unidb. For the rules
and locked decisions themselves see `CLAUDE.md`; for the running
implementation state see `MEMORY.md`; for per-milestone benchmark ledgers
see `PROGRESS.md`. Documents here *distill* those sources into a readable
architecture reference — when they disagree, `CLAUDE.md`/`PROGRESS.md`
win.

- [`processing-engines/`](processing-engines/00_engines_index.md) — **the detailed
  per-engine design collection** (added 2026-07-11): twelve documents covering
  every processing engine — storage core, WAL & recovery, MVCC/transactions,
  SQL, indexing, vector, graph, event queue, parallelism & performance
  (with the benchmark/metrics analysis), server/replication/ops, and a
  proposed future roadmap — each with architecture/flow diagrams, exact data
  structures, border cases, and measured numbers. Start at its
  [index](processing-engines/00_engines_index.md).
- [`engine_design.md`](engine_design.md) — the engine as shipped through
  **M0–M8** (storage core, MVCC + SQL, vector/full-text indexes, graph +
  CSR, event queue, REST server, B-Tree index, Rust attach client).
  Includes a documented correction: M7's CSR graph index was originally
  wired into live traversal with a bug, found and fixed during M8's merge
  verification (§7.3).
