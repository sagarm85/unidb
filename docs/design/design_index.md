# Design documents

Consolidated architecture/design documentation for unidb. For the rules
and locked decisions themselves see `CLAUDE.md`; for the running
implementation state see `MEMORY.md`; for per-milestone benchmark ledgers
see `PROGRESS.md`. Documents here *distill* those sources into a readable
architecture reference — when they disagree, `CLAUDE.md`/`PROGRESS.md`
win.

- [`unidb_engine_architecture.pdf`](unidb_engine_architecture.pdf) — **the
  shareable architecture reference (PDF, added 2026-07-13)**: full component
  breakdown with diagrams (layer stack, deployment/HA topology, page/tuple
  layout, write path + group commit, ARIES recovery, MVCC versioning, IVF-Flat
  `NEAR` path, the moat vs the replaced stack), how every subsystem works, the
  measured performance-improvements ledger, locked decisions D1–D13, the honest
  limitations registry, and a future-scope section aligning against Postgres
  (engine completeness, tiers P0–P3) and Supabase (platform surface) for
  production readiness. Generated from
  [`unidb_engine_architecture.html`](unidb_engine_architecture.html) — edit the
  HTML, then re-render with headless Chromium:
  `chromium --headless --no-pdf-header-footer
  --print-to-pdf=unidb_engine_architecture.pdf unidb_engine_architecture.html`.
  A distilled snapshot: when it disagrees with `CLAUDE.md`/`PROGRESS.md`, those
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
