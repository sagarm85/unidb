**Type:** Performance
**Status:** ⏳ NOT STARTED (prototype measured 2026-07-19)

# Item 86 — CRC at the storage boundary

## Problem

The page layer recomputes the full-page CRC32 far more often than the on-disk
guarantee (D9) requires:

1. `SlottedPage::compute_crc` **clones the entire 8 KiB page**
   (`self.data.clone()`) before hashing — a heap allocation + 8 KiB memcpy per
   call ([page.rs](../../src/page.rs) `compute_crc`).
2. `insert_versioned` and `set_lsn` each call `write_crc()` per mutation, so a
   batched Phase-B fill page pays ~1 full-page clone+hash **per row** where one
   (at write-out) would do. Item 64 Fix A already removed exactly this from
   `set_xmax` with the documented argument "every call site follows with
   `set_lsn() → write_page()`"; the same argument applies to `insert_versioned`.
3. `SlottedPage::from_bytes` verifies the CRC on **every** `fetch_page`, even
   buffer-pool **hits** — pages that were verified when they entered the pool
   and have only been mutated by this process since.

Measured (native profile of `main` @ items 75–84, `sample`, Table-3 mirror
workload `examples/profile_bulk_dml.rs`): `insert_versioned → compute_crc` =
**53% of `exec_update` samples**. Historical: CRC-per-mutation was 87.5% of
DELETE cost before item 64 Fix A (see `64_delete_lazy_xmax.md`).

## Fix (proven design — PG/SQLite/DuckDB all do this)

Checksums live at the **storage boundary** only:

- **Verify once** when a page enters the buffer pool (first fetch from mmap
  into a frame); skip re-verification on frame-resident hits.
- **Compute once** when a page leaves — flush, eviction, checkpoint, FPI.
- Remove `write_crc()` from `insert_versioned` (same pattern as item 64 Fix A;
  `set_lsn` keeps ownership of CRC on the write path until the full boundary
  discipline lands).
- Make `compute_crc` allocation-free: hash `data[..CRC_OFF]`, 4 zero bytes,
  `data[CRC_OFF+4..]` incrementally instead of cloning 8 KiB.

Step 1 (the two `page.rs` changes) was prototyped 2026-07-19: UPDATE HOT 482k →
**607k rec/s native (+26%)**, 37 page/heap/recovery unit tests green. Step 2
(verify-once-on-entry) is the second half of the win and needs the frame-
residency check in `fetch_page`.

## Expected gain

- UPDATE HOT: 0.62× → ~0.75–0.78× PG (step 1 measured; step 2 additional).
- Secondary gains on DELETE selected, SELECT filtered, INSERT, and HNSW NEAR
  (all paths fetch through the same pool).

## Risks / invariants

- **D9 unchanged**: every page on disk still carries a valid CRC; recovery
  reads still verify. The in-memory recompute never protected against
  corruption (it re-hashes whatever is in memory); protection level is
  identical to Postgres `data_checksums=on`.
- FPI path (`maybe_log_fpi`) must log a page image with a valid CRC or a
  payload that recovery re-checksums on apply — verify before shipping.
- Crash harness: full 50+ suite must pass; add a case that kills between an
  in-memory mutation and flush to prove recovery never trusts a stale
  in-memory CRC.

## Acceptance criteria

- Docker Table 3: UPDATE HOT ≥ 0.72× PG; no regression on any other row.
- Crash harness green (incl. new boundary case); `clippy -D warnings` clean.
