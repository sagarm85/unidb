# Postgres baseline comparison — how solid is the standard design?

## Status as of 2026-07-09: NOT STARTED. **Blocked on two merges, by design:**
1. the **commit-time fsync milestone** (`docs/backlog/commit_time_fsync.md`) —
   unidb's "standard design" being measured must be the post-milestone default
   (group-committed force-log-at-commit), not the known per-statement-sync bug;
2. **PR #21** (`benches/decompose.rs`, the W0–W4 ladder) — this work extends
   that harness and its recorded baseline.

## Purpose

A **fitness check, not marketing**: engine-vs-engine, both in their standard
configurations, on the work both can do as shipped. No pgvector, no graph/event
emulation on the Postgres side — the multi-model rungs sit out. This answers
"how solid is unidb's foundation against the reference OLTP engine's default,"
and is deliberately distinct from the two other benchmark framings on file:
- the **ladder** (PR #21): where each write cost lives, unidb-internal;
- the **replaced-stack headline** (`CLAUDE.md` §6, framing "A", later):
  unidb's one-commit multi-model story vs Postgres + vector DB + graph DB +
  queue with no shared transaction.

## The durability lens problem (must be handled, or the numbers lie)

On macOS the two "defaults" are not equally safe: unidb's commit is truly
durable by default (Rust `File::sync_all` → `F_FULLFSYNC`, flush-to-platter),
while Postgres's default `wal_sync_method` uses plain `fsync()`, which macOS
does **not** make durable. Defaults-vs-defaults on a Mac therefore compares a
safer engine against a faster-because-less-safe one. Protocol: **report both
lenses side by side**, never one alone:

- **Lens 1 — as-shipped defaults:** what a user actually gets. Footnote the
  asymmetry. If unidb "loses" only here, the finding is *"unidb's default is
  more conservative than Postgres's on macOS"* — a solidity point.
- **Lens 2 — matched true durability:** Postgres with
  `wal_sync_method=fsync_writethrough` (its `F_FULLFSYNC` mode). The
  engineering truth. Headline numbers come from this lens.

(Same family as the recorded SQLite `PRAGMA fullfsync=ON` rule in
`PROGRESS.md`'s ladder entry — this is the third instance of the macOS
durability trap; treat it as a standing checklist item for any comparison.)

Environment notes: prefer **native** Postgres on macOS for lens 2 (Docker on
macOS runs a Linux VM whose fsync semantics against the host cache are
unquantifiable — flattering to Postgres). A Linux run, where fsync semantics
are uniform for both engines, produces the eventually-publishable numbers.
Client-server vs embedded asymmetry (socket round-trip, per-query planning) is
inherent to what Postgres is: mitigate (local Unix socket, prepared
statements) and state it.

## The fitness matrix

| Test | unidb (post-fsync default) | Postgres (lens 1 / lens 2) | What it proves |
|---|---|---|---|
| Durable single-row insert (W0) | plain table | plain `INSERT`, `PRIMARY KEY` table | commit-path solidity |
| Insert + secondary index (W1) | `USING BTREE` | +btree | index maintenance |
| Point SELECT by key | index path, embedded | prepared stmt, Unix socket | read path + the no-IPC advantage |
| UPDATE (versioned) | MVCC new-version + xmax stamp | heap update (HOT where applicable) | MVCC write cost |
| Sustained churn | update/delete-heavy load, then re-measure read/insert | same, autovacuum on | bloat management maturity (M10 vacuum vs autovacuum) |
| Concurrent writers | N threads over `Arc<Engine>` — **raw-CRUD path and SQL path measured separately** | N connections | write scaling; exposes the documented catalog-`RwLock` SQL-write serialization honestly |
| Size sweep 10k → 5M rows | insert+point-read flatness (P1.c claim) | same | does anything bend at scale |

## Predictions — recorded BEFORE measuring (§6 ethos)

Filed now so results grade the predictions, not the other way around:

1. **Durable insert (lens 2): ~parity.** Both fsync-bound; the ladder already
   showed unidb W0 == SQLite at matched durability.
2. **Point reads: unidb wins** — embedded, no socket, no per-query planning.
3. **Concurrent SQL writes: Postgres wins, possibly by a lot** — unidb's SQL
   writes serialize on the catalog `RwLock` (documented Phase 5 limitation;
   only raw CRUD scales). Publish the ugly number; it points at the next
   optimization.
4. **Update-heavy churn at scale: Postgres ahead** — HOT updates + decades of
   autovacuum tuning vs M10 vacuum.
5. **Big scans (if measured): Postgres wins** — parallel query; unidb has no
   intra-query parallelism (documented deferred).

Expected verdict if predictions hold: *solid, SQLite-class foundation — parity
on durable commits, wins on embedded reads; the gaps are SQL-write concurrency,
churn maturity, and analytic scans — all known, all already documented.* Any
result far from a prediction is the finding worth investigating.

## Checkpoints

### B1 — Postgres harness
- Extend `benches/decompose.rs` with `PG_URL`-gated configs (`pg_w0_default`,
  `pg_w0_durable`, `pg_w1_default`, `pg_w1_durable`); skip cleanly (log, don't
  fail) when `PG_URL` is unset so plain `cargo bench` is unaffected.
- Sync `postgres` crate as a **dev-dependency only** (precedent: rusqlite,
  reqwest — dev-deps never enter the engine's normal-edge graph, so the sync
  invariant holds; verify anyway).
- `scripts/pg_compare.sh`: bring-up (native preferred; Docker mode with the
  VM-durability caveat printed), schema creation, both lenses, teardown.
  Env knobs: `N`, `PG_IMAGE`, `KEEP`.

### B2 — CRUD suite
- Point SELECT by key, UPDATE, and the churn-then-remeasure test, each vs both
  lenses. unidb side reuses existing bench patterns (`benches/load.rs`).

### B3 — Concurrency
- N unidb writer threads (raw CRUD path AND SQL path, separately) vs N
  Postgres connections at N ∈ {1, 2, 4, 8}. Same durability lens (2) both
  sides. This is the checkpoint most likely to produce the unflattering
  number — it ships regardless.

### B4 — Size sweep + report
- 10k → 5M rows: insert throughput + point-read latency at each size, both
  engines. Flat-vs-bending is the result.
- `PROGRESS.md` entry: both lenses side by side, **predictions-vs-actuals
  table**, verdict paragraph, peak RSS, environment (native/Docker/Linux)
  stated. Update `MEMORY.md`; note in this file → done.

## Verification gates (done =)

- All benches run green locally with and without `PG_URL` set; `cargo bench`
  (no Postgres) unaffected.
- Sync invariant still clean (`cargo tree -p unidb --no-default-features
  --edges normal` free of tokio/reqwest/axum/postgres).
- clippy `-D warnings` + fmt clean; no engine-code changes at all (benches +
  script + docs only).
- `PROGRESS.md` entry with both lenses + predictions-vs-actuals recorded.

## Known limitations / deferred

- Not a distributed/replica comparison (single node both sides; unidb replicas
  are Phase 6, Postgres replicas out of scope here).
- Big-scan/analytics comparison optional in v1 (prediction 5 already concedes
  it); include only if time permits.
- The multi-model framings stay separate: ladder (shipped, PR #21) and the
  replaced-stack headline (framing "A", future) — this spec is deliberately
  CRUD-only overlap.
- Linux re-run for publishable numbers: follow-up once the macOS pass lands.
