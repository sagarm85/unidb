# Design documents

Consolidated architecture/design documentation for unidb. For the rules
and locked decisions themselves see `CLAUDE.md`; for the running
implementation state see `MEMORY.md`; for per-milestone benchmark ledgers
see `PROGRESS.md`. Documents here *distill* those sources into a readable
architecture reference — when they disagree, `CLAUDE.md`/`PROGRESS.md`
win.

- [`unidb_engine_architecture.pdf`](unidb_engine_architecture.pdf) — **the
  shareable, end-user architecture & product guide (PDF)**: what unidb is, how
  data is stored and kept safe (incl. crash recovery), transactions &
  concurrency, the SQL layer (supported data types + query examples), search /
  indexing / graph, the event stream, a full REST API reference (endpoints,
  params, payloads, responses, error codes), operations & HA, measured
  performance vs Postgres, correctness/testing, known limitations, and roadmap.
  Written for **users**, not engine engineers — no internal milestone / phase /
  decision / item codes. First page is the title + a clickable table of
  contents (no cover); every page carries a footer page number. Generated from
  [`unidb_engine_architecture.html`](unidb_engine_architecture.html) — edit the
  HTML, then re-render:
  `node render_pdf.mjs unidb_engine_architecture.html unidb_engine_architecture.pdf`
  (headless Google Chrome over DevTools, for the page-number footer; see
  [`render_pdf.mjs`](render_pdf.mjs)). Provenance, source material, and coverage
  notes live in
  [`unidb_engine_architecture_context.md`](unidb_engine_architecture_context.md).
  A distilled snapshot: when it disagrees with `CLAUDE.md`/`PROGRESS.md`, those
  win.

- [`how_unidb_stores_data.md`](how_unidb_stores_data.md) — **"Inside unidb:
  One Order, Start to Finish"** — a byte-level walkthrough of one order moving
  through the real engine (schema → durable insert → buffer pool → versioned
  update → read → vector search → background workers), diagram-heavy, with
  file/line citations back to the actual source and an explicit "why this
  beats the alternative" case at every step. Written for **any** reader,
  technical or not — no prior database-internals knowledge assumed. Start
  here if you want to understand *why* unidb is built the way it is, not just
  what it does; it's the same ground as `processing-engines/` but as one
  linear worked example instead of organized by subsystem.

- [`processing-engines/`](processing-engines/00_engines_index.md) — **the detailed
  per-engine design collection** (added 2026-07-11): twelve documents covering
  every processing engine — storage core, WAL & recovery, MVCC/transactions,
  SQL, indexing, vector, graph, event queue, parallelism & performance
  (with the benchmark/metrics analysis), server/replication/ops, and a
  proposed future roadmap — each with architecture/flow diagrams, exact data
  structures, border cases, and measured numbers. Start at its
  [index](processing-engines/00_engines_index.md).
- [`storage_service.md`](storage_service.md) — **object storage service
  (`unidb-storage`, backlog item 23)**: the design note for the Supabase-Storage
  analog — S3 client choice (`aws-sdk-s3`, one wire impl for MinIO+S3), hybrid
  LOB/S3 tiering, the outbox + reconciler consistency model, and the dated
  correction recording the single-page catalog ceiling that shaped the schema.
- [`engine_design.md`](engine_design.md) — the engine as shipped through
  **M0–M8** (storage core, MVCC + SQL, vector/full-text indexes, graph +
  CSR, event queue, REST server, B-Tree index, Rust attach client).
  Includes a documented correction: M7's CSR graph index was originally
  wired into live traversal with a bug, found and fixed during M8's merge
  verification (§7.3).
