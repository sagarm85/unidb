# Buffer-pool sizing: the full investigation, the fix, and the config picture

> Durable reference doc, not a timestamped snapshot — update it in place as the
> tiers/values below change. For raw per-run report data, see the timestamped
> `multi_model_report_*.md` / `conc_matrix_*.md` files in this directory. For
> the mechanism itself (what a "frame" is, `BufferPool::open`,
> `find_victim`), see `docs/design/engine_design.md` §3.4.

## TL;DR

The buffer pool is **pin/dirty-tracking metadata over an mmap'd page file**
(~24 bytes/frame), not a Postgres-`shared_buffers`-style page-data cache — page
bytes already live in the OS page cache regardless of pool size. But the frame
table *is* allocated **eagerly** at `Engine::open()`, so `capacity` is a fixed
cost paid on every open, not a "how much RAM might get used" knob. That
tension is why the codebase has **three different tiers** below instead of one
number, and why none of them can simply be "raised to be safe."

| Tier | Consumer | Value | Working-set ceiling | Cost per open |
|---|---|---:|---:|---:|
| **Light** | Embedded/CLI/tests (~50 workspace test files, any small app) | compiled default, `DEFAULT_POOL_CAPACITY` = 65,536 | 512 MiB | ~35 µs |
| **Heavy** (demo/prod bulk-load) | `unidb-studio` server (`DEMO_GUIDE.md`) | `UNIDB_BUFFER_POOL_PAGES=1,000,000` | ~7.6 GiB | ~530 µs (once, at server startup) |
| **Heaviest** (internal bench tooling) | `benches/decompose.rs` (`bench_engine_open()`) | `2,000,000` (overridable via the same env var) | ~15.3 GiB | ~1 ms (once per bench engine open) |

The real fix that collapses these three tiers into one is **item 37**
(lazy/growable frame allocation — spec filed, `docs/backlog/37_lazy_buffer_pool_growth.md`,
**NOT STARTED**). Until then, each tier opts in explicitly for its own reason.

---

## How this was found (chronological)

### 1. The demo (`unidb-studio`) — where the pathology first surfaced

Seeding the `unidb-studio` demo past ~30k rows/table collapsed throughput to
~1-2k rows/s on a *correct, current* build (items 35/36/40 already shipped).
Root-caused to `fetch_page_for_write` (`src/lib.rs`): once the buffer pool has
no free/evictable frame, it forces a **synchronous `wal.sync()`** on every
subsequent write (`BufferPoolFull`), independent of the normal size-based
checkpoint trigger. At the *default* capacity of the time (4,096 frames /
32 MiB), a single `customers` table exceeded the pool before 30k rows.

**Fixed in two places**, in order:
1. `unidb-studio/demo/DEMO_GUIDE.md` — set `UNIDB_BUFFER_POOL_PAGES=1,000,000`
   explicitly for the demo server (proven at the `--size 5M`/`10M` presets:
   0 evictions, 250-586 MiB total process RSS, `customers` flat at
   ~23-25k rows/s vs ~1-2k/s before).
2. The engine's own compiled default: `DEFAULT_POOL_CAPACITY` raised
   `4,096 -> 65,536` frames (32 MiB -> 512 MiB) — a modest, measured bump
   (matches the project's own `256 -> 4,096` precedent from P1.c), **not**
   jumped straight to a large number, because the frame table is allocated
   *eagerly*: measured 2.9 µs/open @ 4,096 frames vs ~35 µs/open @ 65,536 vs
   530 µs/open @ 1,000,000 — a big default would tax every `Engine::open()`
   in the codebase, including the ~50 workspace test files, for a case only
   large-scale consumers need.

### 2. The bench harness (`benches/decompose.rs`) — the identical bug, in the project's own measurement tooling

While generating a full-scale multi-model report to verify item 39 (PK/FK
relational-integrity stress, Table 5), the *same* pathology turned up a second
time: every one of the 18 `Engine::open()` call sites in `decompose.rs` used
the plain library default, so **any report sweeping into 1M+ rows was
silently understating unidb's real performance** — measured **1,228 rec/s**
at Table 3.1's 1,000,000-row bulk-insert point, indistinguishable from a real
regression, when items 35/36/40 should deliver 15,000+ rec/s. This is more
consequential than item 39 alone: any *past* report at large `MM_SIZES`/
`MM_BULK_SIZES` may have understated the engine's real throughput too.

**Fixed** (item 42, `docs/backlog/42_bench_harness_buffer_pool.md`): a new
`bench_engine_open()` helper routes every bench engine through
`Engine::open_with_pool_capacity` at 2,000,000 frames instead of the library
default — scoped to the bench, not the engine's compiled default, for the
same eager-allocation reason as above.

**Measured before/after** (smoke-tested at the exact scale that exposed it):

| Workload | Before | After | Recovery |
|---|---:|---:|---:|
| Table 3.1 bulk insert, 1,000,000 rows | 1,228 rec/s | **15,905 rec/s** | **~13×** |
| Table 3.1 bulk insert, 10,000 rows (reference, unaffected) | 17,991 rec/s | — | flat, consistent |

The fixed number is flat and consistent with the unaffected reference point —
the scale-dependent collapse is gone, not just improved.

---

## Item 39 — what the fixed bench actually measures (Table 5)

New Table 5 in `scripts/multi_model_report.sh`: `customers (id PRIMARY KEY,
name)` / `orders (id PRIMARY KEY, customer_id REFERENCES customers(id),
amount, status)`, identical on both engines. Made fair by item 36 (FK
row-level enforcement) — before that, unidb only checked the referenced
*table* existed, not the referenced *row*, so this comparison would have been
apples-to-oranges. Every table in this bench before item 39 had either no
`PRIMARY KEY` at all or a PK with zero `FOREIGN KEY` constraints.

**Real numbers** (small-sweep run for turnaround, `MM_FK_ORDERS=1,000` — see
`multi_model_report_20260715_091035.md` in this directory for the full report
including Tables 1-4):

| operation | unidb (rec/s) | postgres (rec/s) | remark |
|---|---:|---:|---|
| INSERT valid FK (real check every row) | 283 | 274 | **unidb** +3% |
| UPDATE bulk (re-checks FK path) | 13,806 | 69,080 | **postgres** +400% |
| SELECT JOIN orders/customers | 64,340 | 185,917 | **postgres** +189% |

**Correctness proofs** (pass/fail, not speed — a future regression in either
engine's FK enforcement shows up as a flipped checkmark):

- INSERT referencing a non-existent customer: unidb **rejected** ✓, Postgres **rejected** ✓
- DELETE of a still-referenced customer: unidb **blocked (RESTRICT)** ✓, Postgres **blocked (RESTRICT)** ✓

Honest reporting, not cherry-picked: unidb wins the per-row-commit INSERT path
(what items 35/36's index-backed checks were built for); Postgres wins bulk
UPDATE and JOIN, expected given its decades of query-planner/parallel-execution
maturity that this project isn't claiming to match (`CLAUDE.md` §1).

---

## Confirmed at larger scale — 10k/20k sweep (2026-07-15)

The numbers above (`MM_FK_ORDERS=1,000`, `MM_SIZES=100,1000`) were a
turnaround-optimized small sweep. A second, more representative full-report
run at `MM_SIZES=10000,20000` / `MM_BULK_SIZES=10000,20000` /
`MM_TX_SWEEP=10000,20000` / `MM_CRUD_ROWS=20000` / `MM_FK_ORDERS=20000`
(`multi_model_report_20260715_092725.md`, Peak RSS **99 MiB**) confirms the
fix holds at a meaningfully larger scale, not just the tiny smoke-test size:

| Table | Metric | Result | Verdict |
|---|---|---|---|
| 1 — commit ladder | `W4/W0` | 1.20× @ 10k rows -> 1.34× @ 20k | within the historical ~1.1-1.3× band, no pool-exhaustion spike |
| 3.1 — bulk insert | unidb rec/s | 15,039 @ 10k -> 15,723 @ 20k | **flat**, consistent with the 1M-row smoke test (15,905 rec/s) — no repeat of the mid-run dip seen in the interrupted full-scale attempt |
| 4 — atomic multi-model txn | unidb txns/s | 240 @ 10k txns -> 238 @ 20k | flat, fsync/HNSW-bound as expected, unaffected by the pool fix either way |
| Peak RSS | whole process | 99 MiB | scales with data touched, nowhere near the 2,000,000-frame (~15.3 GiB) ceiling — confirms the bookkeeping-vs-cache distinction in practice, not just in theory |

**One new, honestly-reported finding — not a regression, not something the
buffer-pool fix was meant to address:** Table 5's unidb-vs-Postgres gap on
`UPDATE bulk (re-checks FK path)` and `SELECT JOIN` **widens** as
`MM_FK_ORDERS` grows:

| Operation | @ 1,000 orders | @ 20,000 orders |
|---|---:|---:|
| UPDATE bulk (re-checks FK path) | postgres +400% | postgres **+1,041%** |
| SELECT JOIN orders/customers | postgres +189% | postgres +84% (JOIN improved relatively; UPDATE did not) |
| INSERT valid FK | unidb +3% | postgres +3% (essentially even at both scales) |

Postgres's bulk-UPDATE query-planner maturity pulling further ahead at scale
is expected and not evidence of anything broken — flagged here for anyone
who wants to scope a future optimization on unidb's bulk-UPDATE-with-FK-check
path, not as an action item this investigation took on.

---

## Operational notes

- **Setting `UNIDB_BUFFER_POOL_PAGES` too high "to be safe" has a real cost**:
  the frame table is eager-allocated, so it taxes every open, and a larger
  pool lets more dirty pages accumulate before a checkpoint — meaning a crash
  mid-load means a *longer* ARIES redo replay on next open. Pick the tier
  above that matches your actual workload, don't default to the largest one.
- **`scripts/multi_model_report.sh`** now sizes its own bench engines
  internally (2,000,000 frames) — you no longer need to set
  `UNIDB_BUFFER_POOL_PAGES` externally for correctness at the script's default
  sweep sizes; only raise it further for sweeps well beyond those.
- **Full-scale reports are slow "by design"** past this fix — Table 4
  especially (synchronous HNSW/graph index builds swept to millions of
  transactions). If you need fast turnaround rather than official-scale
  numbers, use small sweeps (`MM_SIZES`, `MM_BULK_SIZES`, `MM_TX_SWEEP`,
  `MM_CRUD_ROWS`, `MM_FK_ORDERS`, `MM_SAMPLE`) — still real numbers, just less
  comprehensive coverage. This is how `multi_model_report_20260715_091035.md`
  was generated.

## References

- `docs/backlog/42_bench_harness_buffer_pool.md` — the bench-harness fix spec.
- `docs/backlog/39_pk_fk_relational_stress_bench.md` — the Table 5 spec.
- `docs/backlog/37_lazy_buffer_pool_growth.md` — the long-term fix (NOT STARTED).
- `PROGRESS.md` — "Default buffer-pool capacity raised 4096 -> 65536 frames",
  "Item 42 — Bench harness buffer-pool fix", "Item 39 — PK/FK
  relational-integrity stress bench" — full before/after tables and gate
  results for each step.
- `docs/design/engine_design.md` §3.4 — the mechanism (`Frame`,
  `BufferPool::open`, `find_victim`, `BufferPoolFull`).
- `unidb-studio/demo/DEMO_GUIDE.md` — the demo-side fix and its own
  measured numbers (0 evictions, 250-586 MiB RSS at 1.5M-4M row seeds).
- `docs/performance/multi_model_report_20260715_091035.md` — the small-sweep
  full report (`MM_FK_ORDERS=1,000`).
- `docs/performance/multi_model_report_20260715_092725.md` — the 10k/20k-sweep
  full report confirming the fix at larger scale (Peak RSS 99 MiB).
