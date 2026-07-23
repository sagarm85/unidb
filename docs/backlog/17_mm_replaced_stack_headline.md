# Cross-domain headline — unidb (1 atomic commit) vs the replaced stack

**Type:** Performance
**Status:** SHIPPED (→ PROGRESS.md "Cross-domain headline — unidb (1 atomic commit) vs the replaced stack (item 17)") — native
real-fsync **3.61×** vs the replaced stack; ~parity under Docker's cheap VM fsync;
crash-consistency 0 orphans vs torn record (crash harness 29 → 31).

## Context

CLAUDE.md §1 says we do **not** define success by single-model CRUD vs a
specialized incumbent ("we expect to lose, and that is fine"); §6 says the honest
headline from M2 on is the **replaced stack** — the same cross-domain workload run
against (Postgres + a vector store + a graph DB + a queue) with app glue, versus
unidb doing it in one transaction. This item makes that headline real. (It is why
we **deferred** HOT/A2 — no HOT file created — that reopened locked
decision D4 for ~0.42× on a bench §1 says we should lose.)

**The gap it fixes (grounded in code):** `docker/README.md` called Table 4 *"the
'one atomic transaction vs the replaced stack' framing,"* but `bench_mm_report`
(decompose.rs) actually compared unidb's W4 (row + `VECTOR(128)`+HNSW + graph edge
+ event, **one commit**) against `pg_relational_throughput` — **a single Postgres
relational row and nothing else**. So the project's "beats standard databases"
claim rested on unidb-doing-4×-the-work vs Postgres-doing-1×. The real §6 baseline
was never measured.

## Approach (shipped in this item)

1. **`pg_replaced_stack_throughput(url, n)`** (decompose.rs): the same four
   model-writes as unidb's W4, but as **four independent durable commits with no
   shared transaction** — Postgres row + pgvector(+HNSW) + a graph adjacency table
   + an outbox queue, **each its own connection** so the four `fsync`s cannot
   group-commit-coalesce. Gated on `CREATE EXTENSION vector` (skips cleanly if
   absent, like `PG_URL` unset). Conservative floor — real Neo4j/Kafka/Qdrant are
   heavier, so the true tax (and unidb's win) is larger.
2. **Table 4 rewritten** behind `MM_REPLACED_STACK=1`: the replaced-stack column is
   the headline; `PG relational only` stays as the single-model *floor* (reference,
   not baseline). `unidb ÷ stack` is the win; narrates why it narrows at scale
   (per-model HNSW CPU is paid on both sides).
3. **Crash-consistency proof** (the correctness face HOT could never offer):
   - unidb side, CI-able — `tests/crash`:
     `item16_incomplete_four_model_txn_leaves_zero_orphans` (crash before
     `WAL_TXN_COMMIT` ⇒ recovery undoes row + vector + edge + event, **0 orphans**)
     and `item16_committed_four_model_txn_survives_intact` (all four present). No
     third state. Crash harness 29 → **31**.
   - stack side — `pg_stack_torn_record_demo`: four separate commits mean an
     interruption after the relational commit durably keeps the row while
     embedding/edge/event are absent → a **torn record** printed in-report.
4. **Infra:** `docker/docker-compose.yml` → `pgvector/pgvector:pg18`;
   `MM_REPLACED_STACK=1` toggle in `docker_report.sh` / `multi_model_report.sh`,
   documented in both READMEs.

## Verification
- `tests/crash` item16 pair green; full crash harness **31**; `cargo test` green.
- Plain `cargo bench` (no `PG_URL`/pgvector) unaffected — replaced-stack column
  skipped. `clippy -D warnings` + `fmt` clean; `cargo tree` tokio-free unchanged.
- Benchmark: `MM_REPLACED_STACK=1 scripts/docker_report.sh` (fair fsync) across
  `MM_TX_SWEEP` — unidb-1-txn vs replaced-stack commits/s curve + peak RSS +
  crash-consistency verdict. Numbers → `PROGRESS.md`.

## Non-goals / follow-ups
Real polyglot infra (Neo4j/Kafka/Qdrant) — heavier, different durability models,
would muddy fair-fsync; the PG-roles proxy is the conservative first cut. No
engine/storage/format change. Moat B (log-as-source-of-truth / derived
independently-replayable consumers) is a separate, larger design — the WAL is
physical and WAL-derived streams were rejected (`queue/mod.rs`); B's substrate
would be a generalization of M4's `__events__`+`__consumers__`, filed separately.
