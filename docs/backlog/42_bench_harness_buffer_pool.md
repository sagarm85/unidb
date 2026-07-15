# Bench harness silently understates unidb's performance at scale (buffer pool)

**Type:** Improvement
**Status:** SHIPPED — see `PROGRESS.md` ("Bench harness buffer-pool fix") for
measured numbers.

## Problem

`benches/decompose.rs` opens every internal engine via plain `Engine::open(dir,
0)` — none of the 18 call sites in the file size the buffer pool. At large
scale (1,000,000+ rows, e.g. Table 3.1's bulk-insert-at-scale sweep), this hits
the exact `BufferPoolFull` pathology diagnosed for the `unidb-studio` demo
earlier the same day: the pool exhausts, and `fetch_page_for_write` forces a
synchronous `wal.sync()` on every subsequent write, independent of the normal
size-based checkpoint trigger.

Measured on this exact repo, this exact bench, at the 1,000,000-row point of
Table 3.1: **1,228 rec/s** — indistinguishable from a real correctness
regression, when items 35/36/40 (unique-index enforcement, FK row-level
enforcement, B-tree bulk-load) should deliver 15,000+ rec/s at that scale.

This means **the project's own official measurement tooling has been silently
understating unidb's real performance** at any report run that swept
`MM_SIZES`/`MM_BULK_SIZES`/`MM_CRUD_ROWS`/`MM_FK_ORDERS` into seven-figure row
counts — a more consequential finding than any single feature bench, since it
could have masked or falsely shown "regressions" in every large-scale report
generated before this fix.

## Root cause

Same class of bug as the `unidb-studio` demo fix, in a different consumer:
`DEFAULT_POOL_CAPACITY` (65,536 frames / 512 MiB, `src/lib.rs`) is a
deliberately modest, evidence-based default for the *common* case (tiny
embedded/CLI opens, the ~50 workspace test files) — the buffer pool's frame
table is allocated **eagerly** at `Engine::open()`
(`(0..capacity).map(|_| Frame::empty()).collect()`), so a large compiled-in
default would tax every open in the codebase for the benefit of a comparatively
rare large-scale case. A benchmark harness deliberately creating multi-million-
row tables *is* that rare case, and should opt in explicitly via
`Engine::open_with_pool_capacity` — but `decompose.rs` never did.

## Fix

A new `bench_engine_open()` helper (`benches/decompose.rs`, right after the
imports) routes every engine open through `Engine::open_with_pool_capacity`
with a 2,000,000-frame pool (~15.3 GiB working-set ceiling, ~48 MiB of actual
frame-table bookkeeping — not RAM proportional to the ceiling, same
bookkeeping-vs-cache distinction as the engine-default fix), overridable via
the same `UNIDB_BUFFER_POOL_PAGES` env var the engine itself and
`unidb-studio` already use. All 18 `Engine::open(dir, 0).unwrap()` call sites
replaced with `bench_engine_open(dir)` — a mechanical, verified substitution
(`Arc::new(...)` wrapping preserved everywhere it existed).

## Verification

Smoke-tested directly at the exact scale that exposed the bug (`MM_BULK_SIZES=
10000,1000000`, everything else minimized to isolate Table 3.1): the 1,000,000-
row point recovered from **1,228 rec/s to 15,905 rec/s** (~13×), consistent
with and flat against the 10,000-row point (17,991 rec/s) — no more
scale-dependent collapse.

- `cargo build --release --bench decompose` — clean.
- `cargo clippy --release --bench decompose -- -D warnings` — clean.
- `cargo fmt --all --check` — clean.
- Full-scale report (`scripts/multi_model_report.sh`, default sizes, `PG_URL`
  set) re-run with the fix in place — real Table 3.1/Table 5 numbers recorded
  in `PROGRESS.md`.

## The three-tier buffer-pool config picture (for future reference)

This fix completes a three-tier config story spread across the codebase, each
tier already justified by direct measurement this session:

| Tier | Consumer | Value | Ceiling | Open cost |
|---|---|---:|---:|---:|
| Light | Embedded/CLI/tests | compiled default (65,536) | 512 MiB | ~35µs |
| Heavy (demo/prod) | `unidb-studio` (`DEMO_GUIDE.md`) | `UNIDB_BUFFER_POOL_PAGES=1,000,000` | ~7.6 GiB | ~530µs (once, at server startup) |
| Heaviest (bench tooling) | `benches/decompose.rs` (`bench_engine_open`) | `2,000,000` | ~15.3 GiB | ~1ms (once per bench engine open) |

The real long-term fix that would collapse these three tiers into one is item
37 (lazy/growable frame allocation, filed, NOT STARTED) — once frame
allocation is lazy instead of eager, a much larger ceiling can be the single
default with no tax on small opens.

## Depends on / builds on

- The engine default-bump work (`PROGRESS.md`, "Default buffer-pool capacity
  raised 4096 -> 65536 frames") — same root mechanism, different consumer.
- `Engine::open_with_pool_capacity` (`src/lib.rs`) — the existing API this fix
  uses; no engine changes required.
- Item 37 (lazy/growable frame allocation) — the eventual fix that removes the
  need for this kind of per-consumer opt-in entirely.
