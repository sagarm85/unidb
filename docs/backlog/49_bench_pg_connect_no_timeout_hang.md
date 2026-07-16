# Bench harness hangs indefinitely on an unreachable PG_URL (no connect_timeout)

**Type:** Improvement
**Status:** SHIPPED — see `PROGRESS.md` ("Bench harness Postgres connect-timeout
fix, item 49") for measured numbers.

## Problem

Reported: `scripts/report.sh` (and, by extension, `scripts/multi_model_report.sh`
/ `scripts/pg_compare.sh`) "not working and running in indefinite mode" when
generating the Postgres-comparison columns of the multi-model report.

Reproduced directly: `benches/decompose.rs` opened every Postgres connection via
`postgres::Client::connect(url, NoTls)` — the Rust `postgres`/`tokio-postgres`
client applies **no connect timeout unless one is present in the connection
string**. When `PG_URL` points at a target that does not actively refuse the
connection (wrong host/IP, firewalled port, a Postgres container still starting
up, a stale/misrouted `PG_URL` left over from a previous session) the TCP SYN
simply gets no response, and `Client::connect` blocks on the OS's own SYN-retry
ceiling with **zero output** — not a clean, fast connection error.

Confirmed empirically on this host: a refused connection (nothing listening)
fails in **5 ms**; a connection to a black-holed address is still pending past
**8 s** (test capped there) — `/proc/sys/net/ipv4/tcp_syn_retries` is 6, which on
Linux works out to roughly two minutes per attempt before the kernel gives up.
`benches/decompose.rs` dials Postgres from **24 separate call sites** across the
ladder (B1), CRUD suite (B2/Table 3), bulk stress (Table 3.1), FK stress (Table
5), and replaced-stack (Table 4) sections — so a single bad `PG_URL` could stall
the *entire* report generation for many minutes with no diagnostic printed,
reading exactly like an indefinite hang to anyone watching the terminal.

This is distinct from (and does not implicate) the newly-merged items 47/44
(UPDATE B-tree in-place patch, DELETE batched WAL mini-txn) — both were audited
for latch-ordering/blocking-lock issues as part of this investigation (single
latch held at a time in both `patch_many` and `delete_many`, consistent
ascending-key leaf ordering, `lock_mgr.try_acquire_write` is `WaitPolicy::NoWait`
i.e. never blocks) and no deadlock was found there. The parallel-scan worker
governor (item 15) was likewise audited: `acquire()`/`take_from_pool` are
non-blocking (degrades to serial, never waits), and `conc_matrix`'s
`run_with_deadline` already bounds any real deadlock in that harness to a
120 s-per-cell "HANG" verdict on an isolated, fresh, tempdir-scoped engine (no
cross-cell blast radius). The Postgres connect path was the one genuinely
unbounded wait in the whole report pipeline.

## Fix

`benches/decompose.rs`: added `pg_dial(url) -> Result<Client, Box<dyn Error +
Send + Sync>>`, the single place a Postgres connection is opened. It parses
`url` into a `postgres::Config` and calls `.connect_timeout(Duration)` (default
10 s, overridable via `PG_CONNECT_TIMEOUT_SECS`) before connecting — same
`Result<Client, _>` shape as `Client::connect`, so every call site is a drop-in
replacement. All 24 `Client::connect(..., NoTls)` call sites (ladder, CRUD,
bulk, FK, replaced-stack, `pg_connect`/`pg_open_lens`) now route through it.

## Verification

- `PG_URL` pointed at a black-holed private address (`10.255.255.1:5432`),
  `UNIDB_BENCH=mmreport` run directly: **completed in 14.6 s total** (Table
  1/2 computed normally, Table 3's Postgres connect attempt printed `[pg]
  WARNING: PG_URL set but connect failed (error connecting to server) —
  skipping` and the report finished) — previously this exact scenario would
  have blocked on the first connect attempt for ~2 minutes with no output, and
  potentially compounded across further call sites.
- `PG_URL` pointed at a real local Postgres 16 (reachable): full report run
  (`MM_SIZES=1000,10000 MM_BULK_SIZES=1000,10000`) completed normally end to
  end with all Postgres columns populated — the timeout never fires when the
  server actually responds, so normal-path behavior and numbers are unchanged.
- `cargo build --release --bench decompose` — clean.
- `cargo clippy --release --bench decompose -- -D warnings` — clean.

## Depends on / builds on

- None (self-contained bench-harness fix, no engine/format/WAL change).
