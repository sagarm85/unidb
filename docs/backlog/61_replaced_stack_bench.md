# True replaced-stack benchmark (Postgres + Redpanda, two separate processes)

**Type:** Performance
**Status:** IN PROGRESS

## Context

CLAUDE.md §1/§6 defines unidb's competitive edge as "eliminating the multi-system
dual-write tax" — one WAL append and one commit for row + embedding + graph edge + event
vs 3–4 network round-trips across separate systems. §6 mandates that from M2 on, the
**headline benchmark is the replaced stack**, not single-model CRUD vs an incumbent.

**Item 17** (shipped, PROGRESS.md) took the first step: `pg_replaced_stack_throughput`
runs all four model-writes as four independent durable commits against the **same**
Postgres server (four connections, no shared transaction). This proved the thesis
directionally, but the doc itself noted:

> "Neo4j/Kafka/Qdrant are heavier than PG tables, so the true tax is larger."

The item-17 proxy understates the real replaced-stack overhead in two ways:
1. **No inter-process TCP cost**: the "queue" is a PG outbox table on the same server.
   The bench pays PG's local socket overhead, not a true inter-process round-trip.
2. **Conservative system choice**: PG adjacency table ≠ Neo4j; PG outbox ≠ Kafka.
   A real deployment of the replaced stack uses heavier systems.

Item 61 lifts the proxy to a **genuinely separate event-queue process** using
**Redpanda** (Kafka-compatible, no JVM, ~250 MiB RAM in dev mode) as the queue leg.

## What this item ships

### A — Docker Compose extension (`docker/docker-compose.yml`)

New `redpanda` service (single-broker dev mode, `v24.3.7`):
- `--smp 1 --memory 256M` — lightweight; bench machine resources aren't split.
- Advertised listener `redpanda:9092` — Docker internal DNS, no host port needed.
- Health check via `rpk cluster health`.
- The `bench` service's `depends_on` gains `redpanda: condition: service_healthy`.

### B — Two new bench functions (`benches/decompose.rs`)

**`pg_four_model_one_txn_throughput(url, n)`**
All four model-writes in ONE Postgres transaction (best-case for a single-PG
deployment): `BEGIN` → INSERT rel + INSERT vec + INSERT edge + INSERT outbox → `COMMIT`.
One fsync, fully atomic within PG. Used in Table 4.1 as the "best case for a
PG-only stack" reference, so the cost of splitting across systems is visible by
subtraction.

**`pg_replaced_stack_realistic_throughput(pg_url, redpanda_addr, n)`**
The TRUE replaced stack: three PG autocommit connections (row, pgvector+HNSW, graph
adjacency) + one Redpanda `produce` per record (separate Docker container, real
inter-process TCP, produce ACK waited). No shared transaction — a crash between any
two commits leaves a torn record.

Gated on `REDPANDA_ADDR` being reachable and pgvector installed; skips gracefully
(returns `None`, prints a WARNING line) when either is absent — same pattern as
`PG_URL` unset for all other Postgres paths.

Uses `rskafka 0.6` (pure Rust, no librdkafka) with a single-thread tokio runtime
created per function call; `block_on` waits for each produce ACK before advancing,
matching the "wait for fsync" semantics of the PG writes.

### C — Report integration: Table 4.1 (`bench_mm_report`)

New table gated on `MM_REPLACED_STACK_REALISTIC=1`. For each transaction count in
`MM_TX_SWEEP` it shows four rows:

| system | txns/s | ms/txn | unidb ÷ this | atomicity |
|--------|--------|--------|:------------:|:---------:|
| unidb W4 (1 atomic commit) | — | — | baseline | ✅ |
| PG all-in-one (1 PG txn, same server) | — | — | X.XX× | ✅ (within PG) |
| conservative stack (4×PG outbox, same server) | — | — | X.XX× | ❌ |
| realistic stack (3×PG + Redpanda, 2 processes) | — | — | **X.XX×** | ❌ |

The key deltas to read:
- `PG all-in-one` vs `unidb W4`: the embedded-vs-client-server overhead.
- `conservative` vs `PG all-in-one`: cost of splitting 4 writes into 4 commits.
- `realistic` vs `conservative`: true inter-process TCP tax of the Redpanda leg.
- All `❌` rows vs unidb: the **unconditional** atomicity win (no fsync tuning
  changes which rows can leave torn records).

### D — New environment variables

| var | default | effect |
|-----|---------|--------|
| `MM_REPLACED_STACK_REALISTIC` | (unset) | `=1` enables Table 4.1 |
| `REDPANDA_ADDR` | `localhost:9092` | Redpanda/Kafka bootstrap for Table 4.1 |

Both propagated through `docker/docker-compose.yml`, `scripts/docker_report.sh`,
and documented in `scripts/multi_model_report.sh`.

### E — Backlog and index

This file (`61_replaced_stack_bench.md`) registered in `backlog_index.md`.

## Design choices and honest caveats

**Redpanda, not Kafka:** No JVM startup cost, single Docker image, same Kafka
protocol (`rskafka` connects without code changes). Equivalent wire-level overhead.

**Graph store:** Graph adjacency in PG is a reasonable approximation for the graph
leg in this benchmark. True Neo4j would add JVM startup and Bolt protocol overhead —
a heavier comparison that would widen unidb's advantage. Noted as an honest limitation
(the true tax is larger than measured).

**Single-thread tokio runtime:** `block_on` per produce call adds ~5–20µs of
runtime overhead on top of the actual network RTT. This is negligible (Redpanda RTT
inside Docker is ~0.2–1ms) but acknowledged. A shared runtime across records would be
marginally faster; a function-scoped runtime is simpler and consistent with the
synchronous bench style everywhere else.

**fsync semantics:** Redpanda in single-broker dev mode (`--overprovisioned`) uses
in-memory buffering; produce ACKs are fast but NOT flush-to-platter durable. This is
consistent with the Docker bench's "cheap fsync" caveat: the *ratio* is meaningful
(relative overhead is measured correctly), but absolute durability is VM-bound on
Docker Desktop. The doc states this in Table 4.1 under the existing "durability lens"
disclaimer.

**Conservative proxy still shown:** item 17's `pg_replaced_stack_throughput` column
remains in Table 4 unchanged. Table 4.1 adds the realistic comparison alongside —
both are shown so the reader can see how much Redpanda adds over the outbox proxy.

## Non-goals

- Real Neo4j as graph leg (adds JVM, complicates Docker setup, deferred).
- Redpanda consumer-side latency measurement (only producer RTT is measured here).
- Redpanda durability tuning (`fsync.every.n.seconds`, acks, etc.) — out of scope
  for the benchmark; we measure the default path that a developer would use.

## Verification

- `cargo build --release --bench decompose` compiles with rskafka in dev-deps.
- `cargo test --release` — all existing 424 unit + 46 crash + all integration tests pass.
- `cargo clippy -- -D warnings` and `cargo fmt --all` clean.
- `MM_REPLACED_STACK_REALISTIC=1 scripts/multi_model_report.sh` (with `PG_URL` +
  running Redpanda) produces Table 4.1 with all four rows populated.
- Without `MM_REPLACED_STACK_REALISTIC`, the existing Table 4 is unchanged.
- Without `REDPANDA_ADDR` pointing at a reachable Redpanda, Table 4.1's realistic
  row shows `_(Redpanda n/a)_` and the report still completes.
