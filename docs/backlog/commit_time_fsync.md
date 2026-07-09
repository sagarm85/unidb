# Commit-time WAL fsync — group-committed force-log-at-commit as the default

## Status as of 2026-07-09: DONE — shipped on branch `commit-time-fsync` (C1–C5 all landed). See `PROGRESS.md`'s "Commit-time WAL fsync" entry (before/after ladder table, C1 durability-claim audit, human sign-off). Group-committed force-log-at-commit is the default; crash harness 21 → 25; measured W4 ~33.1 → ~4.40 ms/commit (~7.5×), W0 at SQLite parity.

Evidence base: the **W0–W4 decomposition ladder** (`benches/decompose.rs`,
PR #21; full table in `PROGRESS.md`'s ladder entry). Measured on the ladder,
the default commit protocol pays ~10 F_FULLFSYNCs for the full multi-model
commit (row + vector + edge + event = 33.1 ms) where one suffices (4.3 ms) —
**7.7×** — and at one fsync per transaction the base engine is
SQLite-competitive at matched true durability (3.54 vs 3.58 ms/commit).

## Context — why the current default exists, and why it must change

- **M0 (D2):** the atomic unit was a single *statement* (no user transactions
  existed), so the statement's mini-txn fsync *was* the commit fsync. Correct.
- **M1:** user transactions were layered on top; statements stayed "fsynced
  immediately, unchanged from M0" (`txn.rs` header) — a conservative
  *inheritance*, not a re-decision. Once durability is promised at COMMIT,
  syncing uncommitted statements buys nothing: a crash before commit rolls the
  transaction back whether or not its statements were durable.
- **M9/P5:** `deferred_sync` + group commit (`Wal::sync_up_to`, leader-elected
  fsync off the append lock) shipped the *machinery*; the default was never
  flipped because deferred mode lacked a crash-harness proof.
- **Ladder (2026-07-09):** put the number on it — the multi-model write tax is
  ~97% fsync multiplication, ~3% actual work.

**Standards alignment:** ARIES defines durability as *force-log-at-commit* —
which is exactly this change; D1 already locked "steal + no-force,
ARIES-style," so this **completes D1's intent** rather than amending it.
Postgres (`XLogFlush` at commit), InnoDB (`flush_log_at_trx_commit=1`), and
SQLite-WAL (sync per commit txn) all ship this protocol. Per-statement sync
was the outlier.

**ACID position (explicit, since durability timing changes):**
- **A/C/I** — untouched: undo, constraints, MVCC/locks are orthogonal to fsync
  timing; a mid-txn crash rolls back identically under either policy.
- **D** — exactly satisfied: no commit acknowledged until `sync_up_to
  (commit_lsn)` returns. Durability is a *transaction*-granularity promise;
  per-statement sync exceeded the contract without strengthening any
  user-visible guarantee.
- **Not in scope, deliberately:** Postgres-style `synchronous_commit=off`
  (acknowledge before flush, bounded loss window) is a genuine D violation —
  never the default; at most a documented opt-in, later, separately.

## The protocol

1. **Inside a user transaction:** statement mini-txns append WAL records
   without syncing. `Engine::commit` → `sync_up_to(commit_lsn)` (group-
   committed) → acknowledge. **One fsync per transaction.**
2. **Auto-commit statements:** unchanged — the statement is the commit; it
   still syncs. No user-visible semantic change on any path.
3. **D5 under memory pressure:** dirty pages ahead of the durable WAL are
   currently *skipped* by eviction (verified in `bufferpool.rs`); with
   deferral they can accumulate until no victim exists. When eviction finds no
   evictable frame, **force a WAL sync, then evict** (Postgres's behavior).
4. **Replication guard:** `Wal::ship_from` (and therefore `Engine::ship_wal`)
   must be **capped at the durable LSN** — unsynced records are written to the
   file before they are durable, and shipping them lets a replica get *ahead*
   of a crashed primary (divergence on failover). `SlotKind::Sync` consumers
   ack durable LSNs only.
5. **The per-statement mode survives only as an internal flag** so the crash
   harness can exercise both policies; it is not a user knob.

## Scope

- **IN:** the default flip (scoped as above), the eviction-forced sync path,
  the shipping cap, the crash-harness proof, before/after benchmarks, docs.
- **OUT:** `synchronous_commit=off`-style ack-before-flush (D violation;
  separate opt-in decision later); a background WAL-writer thread (smooths
  eviction-forced syncs — noted as a follow-up optimization, not required for
  correctness); **async derivation** (PARKED per the ladder entry — its
  maximum prize is the +21% work share; re-trigger = re-run the ladder at
  large table sizes where IVF maintenance grows).

## Checkpoints

### C1 — Scoped deferral + durability-claim audit
- Make deferred statement sync the default for mini-txns issued *inside* an
  open user transaction; `Engine::commit`'s `sync_up_to` remains the single
  durable point. Keep today's global flag internal for tests.
- **Audit every `commit_mini_txn` call site not followed by a commit-path
  `sync_up_to`** and confirm each syncs before anything user-visible claims
  durability: checkpoint (control-file update + WAL truncation), `slots.json`
  persistence, backup/PITR paths, vacuum, index backfill, DDL. Each either
  runs under a user txn (covered) or must issue its own sync (document which).
- Files: `wal.rs`, `lib.rs` (commit/abort paths), `checkpoint.rs`.

### C2 — D5 eviction-forced sync
- Eviction that finds no victim (all dirty frames lead the durable WAL) forces
  `wal.sync()` and retries instead of failing with `BufferPoolFull`.
- Test: small pool + one large transaction touching more pages than the pool
  holds → completes correctly, no error, D5 invariant assertions stay green.
- Files: `bufferpool.rs`.

### C3 — Replication durable-LSN cap
- `ship_from` returns records only up to the durable frontier; sync-slot
  acknowledgements likewise.
- Test: writer with unsynced tail + replica shipping loop → replica never
  applies past the primary's durable LSN; kill the primary pre-sync, restart,
  re-ship → no divergence (replica state is a prefix of the primary's).
- Files: `wal.rs`, `replication/mod.rs`.

### C4 — Crash-harness proof (the gate)
New crash points (harness currently 21):
- **(a)** crash mid-txn with N unsynced statements → reopen → zero trace of
  the transaction (valid-prefix property test extended to deferred mode).
- **(b)** txn A's unsynced statements made durable as a side effect of txn
  B's commit sync (one ordered log) → crash → A still cleanly undone, B
  survives.
- **(c)** torn record in the unsynced tail → CRC detects, replay stops
  cleanly at the last valid record (existing behavior, re-proven under the
  new default).
- **(d)** crash between eviction-forced sync and the page write (D5 ordering
  under the new path).

### C5 — Benchmarks, docs, closeout
- **Acceptance benchmark:** re-run `benches/decompose.rs` — expected: default
  W4 ≈ old one-fsync numbers (~4.3 ms/commit, ~7.7× vs old default), W0 at
  SQLite parity; re-run `benches/concurrent_writers.rs` (group commit now
  coalesces cheaper commits — scaling should hold or improve). Record the
  before/after table in `PROGRESS.md`.
- Docs: `docs/design/engine_design.md` durability section (state the
  force-log-at-commit protocol + the D5/eviction and shipping-cap rules),
  README status line, this file flipped to done.
- **Record explicit human sign-off in `PROGRESS.md`**: durability *timing*
  changes (no §3 decision is reversed — D1 fulfilled, D2 bracketing and D5
  untouched — but the §3 ethos applies to durability semantics regardless).

## Locked decisions touched

| Decision | Effect |
|---|---|
| D1 (steal + no-force, ARIES) | **Fulfilled** — ARIES' durability point *is* force-log-at-commit; per-statement sync was an over-fulfillment left over from M0 |
| D2 (statement mini-txn as redo/undo unit) | Unchanged — bracketing and replay semantics identical; only sync timing moves |
| D5 (WAL-before-page) | Unchanged and still enforced — gains the eviction-forced-sync path (C2) so it holds under deferral without starving the pool |

## Verification gates (done =)

- Full suite green: `cargo build --workspace`, `cargo test -p unidb`
  (+ `--features server`), clippy `-D warnings`, fmt, sync-invariant.
- **Crash harness grows 21 → ≥25 and stays green** (C4's four points).
- Decompose ladder re-run showing the ~7.7× W4 improvement at unchanged
  commit durability; `concurrent_writers` scaling intact.
- Replication divergence test (C3) green.
- `PROGRESS.md` before/after table + sign-off recorded; `MEMORY.md` updated.

## Known limitations / deferred

- Eviction-forced syncs cluster latency under memory pressure (the cost moved,
  not created); a background WAL-writer thread is the follow-up smoother.
- `synchronous_commit=off`-style opt-in: separate decision, never default.
- Async derivation stays parked with its recorded re-trigger (ladder at scale).
- macOS benchmark note (recorded in `PROGRESS.md`, applies to all future
  comparisons): SQLite must run `PRAGMA fullfsync=ON` to match Rust
  `sync_all`'s F_FULLFSYNC — plain `fsync` is not durable on macOS.
