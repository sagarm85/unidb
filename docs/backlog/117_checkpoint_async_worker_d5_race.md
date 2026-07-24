# 117 — Checkpoint vs async-HNSW-worker race: spurious D5 hard error fails user commits

**Type:** Improvement (correctness/availability)
**Status:** ⏳ NOT STARTED — filed 2026-07-24 from direct evidence (see below).
Bench-side mitigation (drain-before-checkpoint in `mm_ladder_point`) shipped
with the item 114 Step-0 probe; the engine-side race is UNFIXED.

## Evidence (2026-07-24, macOS, item114_step0 smoke run at 10k, W2, async worker)

```
thread 'main' panicked at benches/decompose.rs:1626:
called `Result::unwrap()` on an `Err` value:
Recovery("D5 violation on flush: page 689 LSN 86719 > durable WAL LSN 86718")
```

Line 1626 is the **pre-grow `engine.commit()`** — not an explicit checkpoint.
The failing checkpoint was an **auto-checkpoint fired inside a user commit**
(`maybe_auto_checkpoint`, `by_time` — the bench raises `max_wal_size` to
512 MiB but keeps the default time trigger). The off-by-one LSN (86719 vs
86718) is the smoking gun for a lost race, not corruption.

## Root cause

`checkpoint::run` snapshots the durable frontier ONCE:

1. `wal.sync()` → durable frontier = X
2. `pool.flush_all(X)` — every dirty page is checked against the **stale** X

The item 107 async HNSW worker appends WAL records and dirties index pages
**concurrently with step 2** (it runs mini-txns outside `txn_mgr`, so the
`active_count() > 0` guard in `maybe_auto_checkpoint` — which predates item
107 — does not see it). A page the worker dirties at LSN X+1 while
`flush_all` walks the dirty set hits the D5 enforcement in `flush_page`
(`bufferpool.rs:745`) and the whole checkpoint fails with a hard
`Recovery(...)` error, which propagates out of the user's `commit()`.

**Impact:** availability, not durability — the WAL is intact, control is not
updated, nothing is poisoned (`flush_poisoned` is not set on this path), and
a retry succeeds. But any deployment running the async worker (every
`open_arc`/server deployment) can have a user commit spuriously fail whenever
auto-checkpoint (time OR size trigger) races the worker. The 07-23 full
Docker bench simply never lost the race.

**This is NOT an item-107 regression — it is an item-107 revelation of a
latent pre-existing race.** Autovacuum has the same exposure (verified
2026-07-24, code inspection):
`Engine::vacuum` appends WAL and dirties pages under `write_serial`
(`lib.rs:4625`), but `Engine::checkpoint` (`lib.rs:4378`) never acquires
`write_serial` — it calls `checkpoint::run` directly. So vacuum's
append→dirty→self-sync window races `flush_all` identically; it has simply
never lost (vacuum self-syncs promptly, keeping the window narrow, where the
HNSW worker's drain backlog keeps it wide). There is NO existing coexistence
discipline for the worker to inherit — which is the strongest argument for
fix direction 1: one flusher-side fix covers every concurrent WAL writer,
present and future, instead of per-actor quiescing.

## Fix directions (decide with §0.6 review before implementing)

1. **Postgres-shaped (preferred):** before flushing a page whose LSN is ahead
   of the durable frontier, sync the WAL up to that LSN and re-check —
   `XLogFlush(pageLSN)` semantics. D5 stays a hard invariant; the flusher
   *satisfies* it instead of failing. Needs `flush_all`/`flush_page` to get a
   sync capability (closure param or `Arc<Wal>`), a deliberate layering
   change — the current `durable_wal_lsn: Lsn` parameter decoupling is
   intentional.
2. **Quiesce:** checkpoint drains/pauses the HNSW worker (and autovacuum)
   before `flush_all`. Simple, but couples checkpoint to every background
   writer and adds drain latency (up to the full backlog) to every
   checkpoint — including the in-commit auto-checkpoint path. Wrong shape.
3. **Retry loop:** catch the D5 error in `checkpoint::run`, re-sync, re-run
   `flush_all` (bounded). Works but re-walks the dirty set and leaves the
   hard error as control flow. Band-aid.

D5 (§3) is NOT re-litigated by any of these — the invariant "no page reaches
disk ahead of the durable WAL" is preserved by all three; only the *reaction*
of the flusher changes (make-durable-then-flush vs error).

## Acceptance

- A test that reproduces the race deterministically (checkpoint concurrent
  with a WAL-appending background writer; injection point or a tight loop)
  fails before / passes after.
- Crash harness stays 54/54; a new injection point mid-checkpoint-with-
  concurrent-worker is worth considering (D7 list already has "mid-
  checkpoint").
- Full bench ladder (which hit this in the wild) runs clean with the
  bench-side drain removed OR kept — the engine must be correct either way.
