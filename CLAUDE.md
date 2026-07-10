# CLAUDE.md

> The durable brain for this project. Claude Code reads this **every session**.
> It changes rarely. For "where are we right now," read `MEMORY.md` first.
> For "what has shipped," read `PROGRESS.md`.

---

## 0. Session protocol (do this every time)

1. **Read `MEMORY.md` first.** It holds the current implementation state and the ordered next tasks. Never start work without it.
2. Read this file for the rules and locked decisions below.
3. Do the work for the **current milestone only** (see §5). Do not pull features forward from the backlog.
4. **At session end, update `MEMORY.md`** (current state + session log entry) and, if a milestone shipped, `PROGRESS.md`. **Before pushing or raising a PR, also check `README.md` and `docs/` for staleness** (see §9) — these do not update themselves the way `PROGRESS.md` does as part of the per-milestone habit.
5. **Dates:** always stamp entries with the *actual current system date*. Never copy a date from an earlier session or from this file. If unsure of the date, get it from the system, not from context.

---

## 1. What this is

A single embedded storage/transaction engine in Rust that unifies four data models over **one page store, one WAL, one buffer pool, one transaction manager**: relational CRUD, vector search (HNSW), graph edges, and a WAL-derived event queue. A single transaction can touch all four atomically because there is one node and one log.

**Read this twice — it sets what "success" means:**

- We are **not** trying to out-Postgres Postgres on single-model workloads. Rebuilding Postgres's architecture (which we largely do) yields Postgres-class throughput at best, and inherits its bloat/VACUUM cost. On a single-table CRUD benchmark against a specialized incumbent, we expect to *lose*, and that is fine.
- Our competitive edge is **eliminating the multi-system dual-write tax.** "Save row + embedding + graph edge + event" is *one* WAL append and *one* commit for us, versus 3–4 network round-trips with no shared transaction for Postgres + vector store + graph DB + Kafka. That is the win, and it is **workload-specific**.
- Therefore we **benchmark the stack we replace** on cross-domain transactional workloads — not one engine on its home turf. See §6.

**Scope discipline (non-goals):** no distributed consensus (single-primary only); not full ANSI SQL (practical subset); no cloud control plane. The unification goal fights the throughput goal — every generalization we add costs throughput a specialized engine wouldn't pay. When in doubt, keep it specialized and simple.

---

## 2. Architecture (layer stack)

```
API layer (M5, optional server)      REST/gRPC + Auth(JWT) + subscribe; embedded crate is primary
Query & execution (M1+)              parser -> logical -> physical; vectorized scans; row-at-a-time point ops
Logical record layer (M1+)           rows / vector records / graph edges / queue events — one record-kind tag
Transaction & concurrency (M1+)      MVCC snapshots; lock mgr keyed by (record_kind, record_id)
Storage layer (M0) ← WE ARE HERE     single-file paged store; buffer pool; WAL; control file; recovery
```

Everything sits on the storage layer. M0 has zero dependency on any vector/graph/queue/RLS decision.

---

## 3. LOCKED design decisions — do not silently re-litigate

Changing any of these requires explicit human sign-off, recorded in `PROGRESS.md`. They are settled.

### Storage & recovery (M0)
- **D1 — Buffer policy: steal + no-force, ARIES-style.** Requires **both redo and undo** logging. This dictates the WAL record format.
- **D2 — Atomic unit in M0 is a single statement, implemented as a mini-transaction:** a WAL-bracketed group of page writes (begin/commit log records) that redo/undo treat as one. There is no user-visible transaction in M0.
- **D3 — Control file (our `pg_control`).** A dedicated meta-page/file holding: magic number, format version, `page_size`, last-checkpoint LSN, WAL tail pointer. **Recovery starts here** — it is the single source of recovery truth. Created at DB init.
- **D4 — Tuple header reserves MVCC bytes now, versioning deferred.** M0 reserves space for `xmin`/`xmax` in the tuple header but may do **in-place** UPDATE/DELETE. Real MVCC versioning (insert-new-version) lands in M1. The on-disk tuple format must be forward-compatible so M1 does not rewrite it.
- **D5 — WAL-before-page invariant (the one that must never break).** A dirty page may **not** be flushed/evicted while `page.LSN > durable_WAL_LSN`. The buffer pool enforces this on every eviction. This is a tested invariant, not folklore.
- **D6 — Single-file storage for M0** (WAL may be a separate file). We deliberately diverge from the multi-file idea for now — it forces per-file LSN tracking into recovery for benefits (file placement, parallel backup) we don't need yet. Matches the DuckDB inspiration. Revisit post-M4.
- **D7 — Crash-injection harness is an M0 deliverable, kept simple.** "Kill at ~5 defined points, reopen, assert recovered state." **Not** a deterministic simulator (no TigerBeetle/FoundationDB-grade sim). See §7.
- **D8 — Page size 8 KiB default, config-overridable at init**, baked into the control file. Not changeable after files exist.
- **D9 — On-disk format is fixed little-endian**, every page carries a CRC32 checksum + LSN, magic+version in the control file.

### Transactions & isolation (decided now, implemented M1)
- **D10 — Isolation: default `READ COMMITTED`; offer `REPEATABLE READ` (= snapshot isolation) on the same MVCC snapshots.** RC and SI differ only in snapshot lifetime (per-statement vs per-transaction).
- **D11 — Build the `on_read()` / `on_write()` seam now (no-op initially)** in every scan and index-lookup path, so `SERIALIZABLE`/SSI is a later *addition*, not an executor rewrite. Read-set tracking is the retrofit trap we are avoiding.
- **D12 — Implement SI's abort-on-conflict path before RC's re-evaluation path.** RC is the default but is *harder* to implement correctly in MVCC (concurrent-update re-check, à la Postgres EvalPlanQual); SI simply aborts. Get the simple conflict path working first.

### Observability (from M0)
- **D13 — Structured logging from day one** for WAL writes, checkpoint events, and crash-recovery replay (this is how "did recovery work" is answerable). Prometheus-style `/metrics` endpoint comes with the server (M5). Use `tracing`.

---

## 4. Coding conventions

- **Rust edition 2021**, stable toolchain. `#![forbid(unsafe_code)]` except in the page/mmap module, which must isolate and document every `unsafe`.
- **No `unwrap()`/`expect()` outside tests.** Errors via `thiserror`; return `Result`. Recovery code especially must never panic on a malformed page — it must detect and report.
- **No `serde` on the page hot path.** Use `zerocopy` / hand-rolled encode for page & WAL records — we need exact byte control. `serde` is fine for config/CLI only.
- **On-disk = little-endian, always.** Never serialize host-endian.
- **Every mutation is WAL-logged before the page is written** (D5). No exceptions, no "fast path" that skips the log.
- **Module layout (M0):**
  ```
  src/
    format.rs      magic, version, constants, endian helpers
    control.rs     control file (D3)
    page.rs        page header + slotted-page body; tuple header w/ reserved MVCC bytes (D4)
    bufferpool.rs  frames, pin/unpin, clock/LRU, dirty set, WAL-before-page enforcement (D5)
    wal.rs         log records (redo+undo), LSN, fsync boundary, mini-txn bracketing (D2)
    heap.rs        single-table heap: insert/read/update/delete; simple FSM
    checkpoint.rs  flush dirty + checkpoint record + control-file update + WAL truncation
    recovery.rs    open -> read control -> redo -> undo incomplete mini-txn (D1)
    lib.rs         engine open/close API
  tests/
    crash/         crash-injection harness (D7)
  benches/         load tests (throughput + memory)
  ```
- Suggested crates: `memmap2` (page file), `crc32fast` (checksums), `zerocopy` (format), `thiserror` (errors), `tracing` + `tracing-subscriber` (D13). Deferred: `sqlparser` (M1), `arrow` (M1+ vectorized), `hnsw_rs` or hand-rolled (M2), `tokio` (M5 server only — the engine stays sync).

---

## 5. Milestone roadmap

Current milestone is tracked in `MEMORY.md`. Each milestone is independently demoable and benchmarkable.

- **M0 — Storage core.** Single-file page store, buffer pool, WAL, control file, crash recovery, durable single-table CRUD, single-threaded, **no MVCC**. Plus crash harness (D7) + structured logging (D13).
- **M1 — MVCC + CRUD.** Transactions, RC default / RR available (D10–D12), `on_read` seam, catalog, SQL subset. Fold in JSON columns and RLS here (they are a column-type + index-type and a planner rewrite, respectively — same machinery).
- **M2 — Vector & Text search.** `VECTOR(n)` type, HNSW secondary index built **asynchronously** in a background worker (row write is the only synchronous cost), `NEAR` operator; full-text (inverted index) built alongside since both are over-fetch-then-filter secondary indexes.
- **M3 — Graph.** Edge records `(from_id, to_id, edge_type, props)`, edge-list index by `from_id`, Cypher subset. Per-edge locking; batch-latch the adjacency scan on hot hubs.
- **M4 — Event queue.** WAL-derived event stream, durable consumer offsets, replay. Resolve the slow-consumer-vs-vacuum durability contract here.
- **M5 — API / server.** Stabilize the embedded crate; optional server with REST + JWT auth + subscribe API + `/metrics`.

---

## 6. Benchmark philosophy (non-negotiable)

- **Baseline choice:** for **M0–M1** (single-model CRUD) compare against **SQLite** — the honest analog, since both are embedded single-file engines. Do **not** headline a Postgres single-table comparison; we will lose it and it measures the wrong thing.
- From **M2 onward**, the headline benchmark is the **replaced stack**: the same cross-domain workload run against (Postgres + a vector store + a graph DB + a queue) with app-level glue, versus us doing it in one transaction. This is where "beats standard databases" is true and defensible.
- **Every milestone PR must include** a metrics table: throughput (ops/sec), latency p50/p99, **peak RSS**, and the baseline comparison. Numbers go in `PROGRESS.md`. "Is this actually better" stays evidence-based, never aspirational.

---

## 7. Testing requirements

- **Crash-injection harness (D7) is mandatory for M0 and maintained forever after.** Minimum injection points: (a) after WAL append, before page flush; (b) mid-checkpoint; (c) after heap mutation, before commit record; (d) during WAL truncation; (e) immediately after commit fsync. For each: kill → reopen → assert recovered state equals the expected committed set. A committed statement must survive; an incomplete one must leave no trace.
- **Property/fuzz tests for recovery:** random sequences of insert/update/delete with random crash points; invariant = recovered DB is a valid prefix of committed operations.
- **Invariant assertions in debug builds:** D5 (no page flush ahead of WAL), checksum validity on every page read, control-file consistency.
- Unit tests per module; integration test for full open→CRUD→crash→recover→verify.

---

## 8. Commands

```bash
cargo build                 # debug
cargo build --release       # release (use for benchmarks)
cargo test                  # unit + integration
cargo test --test crash     # crash-injection harness
cargo bench                 # load tests (throughput + memory)
cargo clippy -- -D warnings # lint gate; PRs must be clippy-clean
cargo fmt --all             # format gate
```

---

## 9. PR / commit workflow

- **Backlog docs follow `docs/backlog/CONVENTIONS.md`; `docs/backlog/README.md` is the numbered index** (the at-a-glance pending/completed tracker + where the next number comes from). Read both before creating a backlog file. In short: every effort is one of **Phase / Milestone / Improvement / Performance**; **new files are named `NN_<slug>.md`** where `NN` is the next free stable ID in the index (no `phase` in the slug, no internal sub-parts like `_phaseA_B` — name those inside the doc). Register each new file in `README.md`. Each file opens with a `**Type:**` + `**Status:**` header; metrics live in `PROGRESS.md`, not the backlog file. (Existing files keep their un-numbered names; the historical `phase<N>_` files are the roadmap's numbered phases.)
- **One PR per milestone.** The PR description **must** contain the benchmark metrics table (§6) and a note on peak memory.
- Conventional commits (`feat:`, `fix:`, `test:`, `bench:`, `docs:`, `refactor:`, `perf:`).
- Every PR: `cargo fmt` clean, `clippy -D warnings` clean, all tests + crash harness green, benchmarks recorded.
- Update `PROGRESS.md` (milestone entry with metrics + PR link) and `MEMORY.md` (current state) in the same PR.
- **Before every push or PR, check `README.md` and every file under `docs/` for staleness — not just `PROGRESS.md`/`MEMORY.md`.** `PROGRESS.md` gets updated reliably because it's part of the per-milestone habit; `README.md` and `docs/` (`docs/design/engine_design.md`, `docs/REST_API.md`, `docs/backlog/*.md`) do not update themselves and have gone stale in the past (e.g. a design doc left claiming a shipped milestone was "not started," and once documenting a policy that had since been reverted as a bug fix). Concretely, for any change that touches the public surface (new/changed API, new module, new deployment mode, a reverted design decision, a milestone opened or closed):
  - `README.md`: status line, milestone table, project-layout tree, any usage section the change affects.
  - `docs/design/engine_design.md` (if it exists): the section covering the affected area, the module map, the tech-debt registry, and the document-version footer.
  - `docs/REST_API.md`: any new/changed/removed route or error code.
  - `docs/backlog/*.md`: flip a plan's status line to done/shipped (pointing at its `PROGRESS.md` entry) once the work it describes lands, rather than leaving it claiming "not started."
  - If a design decision documented in one of these files is found to be wrong (a bug, not a tradeoff), correct it explicitly with an inline correction note, not a silent rewrite — the same evidence-based ethos §0.5 and §6 already apply to `PROGRESS.md` extends to every doc.

### Definition of done (per milestone)
Feature works end-to-end · crash harness green (where storage is touched) · benchmark table recorded in `PROGRESS.md` · `MEMORY.md` updated · `README.md` and affected `docs/` files updated (see above) · demoable in isolation · no locked decision (§3) violated.

---

## 10. Do NOT

- Pull backlog features forward (S3 tiering, large objects, full PITR, compression, extensions) — they are tracked in the design doc, not now.
- Switch M0 to multi-file (D6) or skip the control file (D3) or the crash harness (D7).
- Add a code path that writes a page before its WAL record (D5).
- Re-open a §3 decision without explicit human sign-off recorded in `PROGRESS.md`.
- Headline a single-model benchmark vs a specialized incumbent as evidence of success (§6).
- Copy dates from context — always use the current system date (§0.5).
