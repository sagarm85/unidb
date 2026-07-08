# Phase 6 — Operations & HA (Core WAL + Ops lane)

## Status as of 2026-07-08: NOT STARTED.

Delivers the confirmed target — **a strong single node + read replicas**,
deployable and operable. Companion to [`roadmap.md`](roadmap.md) §4. WAL work is
the Core lane; the rest is the Ops lane (`server/*` + new modules). **Depends on
Phase 1** (auto-checkpoint) and **Phase 5** (concurrent-read apply on replicas).

## Context

Today: manual checkpoint, single-file rewrite-to-truncate WAL, one JWT = full
access, no backups, no replication, no TLS. None of that is deployable at scale.
This phase makes unidb operable and highly available — the single-primary +
read-replica model, which serves the vast majority of real workloads.

## Scope

- **IN:** segmented WAL + replication slots + archiving, streaming replication →
  read replicas + failover, backups + PITR, users/roles/GRANT, TLS + encryption-
  at-rest + audit, observability.
- **OUT:** multi-primary / distributed consensus / sharded writes (parked —
  reverses the single-primary charter).

## Checkpoints

### P6.a — Segmented WAL
- Split the single WAL into fixed-size **segments** (e.g. 16 MB): append to the
  current segment, seal + rotate; consumers read sealed segments; truncation
  deletes **whole consumed segments** (replaces rewrite-to-truncate — this is
  what makes concurrent WAL readers possible at all).
- Files: `wal.rs`, `recovery.rs`, `checkpoint.rs`.

### P6.b — Replication slots + WAL shipping
- Each consumer/replica registers a **slot** (a retained position); the primary
  won't truncate WAL past the minimum slot (so no one's data is removed early —
  monitor for a stuck slot causing WAL growth). Stream records to replicas over
  the network.
- Files: new `replication` module, `server/*`.

### P6.c — Read replicas + failover
- A replica opens read-only, applies the WAL stream (redo) using the Phase-5
  concurrent-read infrastructure, and serves reads. Promotion/failover — manual
  first, then coordinated. Offer a **synchronous-replica** option so a failover
  doesn't lose committed data.

### P6.d — Backups + PITR
- Online **base backup** (a consistent page snapshot) + **WAL archiving**
  (local, and optionally S3) → restore to any point in time.
- Files: new `backup` module.

### P6.e — Users / roles / GRANT
- A user/role store, role hierarchy, per-table/column privileges, and **RLS
  exposed over SQL** (completes today's Rust-API-only RLS). Auth moves from a
  single shared JWT to per-user identity.
- Files: `catalog.rs` (roles/grants), `server/auth.rs`, `sql/*`
  (`CREATE USER`/`GRANT`/`REVOKE`).

### P6.f — Security
- Native **TLS** termination (not just a reverse-proxy assumption), **encryption
  at rest** (page + WAL), and an **audit log**.

### P6.g — Observability
- Slow-query log, stat views (Postgres-`pg_stat_*`-style), active-session + lock
  + wait-event monitoring, `EXPLAIN` integration.

## Locked decisions touched

| Decision | Effect |
|---|---|
| D3 (checkpoint / WAL root) | Extended with segments + slots + archiving |
| §1 "single-primary only" | **Preserved** — async (or sync) read replicas, *not* consensus |
| §1 "no cloud control plane" | Relaxed slightly for backup/replication ops (record in `PROGRESS.md`) |

## Verification gates (Phase 6 done =)

- Replica catch-up + lag monitoring; **failover drill** (promote; with the sync
  option, zero committed-data loss).
- **Backup + PITR restore drill** — restore to a chosen point, verify contents.
- Security review: authz enforced, TLS on the wire, encryption at rest, audit
  trail present.
- An ops runbook (start/stop, backup, restore, add-replica, failover);
  `clippy -D warnings` + `fmt` clean; PR per checkpoint.

## Known limitations / deferred

- Async replication can lose the last un-shipped commits on failover unless the
  sync-replica option is used — document the durability tradeoff.
- No automatic failover coordinator in v1 (manual / simple promotion).
- S3 tiering for cold storage is a follow-up on top of WAL archiving.
- Multi-primary / distributed consensus remains parked.
