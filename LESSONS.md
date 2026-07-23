# LESSONS.md — standing operational rules

Durable operational lessons promoted from MEMORY.md/PROGRESS.md session logs
(sweep of 2026-07-22). CLAUDE.md §0.6 holds the top-level review lens; this
file holds the concrete standing rules that back it. Append new lessons at the
bottom of the matching section with the date learned.

Rules that already live in CLAUDE.md §0.6 (one bench process / `pkill` strays
first; trust absolutes + WAL-B/row over noisy single-run ÷PG ratios; never
skip unchanged-column index maintenance; gate optimizations by measured
conditions; escalate honestly with sign-off) are deliberately NOT repeated
here — this file holds the rules that back and extend them.

---

## Benchmarking & measurement

- **NEVER run test suites (or any other heavy job) concurrently with a bench — benches get exclusive machine time.**
  The item-107/109 session (2026-07-22) self-inflicted the item-108
  environment-drift effect twice by running full test suites alongside a
  running bench; the numbers were garbage until the machine was quiet.

- **Run `docker compose down -v` before every Docker bench rerun.**
  Leftover Postgres volume state from a prior run can hang a fresh compose
  bench indefinitely (2026-07-22 session hygiene log).

- **A cross-run ÷PG ratio delta is evidence only when the PG-absolute environment canary is quiet; otherwise judge by unidb absolutes + WAL-B/row.**
  Item 108 (2026-07-21): Postgres, code-identical between two runs, moved
  2.1–28× in its own absolutes (VM fsync ~30×); every apparent unidb ratio
  "regression" was PG gaining more from a healthy environment.
  `compare_bench.py` now prints "ENVIRONMENT CHANGED" at >25% median PG-absolute
  drift. Within-run ratios remain fair by construction.

- **Classify a suspected regression absolutes-first; if still ambiguous, re-run the old commit on today's environment before bisecting.**
  Item 108 was closed in one step with zero bisection (2026-07-21), and the
  2026-07-22 A/B (`51022be` on current environment) proved the old ratios
  were not reproducible by the code that produced them.

- **Before trusting a bench number that implicates a feature, confirm the feature is actually ACTIVE on the measured path — and make bench engines open the way production does.**
  Item 107 (2026-07-21/22): the async HNSW worker existed since item 67, but
  only `open_arc` spawned it — the server and the bench both silently took the
  synchronous fallback, so W4/W0 = 96× measured a path production was never
  meant to run. Same family as item 15's default-off parallel-scan toggle.

- **A bench section that can silently skip is a coverage hole; external baselines must fail fast, not hang.**
  Table 3 was gated on a reachable Postgres and had silently skipped for most
  of the project's history, hiding the item-50 `patch_many` infinite loop;
  item 49 (2026-07-16) added connect timeouts after an unreachable `PG_URL`
  stalled reports "indefinitely" (24 call sites, no timeout).

- **Size the bench harness's own resources like production, or the harness understates the engine.**
  Item 42 (2026-07-15): `decompose.rs` never sized its buffer pool, so 1M-row
  sweeps hit `BufferPoolFull` and reported 1,228 rec/s where the truth was
  15,905 (~13×). The same unsized-pool pathology had already burned the
  unidb-studio demo (2026-07-14).

- **Compare "after" numbers only against a baseline with identical knobs, and remember the five row-count knobs are independent.**
  Item 53's target was judged against a comparator made stale by an
  `MM_CRUD_ROWS` change between runs (2026-07-16); `MM_SIZES`, `MM_BULK_SIZES`,
  `MM_CRUD_ROWS`, `MM_FK_ORDERS`, `MM_TX_SWEEP` share no default and must all
  be scoped together (item 50, 2026-07-16). PG SELECT parallelism is capped
  (`max_parallel_workers_per_gather=2`) for cross-machine comparability
  (calibrated baseline, 2026-07-16).

- **Baseline carry-forward (`MM_BASELINE` stitching) is invalid for shared-layer changes; a full baseline is mandatory per major release.**
  Item 105 (2026-07-21): stitched tables carry a provenance stamp and are
  excluded from comparison, but any WAL/commit/pool/heap/format change
  invalidates every carried table.

- **Report the measured spread across paired runs, never a lucky single run; overlapping distributions = noise, not regression (or win).**
  Item 21's Table-C overhead check (2026-07-13) and the item-11 default-ON
  flip (+25% reported when prior art said +38%, same mechanism, different
  machine) both held this line; JSON logging "beating" text (item 22) was
  called noise, not a win.

- **Benchmark durability edges under matched AND expensive sync; never headline a cheap-fsync (Docker VM) run.**
  Item 17 (2026-07-11): the replaced-stack advantage is ~parity under Docker's
  buffered VM fsync but a stable 3.61× under native `F_FULLFSYNC` — the win is
  durability-cost-dependent. Docker ratios are fair; absolutes are not
  publishable — use native Linux for publishable numbers.

- **Attribute against the SHIPPING default mode — check which mode a knob is actually in before concluding anything.**
  Item 116 (2026-07-24): a probe flipped `set_deferred_sync(false)` believing
  it was "bench-identical" and measured THREE fsyncs per INSERT; the engine
  default (set at open) is commit-time fsync with exactly one — the legacy
  per-statement mode exists only for the crash harness. Half a night's lever
  design targeted a mode nothing ships with.

- **`cargo test` is fail-fast per test binary: a green-looking tail is NOT a green suite.**
  2026-07-24: `cargo test --release | tail` showed the last suites green and
  exit 0 (the pipe masks cargo's code) while the run had STOPPED at the 11th
  binary's failure; only `--no-fail-fast` swept all 72 binaries and surfaced
  two more pre-existing failures. Sweeps must use `--no-fail-fast` and count
  `test result:` lines against the expected binary count.

## Process & planning

- **Run Step-0 (audit what already exists, then measure) before implementing any filed design — a backlog file's analysis is a hypothesis, not a work order.**
  Item 109 (2026-07-22): Step-0 refuted the filed parallel-resolution design
  (it had existed since items 45/54); the real lever was per-candidate page
  copy+CRC. Item 92 (2026-07-21): levers 1–3 turned out to be already merged.
  Item 107: the worker was already built, just never spawned.

- **Keep backlog Status headers and index rows in lockstep with reality; run `scripts/lint_backlog.sh` before any docs push.**
  Stale index rows (items 52/53/55/64 still "NOT STARTED" after shipping)
  caused bad ROI picks in the 2026-07-19 planning session; the 2026-07-22 docs
  audit found 23 status mismatches and built the lint as the guard.

- **Correct wrong claims in docs with dated inline correction notes, never silent rewrites.**
  The item-104 COUNT-baseline claim was corrected inline in PROGRESS.md
  (2026-07-21); M7's entry carries a correction block rather than rewritten
  history (2026-07-07). Same evidence ethos as §6, applied to every doc.

- **One PR per unit of work; after merging, verify main's content by `git ls-tree` (the actual tree), not by PR state.**
  2026-07-22 session close: stacking units on one branch hit the squash-merge
  orphan race; PR state alone misrepresented what main actually carried.

- **Register every feature-gated test file in `Cargo.toml` with `required-features` the moment it is created.**
  The PR-#28 lesson (2026-07-10), repeated by item 34's
  `server_observability.rs` (2026-07-16): an unregistered server test breaks
  the plain `cargo test` build while `--features server` CI stays green — the
  exact split that lets it merge broken.

- **Run race-prone tests repeatedly and in isolation — a green `--workspace` run can mask a deterministic race.**
  The M7 CSR self-visibility bug (2026-07-07) reproduced 30/30 via
  `cargo test -p unidb --test graph_mvcc` alone but was invisible under
  `--workspace` (feature unification changes test-binary composition/timing).
  Concurrency bugs also need CPU contention: item 16's failures only appeared
  under spinners / parallel test-binary load (2026-07-12).

- **Diagnose a hang with live evidence before theorizing: two `gdb -p <pid> -ex bt` samples; an identical stack at ~100% CPU is a spin, not a lock wait.**
  Item 50 (2026-07-16) was pinned to `patch_many` in minutes this way after a
  29-minute "hang" report.

- **Never assert `before == after` on a process-global counter in parallel tests; assert monotonic `after > before`, verify by behavior, or poll to quiescence.**
  The item-102/102-B counter flakes (2026-07-20) and item 107's queue-depth
  gauge test (2026-07-22) all landed on this rule; wall-clock comparisons
  inside a parallel run measure contention, not the optimization.

- **When you introduce a new usage pattern (a new sequence of existing ops), add an end-to-end continuity test for it — independently-correct pieces still compose into bugs.**
  The xid-reuse-after-checkpoint bug (2026-07-06) was silent
  data-corruption-class: no test had ever combined commit → checkpoint →
  reopen, because every reopen test used `flush()`. Test *state continuity*
  (counters, ids) across checkpoint+reopen, not just data survival.

- **Verify third-party crate capabilities against the vendored source before designing on them.**
  `instant-distance` had no incremental insert despite the plan assuming it
  (M2.b); `sqlparser` has a native `IndexType::BTree` (not `Custom`) and puts
  `USING` before the column list (M2.c/M6) — each caught by reading the
  vendored source, or breaking immediately when not.

## Engine-specific invariants

- **Vacuum's aliasing gate: scrub every reclaimed RowId from ALL secondary indexes BEFORE any slot becomes reusable.**
  M10.c (2026-07-08): stale index entries are harmless only while slots are
  never reused; after reuse a stale entry resolves to a live, MVCC-visible,
  semantically WRONG row. Every new index type must be wired into this gate.

- **`IndexStatus::Ready` on any async/debounced index means "backfill done", NOT "reflects every write" — never route correctness-critical reads through one.**
  The M7 CSR-traversal bug (2026-07-07) broke same-transaction edge
  self-visibility; M11 deliberately made UNIQUE enforcement a synchronous
  check for the same reason. Async indexes are read accelerators for
  slack-tolerant queries (NEAR) only, and the freshness contract must be
  explicit (item 107's queue-depth gauge, 2026-07-22).

- **Abort must physically undo while the xid is still in the active set; and every heap write on any new path must be paired with `record_undo`.**
  Item 16 (2026-07-12): removing the xid from `active` before undo made
  doomed versions look committed (visibility has no "aborted" state) —
  producing persistent duplicate/missing rows. A missed `record_undo` on a
  new write path (M4.a) makes aborted data durably visible with no error
  anywhere near the bug.

- **In batch write paths, complete ALL conflict detection before any committed mini-txn work; orphaned committed work survives the abort.**
  Item 85 (2026-07-19): `hot_update_many` ran Phase B (committed new-version
  inserts) before Phase A (xmax conflict check); a conflict left permanent
  ghost rows and a livelock. Order must be detect → mutate → link.

- **Never let a bounds/skip heuristic gate the FIRST element of a loop group — guarantee index progress unconditionally.**
  Item 50 (2026-07-16): `patch_many`'s leaf-bounds check gated `j == i`, but
  a leaf's live entries need not span its structural key range, so the loop
  repeated the identical lookup forever.

- **Duplicate-key runs straddle leaf boundaries: reads must descend to the LEFTMOST candidate leaf and walk next-links past the target.**
  The P3.c spike (2026-07-08) found `search_eq`/`remove` under-returning
  mid-run — silently incomplete results for any hot token/hub/value across
  P3.a/P3.b too.

- **HOT eligibility must consider every place a column's value is materialized — any column whose value is duplicated into an index leaf disables HOT when SET.**
  Item 102-B (2026-07-20): SET on an INCLUDE column taking the HOT path
  (which skips B-tree maintenance) left stale include bytes in the leaf.

- **A new WAL record type REQUIRES a FORMAT_VERSION bump — recovery's `_ => {}` arm makes old builds silently misrecover, not fail.**
  Item 56 Step 3 (2026-07-17): the bump ensures pre-bump builds get
  `BadVersion` instead of skipping `WAL_XMAX_BATCH` records. (Additive row
  *encoding tags* are fine without a bump; unknown *WAL kinds* are not.)

- **When deduping durability work, remove the whole coupled unit — a persist without its paired fsync (or vice versa) breaks replication invariants.**
  Item 104 (2026-07-20): dropping the catalog fsync while keeping
  `persist_only()` flipped `catalog_root` ahead of the shipped WAL stream →
  replica `SlotOutOfRange`. The control file must never reference state the
  durable/shipped WAL doesn't carry.

- **unidb is mmap-as-storage: buffer-pool frames are pin/dirty metadata (~24 B/frame), not a page cache — and the frame table allocates eagerly at open.**
  Postgres `shared_buffers` sizing intuitions are wrong here in both
  directions (2026-07-14 default bump): a "big" pool is nearly free in RAM,
  but a full pool of not-yet-durable dirty pages forces a synchronous
  `wal.sync()` per write — collapse indistinguishable from a regression.

- **In multi-phase crash tests, beware the LSN-restart collision: a Phase-1 flush writes high page LSNs, and a truncated/fresh WAL restarting at LSN=1 makes the redo gate (`page.lsn >= r.lsn`) skip Phase-2 records.**
  Hit while writing P56a/P56b (2026-07-17); the tests were restructured into
  a single session to avoid it.

## Tooling & environment

- **The sync-invariant check is `cargo tree -p unidb --no-default-features --edges normal` — nothing less.**
  Plain `cargo tree` from the workspace root shows the whole workspace's
  dependency union including dev-deps (tokio/axum appear legitimately), which
  has twice been mistaken for a broken "engine stays sync" claim
  (M5.d 2026-07-07, workspace note 2026-07-13).

- **Before any bench or long run, kill orphaned report/bench/server processes from prior sessions — criterion children outlive their parent shell.**
  2026-07-15: an orphaned 2.5-hour report run competed with a fresh one; the
  Phase-A session (2026-07-10) had 2–3 stray `decompose` processes
  contaminating "regression" runs. (The one-bench-process/pkill rule itself is
  CLAUDE.md §0.6.4; the standing habit is: check `ps` FIRST, every time.)

- **A per-item Docker profile that "should take 1.5 h" but takes 4 h means the knobs aren't reaching the container — verify env plumbing through docker-compose.yml, not just the script.**
  Item 105 (2026-07-21): `MM_TABLES`/`MM_SKIP_*` were never threaded through
  compose, so selective profiles silently ran the full bench.
