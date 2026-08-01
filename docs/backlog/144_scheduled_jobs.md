# Scheduled jobs (cron)

**Type:** Improvement
**Status:** IN PROGRESS

> Wave-2 free-roadmap item (`137`). Supabase's `pg_cron`: run SQL on a schedule.
> unidb has no scheduler. This adds a background worker that runs registered SQL
> statements on a cron schedule. **Control-plane only** — the worker calls the
> existing `execute_sql` path (post-commit, same as any statement); no
> storage-engine change, crash harness stays 54/54.
>
> **Free/self-hostable:** entirely in-process. No third-party.

## Design (v1)

**Registration (control-plane, superuser).** A job =
`(name, schedule, sql, enabled, run_as?)`:
- `schedule` — a standard 5-field cron expression (min hour dom mon dow),
  evaluated in the server's local time (documented). Use a small, well-
  maintained cron crate (e.g. `cron` / `saffron`) or a minimal hand-rolled
  5-field parser — no heavy dep.
- `sql` — the statement(s) to run (one `execute_sql` call).
- `run_as` — the principal the job runs under. **Default: a superuser/service
  principal** (cron jobs are admin-defined; documented). Optionally a named role
  so RLS/grants apply as that role — resolve via the existing principal path.
- Stored in the control-plane store (`roles.json`-style, `#[serde(default)]`, no
  FORMAT_VERSION bump). Admin API (superuser-gated, mirroring `/webhooks` /
  `/realtime/policies`): `POST /cron/jobs` (upsert), `GET /cron/jobs` (list +
  last-run status), `DELETE /cron/jobs/{name}`. + `Engine`/`EngineHandle` methods.

**Scheduler worker (background, runtime-gated).** One `tokio` task:
- Ticks (e.g. every 1s / aligned to the minute); for each enabled job whose cron
  expression matches the current minute and hasn't already run this minute, spawn
  the run.
- Each run calls `execute_sql` under `run_as` and records `last_run_at`,
  `last_status` (ok/error), `last_error`, `run_count` in-memory (+ a
  `unidb_cron_runs_total` / `unidb_cron_failures_total` metric). A failing job
  logs + is isolated (never panics, never blocks other jobs or the engine).
- **No overlap:** if a job's previous run is still in flight when its next tick
  fires, skip (log a `skipped_overlap`) — don't stack runs. Missed ticks
  (server down) are NOT backfilled in v1 (documented — this is a scheduler, not a
  durable job queue).
- Never blocks the commit path; each run is its own transaction via `execute_sql`.

## Correctness / security
- Superuser-only registration. `run_as` defaults to an admin principal; if a
  named role is given, the job's SQL is subject to that role's RLS/grants (reuse
  the principal machinery) — so a job can be scoped down, not just run as god.
- The scheduler is strictly a *caller* of `execute_sql` — no new write path, no
  WAL/heap/MVCC change. A job that does DML commits like any statement.
- Cron expression validated at registration (reject malformed → 400).
- Job SQL is stored verbatim (admin-authored); it is NOT user input in the SSRF
  sense. Errors captured per-run, never crash the worker.

## Acceptance
- Register a job `* * * * *` that INSERTs a marker row; within ~1 minute the row
  appears; `GET /cron/jobs` shows `last_status: ok` + an incremented run count.
  (Test with a fast tick / a directly-invokable "run due jobs now" hook so the
  test doesn't wait a real minute — expose an internal `run_due(now)` the test
  drives with an injected timestamp.)
- A job with bad SQL records `last_status: error` + `last_error`, and a second
  healthy job still runs (isolation).
- `run_as` a limited role → the job's DML is subject to that role's RLS/grants
  (parity with running the same SQL as that principal).
- Malformed cron → 400; superuser-only admin (non-superuser → 403).
- Overlap: a long job doesn't stack (skipped_overlap logged).
- New `tests/item144_cron.rs` (`#![cfg(feature = "server")]` first line), driving
  the injectable `run_due(now)` — NO real wall-clock waits.
- Every pre-existing server/auth/events test unchanged.
- **Crash 54/54**; `cargo test --no-run` (no features) + `clippy --all-features
  --all-targets -D warnings` + `fmt` clean.
- `docs/REST_API.md` (`/cron/jobs` routes + cron syntax + run_as + no-backfill
  note), `README.md`, `137` Wave-2 line, this Status flipped on merge.

## Non-goals (v1)
- Missed-tick backfill / durable job history (in-memory status only).
- Sub-minute schedules; second-level cron.
- A UI (studio panel is a separate item).
