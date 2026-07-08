# Phase 1 — ACID & storage foundation (Core lane, `acid-hardening`)

## Status as of 2026-07-08: IN PROGRESS — P1.a shipped; P1.b–P1.e next.

**P1.a (full-page-writes / `WAL_FPI`) is shipped** — see `PROGRESS.md`'s
"Phase 1 → P1.a" entry (crash point P11 added, `FORMAT_VERSION` 3→4, benchmark
recorded). P1.b (fsync-failure handling) is the next checkpoint. Remaining:
P1.b → P1.c → P1.d → P1.e, one PR each, in order.

The **feature-freeze gate**: no Phase-4+ engine feature lands until Tier-0
correctness is closed. Companion to [`roadmap.md`](roadmap.md) §4/§6 — this is
the detailed spec the `acid-hardening` Core-lane worktree executes. Serial lane
(only one Core worktree at a time); disjoint from the SQL (`sql-types`) lane.

## Context — why this is first

unidb's engine core is architecturally correct (ARIES WAL, MVCC, buffer pool),
but it has **invisible correctness holes** that pass every test and demo, then
silently lose or corrupt data under a crash at load. Scaling a system with a
data-loss hole just loses data faster — so this phase comes before all
scale/feature work. **Important:** every item here *completes or strengthens* a
locked decision (D1/D5/D9/D10–D12/D3) — none reverses one. The foundation is
right; we are finishing it.

## Scope

- **IN:** torn-page protection, fsync-failure handling, the `alloc_page`
  whole-file-remap fix + configurable buffer pool + real FSM, isolation
  correctness (RC re-evaluation + SSI), auto-checkpoint.
- **OUT (later phases):** durable indexes (Phase 3), concurrent writers
  (Phase 5), segmented WAL / replication (Phase 6). Auto-checkpoint here is the
  *trigger*; segmented WAL is Phase 6.

## Checkpoints

### P1.a — Full-page-writes (the #1 data-loss hole)

An 8 KiB page write is not atomic; a crash mid-write leaves a half-old/half-new
page. Today CRC *detects* it, then the page is unrecoverable. Fix (Postgres's
`full_page_writes`):

- On the **first modification of a page after each checkpoint**, log the entire
  8 KiB page image to the WAL (a new `WAL_FPI` record) *before/with* the first
  incremental change record.
- **Recovery redo:** on encountering a `WAL_FPI`, write the whole page image
  (overwriting any torn state) as the clean base, then replay subsequent
  incremental redo on top.
- **Buffer pool** tracks a per-frame "FPI-logged since last checkpoint" flag,
  set on first dirty-mark, **reset for all frames at checkpoint**.
- Files: `wal.rs` (`WAL_FPI` record + writer), `bufferpool.rs` (first-touch
  flag + emit-on-first-dirty), `recovery.rs` (redo handling), `checkpoint.rs`
  (reset flags). `FORMAT_VERSION` bump (new WAL record kind, D9).
- **New crash-injection point (D7):** write a *partial* page (simulate a torn
  8 KiB write), crash, reopen → assert the page is restored from `WAL_FPI` +
  redo and the committed row is intact.
- Cost/tradeoff: WAL grows (full pages on first touch) — bounded by checkpoint
  frequency, which is exactly why P1.e (auto-checkpoint) pairs with this. Record
  the WAL-size + write-throughput overhead in the benchmark.

### P1.b — fsync-failure handling (fsyncgate) + ordering

- On `fsync`/`msync` **failure**, do **not** advance `durable_wal_lsn` and do
  **not** mark pages clean; surface a fatal error (the OS may have dropped the
  dirty page — silently continuing corrupts data). Treat a failed data-file
  flush as unrecoverable for that session.
- Re-verify the D5 invariant end-to-end (WAL fsync strictly before page write)
  and add a debug assertion + a fault-injection test that forces an fsync error
  and asserts the engine refuses to report success.
- Files: `wal.rs`, `bufferpool.rs` (flush path), `mmap.rs` (`flush_range`).

### P1.c — `alloc_page` remap fix + configurable buffer pool + real FSM

- **Remap fix:** today `alloc_page` calls `set_len` + **re-maps the whole file
  per page** (`bufferpool.rs`) — O(inserts) full-file remaps, fatal at 100s of
  GB. Grow the file in **large chunks** (e.g. pre-extend by N MB) and remap only
  when crossing a chunk boundary, not per page.
- **Buffer pool:** make `POOL_CAPACITY` **configurable** (env/config), default
  much larger, with clock eviction proven under load — no `BufferPoolFull` at
  100k+ rows/table.
- **Real FSM:** replace the linear `free_space()` scan with a free-space map so
  page allocation for a given size is ~O(1)/O(log n), not O(pages).
- Files: `bufferpool.rs`, `mmap.rs`, `heap.rs` (FSM), `lib.rs` (config plumb).
- **Benchmark:** insert/point-read throughput must stay ~flat as the table grows
  to 100k → 1M rows (today it degrades then fails). This is the "vertical
  scaling" number.

### P1.d — Isolation correctness (RC re-evaluation + SSI)

- **RC EvalPlanQual:** at `READ COMMITTED`, a concurrent update currently
  **aborts** (`WriteConflict`) instead of re-reading. Implement re-read of the
  latest committed version and re-evaluate the predicate/update against it (the
  D12-deferred "RC re-evaluation path"). Removes spurious aborts.
- **SSI (true `SERIALIZABLE`):** wire the currently no-op `on_read`/`on_write`
  seam (`concurrency_hooks.rs`) to track rw-antidependencies and abort on
  dangerous structures — real serializability. **This is the hardest item in
  the phase**; scope it as its own sub-checkpoint and lean on the existing seam.
- Files: `mvcc.rs`, `txn.rs`, `lockmgr.rs`, `concurrency_hooks.rs`, `heap.rs`
  (update re-eval path), executor read path.
- **Tests:** a **write-skew** scenario (must commit under RR/SI, must abort
  under SERIALIZABLE); a concurrent-update-at-RC test proving no spurious abort.

### P1.e — Auto-checkpoint

- Trigger `checkpoint()` automatically on **both** a time interval
  (`checkpoint_timeout`) **and** a WAL-size threshold (`max_wal_size`) —
  today checkpoint is manual-only, so the WAL (and the P1.a FPI volume) grow
  unbounded. Spread/throttle checkpoint I/O so it doesn't spike latency.
- A lightweight monitor (timer thread, or a check on the writer thread) invokes
  the existing checkpoint path; config-tunable, sane defaults.
- Files: `checkpoint.rs`, `lib.rs`/`server/engine_handle.rs` (trigger), config.

## Locked decisions touched (all completed/strengthened, none reversed)

| Decision | Effect |
|---|---|
| D1 (redo+undo WAL) · D5 (WAL-before-page) | Strengthened — FPI makes redo torn-page-safe; fsync-failure path hardens D5 |
| D9 (format, CRC+LSN) | `FORMAT_VERSION` bump for the `WAL_FPI` record |
| D3 (control file / checkpoint) | Auto-checkpoint extends the existing path |
| D10 / D11 / D12 (isolation) | Completes the deferred RC re-eval + SSI seam as originally designed |
| D6 (single file) · D8 (8 KiB) | Unchanged |

## Verification gates (Phase 1 done =)

- Crash harness gains: torn-page-recover (P1.a), fsync-failure-refuses-success
  (P1.b); full P-series + property test green.
- Isolation: write-skew aborts at SERIALIZABLE; RC concurrent update no longer
  spuriously aborts.
- Scale: insert/read throughput ~flat to 1M rows/table (no `BufferPoolFull`).
- Benchmarks recorded (FPI WAL overhead, alloc_page-at-scale, checkpoint I/O);
  no regression on existing tests; `clippy -D warnings` + `fmt` clean.
- `PROGRESS.md` + `MEMORY.md` updated; PR per checkpoint with its numbers.

## Known limitations / deferred

- **Double-write buffer** (InnoDB's alternative to FPI) is not pursued — FPI is
  simpler and matches the WAL-centric design.
- SSI may ship in a reduced form first (detect-and-abort without full predicate
  locking); full predicate locks can be a follow-up.
- Torn-page protection assumes the *WAL* itself is written atomically per record
  (CRC-framed, stops at first bad record) — unchanged from today.
