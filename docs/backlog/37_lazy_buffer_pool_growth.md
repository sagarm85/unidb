# Item 37 — Buffer pool frame table is eagerly allocated, not lazy/growable

**Type:** Improvement
**Status:** SHIPPED 2026-07-16 — see PROGRESS.md
**Priority:** Medium — not a correctness issue and not urgent (the default-bump
follow-up covers the immediate demo-scale pain), but it removes a real
tradeoff that a static default can never fully resolve.

---

## Problem

`BufferPool::open` allocates the entire frame table up front, unconditionally:

```rust
let frames = (0..capacity).map(|_| Frame::empty()).collect();
```

(`src/bufferpool.rs`, `BufferPool::open`). `capacity` is fixed for the
lifetime of the pool — set once at open (`DEFAULT_POOL_CAPACITY`, or
`UNIDB_BUFFER_POOL_PAGES`, or `Engine::open_with_pool_capacity`) and never
grown or shrunk afterward.

This forces a single static number to serve two conflicting goals:

- **Small/embedded use** (the common case — tiny CLI tools, the ~50 test files
  that call `Engine::open()`, most real embedded consumers) wants the default
  to be *cheap*: minimal allocation time and memory at open, since most opens
  never touch anywhere near the full capacity.
- **Large bulk-load use** (demo seeding, data migration, batch import) wants
  the default to be *generous*: enough frames that the pool never runs dry
  mid-load, since running dry forces a synchronous `wal.sync()` on every
  subsequent write (`BufferPoolFull` in `fetch_page_for_write`) — a severe,
  confusing throughput cliff that looks exactly like a correctness regression
  (see the item this follows up on: `PROGRESS.md`, "Default buffer-pool
  capacity raised 4096 -> 65536 frames").

No single fixed default serves both well. The 4096 -> 65536 bump moved the
wall from ~32 MiB to ~512 MiB of working set, which covers most demo/embedded
scenarios — but it is still a wall, and it was chosen by trading off measured
per-open cost (2.9 µs @ 4096 -> ~35 µs @ 65536 -> 530 µs @ 1,000,000) against
how big a default the *common* case should have to pay for on every open. A
multi-million-row bulk load can still exhaust it and hit the same pathology;
the only current answer is "know to set `UNIDB_BUFFER_POOL_PAGES` yourself,"
which is exactly the trap two separate `unidb-studio` demo debugging sessions
fell into before the cause was found.

## Root cause (why this matters, precisely)

The frame table is genuinely cheap **per frame** (`struct Frame { page_id:
Option<PageId>, pin_count: u32, dirty: bool, clock_ref: bool }`, ~24 bytes —
this is pin/dirty/clock-bit bookkeeping over an mmap'd page file, *not* a
page-data cache; the actual bytes already live in the OS page cache regardless
of pool size). So the cost of a large `capacity` is not "wasted RAM" the way a
Postgres `shared_buffers`-sized value would be — it's purely the **eager,
up-front allocation** of `capacity` frame-slots at open time, whether or not
they are ever used. A database that only ever touches 200 pages still pays for
allocating and zero-initializing however many frames `capacity` specifies.

If the frame table grew on demand instead — starting small and expanding as
distinct pages are actually touched, up to `capacity` as a ceiling rather than
a pre-allocation target — both goals above are satisfied simultaneously: tiny
opens stay cheap (a handful of frames get allocated, not the whole table), and
`capacity` can be set to a much larger ceiling by default without taxing the
common case at all, since the cost only materializes for workloads that
actually grow into it.

## Proposed scope (re-derive the exact mechanism per CLAUDE.md §0.6.2 before
implementing — this is a sketch of the shape, not a spec to implement as-is)

1. **Growable frame storage.** Replace the eagerly-sized `Vec<Frame>` with a
   structure that starts small (e.g. a fixed small initial allocation, or
   empty) and grows in chunks as new pages are pinned, up to `capacity` as a
   hard ceiling (not a pre-allocation size). Consider whether this needs a
   different data structure entirely (e.g. a chunked/segmented vector so
   growth doesn't require moving already-issued frame indices) or whether
   `Vec::reserve`-style amortized growth is sufficient given `frame_index:
   HashMap<PageId, usize>` already indirects lookups.
2. **Concurrency (P5.a).** The pool is `Send + Sync`, shared as `&BufferPool`/
   `Arc<BufferPool>` across many writer threads, with `PoolState` behind one
   `Mutex`. Growing the frame storage must not violate that — confirm growth
   happens under the same state-mutex critical section as `find_victim`/
   `fetch_page`, so no thread ever observes a torn or resized-mid-read table.
3. **D5 interaction.** Growth itself doesn't touch WAL/durability, but confirm
   `find_victim`'s eviction logic and the `BufferPoolFull` fallback
   (`fetch_page_for_write`'s `wal.sync()` retry) still make sense once
   "full" means "grown to the ceiling and nothing evictable," not "the
   original small table is full."
4. **A much larger default ceiling becomes viable.** Once allocation is lazy,
   revisit `DEFAULT_POOL_CAPACITY` itself — with no eager-allocation penalty,
   the ceiling can be set far higher (millions of frames) as the default,
   closing the demo-scale trap this item's predecessor only reduced, not
   eliminated.
5. **Crash recovery / reopen.** Confirm a freshly reopened pool (post-crash or
   normal restart) starts small again correctly, not at whatever size it had
   grown to in the previous session — growth state is runtime-only, never
   persisted.

## Correctness invariants the implementation MUST preserve

1. **No behavior change for existing capacity semantics** — `capacity` still
   means "hard ceiling," `UNIDB_BUFFER_POOL_PAGES` and
   `Engine::open_with_pool_capacity` still work identically from the caller's
   perspective; only the *allocation timing* changes.
2. **P5.a concurrency safety** — growth must be race-safe under concurrent
   pin/fetch from multiple writer threads; no lost frame, no double-issued
   frame index, no read of a torn/half-grown table.
3. **D5 (WAL-before-page)** — untouched by this change; growth must not
   introduce a path where a page is evicted/flushed ahead of its durable WAL
   LSN.
4. **Performance regression guard** — a small-scale open (few hundred pages)
   must stay at or near today's 4096-capacity open cost (~2.9 µs), not
   regress toward the eager-allocation cost of a large ceiling. This is the
   entire point of the change; measure it, don't assume it.
5. **Large-scale bulk-load proof** — re-run the `unidb-studio --size 5M`
   scenario (or an equivalent in-repo scale test) with a much larger default
   ceiling and confirm 0 forced-sync-on-`BufferPoolFull` events, matching or
   improving on the 65536-frame-default results this item follows up on.

## Acceptance criteria

- [ ] Small-scale `Engine::open()` cost (few hundred pages touched) stays
      within measurement noise of today's cost at the *current* default —
      i.e. lazy growth does not tax the common case, proven with a
      micro-benchmark matching the one used to justify the 4096 -> 65536
      bump (`PROGRESS.md`, "Default buffer-pool capacity raised" entry).
- [ ] A much larger `capacity` (proposed default or an explicit override) no
      longer costs proportional open-time or open-memory — proven by
      benchmarking open cost at, e.g., capacity = 1,000,000 and confirming it
      is close to the small-capacity cost, not the ~530 µs eager-allocation
      cost measured for today's implementation at that size.
- [ ] `benches/conc_matrix.rs` and the crash harness (`cargo test --test
      crash`) pass unchanged — no new races or recovery gaps introduced.
- [ ] A large bulk-load regression test (the `unidb-studio --size 5M`-shaped
      scenario, or an in-repo equivalent) shows 0 `BufferPoolFull`-forced
      syncs at whatever the new default ceiling is, without requiring
      `UNIDB_BUFFER_POOL_PAGES` to be set manually.
- [ ] `docs/design/engine_design.md` §3.4 and `src/lib.rs`'s
      `DEFAULT_POOL_CAPACITY` doc comment updated to describe the lazy-growth
      model and the (likely much larger) new default ceiling.
- [ ] No `FORMAT_VERSION` bump expected (runtime allocation strategy, not an
      on-disk format change) — confirm, don't assume, before shipping.

## Depends on / builds on

- The default-bump item this follows up on (`PROGRESS.md`, "Default
  buffer-pool capacity raised 4096 -> 65536 frames") — that item is the
  immediate, modest fix; this item is the proper architectural one that
  removes the tradeoff it could only partially resolve.
- `src/bufferpool.rs` — `BufferPool::open`, `PoolState`, `find_victim`,
  `fetch_page`/`fetch_page_for_write` — the machinery to modify.
- P5.a (buffer-pool concurrency) — the locking model any growth logic must
  respect.
