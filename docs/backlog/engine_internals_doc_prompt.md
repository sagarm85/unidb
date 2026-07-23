# Engine-internals doc generation prompt (meta doc — tooling)

> _Housekeeping note (2026-07-22): this file was committed **empty** on
> 2026-07-09 (commit `4ced2aa`) and stayed a 0-byte placeholder while being
> cited by `backlog_index.md` and `CONVENTIONS.md` as a real meta doc. The
> original prompt text was never committed. This note replaces the empty
> placeholder so the reference is no longer misleading._

## What this was for

The generation prompt behind the
[`docs/design/processing-engines/`](../design/processing-engines/00_engines_index.md)
collection (12 documents, PR #42): a per-engine technical reference written
from the shipped code (storage, WAL/recovery, transactions, SQL, indexing,
vector, graph, event queue, parallelism, server/replication, roadmap).

## Regenerating or extending the collection

The original prompt is lost; the working recipe it encoded is recoverable from
the collection itself:

- **Source of truth is the code**, not prior docs — each document cites
  `src/…` modules and states "written from the shipped code; when this and
  `CLAUDE.md`/`PROGRESS.md` disagree, those win."
- One document per engine, numbered `NN_<engine>.md`, indexed in
  `00_engines_index.md` with an **engine-state stamp** (FORMAT_VERSION, crash
  harness point count, items incorporated). Update the stamp whenever a
  document is regenerated.
- Mermaid diagrams inline (GitHub renders them natively); no binary artifacts.
- When regenerating after a batch of shipped items, sweep `PROGRESS.md` and
  `docs/backlog/backlog_index.md` for the item range since the stamp, and fold
  only shipped work in.
