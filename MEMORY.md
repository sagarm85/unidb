# MEMORY.md

> **Read this FIRST every session. Update it LAST every session.**
> This is the running state of the implementation — what exists, what's in
> progress, what's next. Rules & locked decisions live in `CLAUDE.md`.
> Shipped-milestone records + metrics live in `PROGRESS.md`.
>
> When you update this file, stamp the log with the **actual current system
> date** — never copy a date from above.

---

## Current status

- **Milestone:** M0 — Storage core
- **State:** Implementation complete. All code compiles clippy-clean. 30 unit
  tests + 6 crash-harness tests all pass. Benchmarks not yet recorded.
- **Immediate next task:** Run `cargo bench` (release build) to fill in the M0
  benchmark table in `PROGRESS.md`, then close out M0.
- **Last updated:** 2026-07-06

---

## What exists now

All M0 source modules are implemented and passing tests:

```
src/
  format.rs      — magic, version, constants, endian helpers
  error.rs       — DbError + Result type (thiserror)
  control.rs     — control file: create/read/write, CRC, magic/version check (D3)
  mmap.rs        — ONLY unsafe module: PageFileMmap wrapper around memmap2
  page.rs        — slotted-page body, tuple header w/ reserved xmin/xmax (D4), CRC (D9)
  bufferpool.rs  — frames, pin/unpin, clock eviction, D5 enforced at flush/evict (D5)
  wal.rs         — append-only log, redo+undo payloads, LSN, mini-txn bracketing (D1/D2/D13)
  heap.rs        — insert/read/update/delete, in-place update for M0, linear-scan FSM
  checkpoint.rs  — flush dirty → checkpoint WAL record → update control → truncate WAL
  recovery.rs    — control → redo committed → undo incomplete mini-txns (D1, ARIES-style)
  lib.rs         — Engine API (open/insert/get/update/delete/checkpoint/flush), init_tracing()
tests/
  crash/main.rs  — 6 crash-injection tests covering P1–P5 (D7)
benches/
  load.rs        — INSERT / point-SELECT / UPDATE criterion benchmarks (not yet run)
```

Key design decisions confirmed in implementation:
- D5 enforced: checked at `flush_page()` and in `find_victim()` eviction path only
  (write_page is in-memory; checking there would block valid WAL-then-write ordering)
- WAL uses length-prefix framing (u32 LE) + per-record CRC32; scan stops at corruption
- `mmap.rs` is the sole `#![allow(unsafe_code)]` module; rest of crate uses `#![deny]`
- All page/WAL integers are little-endian (D9)
- Tuple header reserves 16 bytes (xmin+xmax) for MVCC forward-compat (D4)

---

## In progress

Nothing — awaiting bench run.

---

## M0 task breakdown (ordered — this is the plan of record)

1. ✅ **Scaffold.** Cargo project (edition 2021), module layout, deps, tracing init.
2. ✅ **On-disk format constants (`format.rs`).**
3. ✅ **Control file (`control.rs`) — D3.**
4. ✅ **Page + slotted body (`page.rs`) — D4, D9.**
5. ✅ **Buffer pool (`bufferpool.rs`) — D5.**
6. ✅ **WAL (`wal.rs`) — D1, D2, D13.**
7. ✅ **Heap access (`heap.rs`).**
8. ✅ **Checkpoint (`checkpoint.rs`).**
9. ✅ **Recovery (`recovery.rs`) — D1.**
10. ✅ **Crash-injection harness (`tests/crash/`) — D7.** All 6 injection points green.
11. ⏳ **Load test (`benches/`).** Criterion bench exists; benchmarks not yet executed.
    Run `cargo bench` (release build) and record results in `PROGRESS.md`.

**M0 done when:** durable single-table CRUD survives all crash-harness points ✅,
recovery is verified ✅, benchmark table is recorded ⏳, and no locked decision is
violated ✅.

---

## Open questions / pending human input

- None blocking M0 completion. Benchmarks are the only remaining step.
- Deferred-but-flagged for later milestones: slow-consumer-vs-vacuum durability
  contract (M4); filtered-HNSW vs over-fetch for RLS on `NEAR` (M2); SSI
  activation (post-M1, seam built in M1 per D11).

---

## Known issues / tech debt

- FSM is a linear scan over all heap pages — fine for M0, revisit if insert
  throughput regresses in M1.
- `Heap` stores page list in-memory only; after reopen, the Engine creates a new
  `Heap` with an empty page list. `fetch_page` still works because the buffer pool
  re-reads pages from the mmap, but `find_or_alloc_page` will always allocate a
  fresh page on the first insert after reopen. In M1 the catalog will fix this.
- WAL truncation rewrites the entire file — acceptable for M0, needs a proper
  log-segment scheme in later milestones.

---

## Session log (append newest at top; use the real current date)

### 2026-07-06 — M0 implementation (Tasks 1–10)

- Created all M0 source modules from scratch (Tasks 1–10).
- Fixed D5 enforcement: `write_page` is in-memory only (no D5 check); D5 is
  enforced at `flush_page()` and `find_victim()` eviction.
- Fixed `mmap.rs` `unsafe` isolation: crate uses `#![deny(unsafe_code)]`, mmap
  module uses `#![allow(unsafe_code)]`.
- Fixed WAL BufWriter flush ordering: tests that scan the WAL now commit (fsync)
  before scanning so records are durable on disk.
- **Final state:** `cargo clippy -- -D warnings` clean, 30 unit tests + 6 crash
  harness tests all green.
- **Next:** Run benchmarks (`cargo bench --release`), record results in
  `PROGRESS.md`, mark M0 done.

### 2026-07-06 — Project initialization
- Architecture design doc reviewed; six foundational gaps identified and resolved.
- Isolation decided: RC default / RR available / SSI seam now (D10–D12).
- Scope adjusted: single-file for M0 (D6); benchmark the replaced stack (§6).
- `CLAUDE.md`, `PROGRESS.md`, `MEMORY.md` created.
