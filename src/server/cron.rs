//! Scheduled-jobs (cron) background worker (item 144, backlog doc's locked
//! v1 design): a `tokio::spawn` task that runs superuser-registered SQL on a
//! standard 5-field cron schedule — Supabase's `pg_cron`, built entirely as
//! a **control-plane caller of the existing `execute_sql` path**.
//!
//! **No storage-engine change.** This worker never touches the WAL/heap/
//! MVCC/catalog directly — every job run is exactly one `Engine::
//! execute_sql_as_principal` call inside its own `begin`/`commit`, the same
//! path `POST /sql` uses. Cron matching (`crate::cron::CronSchedule`) is
//! pure calendar-field arithmetic with no engine involvement at all.
//!
//! ## Lifecycle (mirrors `server/webhooks.rs`, not `server/mod.rs`'s
//! `spawn_reaper`)
//!
//! Same shape as [`crate::server::webhooks::spawn_webhook_worker`]: the
//! background loop holds only a [`Weak<EngineHandle>`], so it never keeps
//! the server alive, and a failed `upgrade()` is the exit signal. [`spawn_cron_worker`]
//! must be called from inside a tokio runtime (from
//! [`crate::server::AppState::with_config`]).
//!
//! ## The injectable `run_due` seam
//!
//! [`run_due`] is the one piece of scheduling logic that matters, and it is
//! deliberately decoupled from any real clock: it takes `now` as a plain
//! [`chrono::NaiveDateTime`] (the server's **local** wall-clock time — see
//! `docs/REST_API.md`'s cron section for why local, not UTC) and does
//! exactly one pass: find every enabled job whose schedule matches `now`'s
//! minute and run each once. [`spawn_cron_worker`]'s real loop sleeps until
//! the next minute boundary and calls `run_due(&engine, &state,
//! Local::now().naive_local())`; `tests/item144_cron.rs` calls the exact
//! same function with a hand-picked `now`, so the test suite never waits on
//! a real minute to tick over.
//!
//! **Non-blocking by design.** `run_due` does not await job completion — it
//! marks each due, not-already-running job as in-flight
//! ([`CronState::try_start`]) and `tokio::spawn`s its execution, then
//! returns immediately. This is deliberate, not an oversight: if `run_due`
//! blocked on a slow job, the *whole scheduler* would stall behind it and
//! every other job (and the next tick) would be delayed too — exactly the
//! failure mode the backlog spec's "no overlap" contract exists to bound.
//! Instead, a job whose previous run is still `running` when its next tick
//! fires is skipped this tick (`skipped_overlap`, logged +
//! `unidb_cron_skipped_overlap_total`), and every other due job still runs
//! on schedule. A caller (test or otherwise) that needs to observe a run's
//! outcome polls [`CronState::status`] (mirrors `tests/item141_webhooks.rs`'s
//! `wait_for` pattern for the same reason: delivery there is equally
//! decoupled from the poll loop that triggers it).
//!
//! **No backfill.** `run_due` only ever evaluates the exact `now` it's
//! given — there is no missed-tick catch-up. A server that was down (or a
//! test that never calls `run_due` for some intervening minute) simply
//! never runs jobs for that minute; this is a scheduler, not a durable job
//! queue (documented non-goal).
//!
//! **Isolation.** Each job's run is independent: a failing job's error is
//! captured into its own [`CronRunStatus::last_error`] and never panics,
//! never blocks, and never affects any other job or the engine (mirrors
//! `webhooks.rs::deliver_with_retry`'s isolation posture).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use chrono::{Datelike, Local, NaiveDateTime, Timelike};

use crate::auth_principal::AuthPrincipal;
use crate::authz::CronJobDef;
use crate::server::engine_handle::EngineHandle;

/// In-memory last-run status for one registered cron job. **Never
/// persisted** — a server restart resets every job's status to "never run"
/// (documented non-goal: no durable job history in v1).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CronRunStatus {
    /// Unix milliseconds of the last completed run (success or failure).
    /// `None` if the job has never run since this process started.
    pub last_run_at: Option<i64>,
    /// `"ok"` or `"error"` for the last completed run; `None` if never run.
    pub last_status: Option<String>,
    /// The error message from the last run, if it failed; `None` after a
    /// successful run or if the job has never run.
    pub last_error: Option<String>,
    /// Total completed runs (success + failure). Does **not** count ticks
    /// skipped via `skipped_overlap`.
    pub run_count: u64,
}

/// Per-job runtime bookkeeping: the status the admin API reports, plus the
/// `running` guard backing the no-overlap contract (see the module doc).
#[derive(Default)]
struct JobRuntime {
    status: CronRunStatus,
    running: bool,
}

/// Shared, in-memory scheduler state: last-run status per job name, plus the
/// no-overlap in-flight guard. Cheap to clone (one `Arc<Mutex<..>>`) —
/// shared between `AppState` (read by `GET /cron/jobs`), the background
/// worker loop, and the test-injectable [`run_due`].
#[derive(Clone, Default)]
pub struct CronState {
    jobs: Arc<Mutex<HashMap<String, JobRuntime>>>,
}

impl CronState {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, JobRuntime>> {
        self.jobs.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The last-run status for `name`, or the all-`None`/zero default if it
    /// has never run (or was never registered at all) since this process
    /// started. Used by `GET /cron/jobs` (`server::handlers::get_cron_jobs`).
    pub fn status(&self, name: &str) -> CronRunStatus {
        self.lock()
            .get(name)
            .map(|j| j.status.clone())
            .unwrap_or_default()
    }

    /// Claim `name` as running. Returns `true` if this call won the claim
    /// (the caller should now run the job and eventually call [`Self::
    /// finish`]); `false` if `name` was already running — the caller must
    /// skip this tick (`skipped_overlap`) rather than run it again.
    ///
    /// `pub` (not just `pub(crate)`) so `tests/item144_cron.rs` can drive
    /// the exact no-overlap guard [`run_due`] itself uses, deterministically
    /// — simulate "job still in flight" by calling this directly instead of
    /// racing two real concurrent runs against wall-clock timing.
    pub fn try_start(&self, name: &str) -> bool {
        let mut jobs = self.lock();
        let entry = jobs.entry(name.to_string()).or_default();
        if entry.running {
            false
        } else {
            entry.running = true;
            true
        }
    }

    /// Release the running guard claimed by [`Self::try_start`] and record
    /// the run's outcome. `pub` for the same direct-testability reason as
    /// `try_start`.
    pub fn finish(&self, name: &str, result: Result<(), String>) {
        let mut jobs = self.lock();
        let entry = jobs.entry(name.to_string()).or_default();
        entry.running = false;
        entry.status.last_run_at = Some(now_unix_ms());
        entry.status.run_count += 1;
        match result {
            Ok(()) => {
                entry.status.last_status = Some("ok".to_string());
                entry.status.last_error = None;
            }
            Err(e) => {
                entry.status.last_status = Some("error".to_string());
                entry.status.last_error = Some(e);
            }
        }
    }
}

fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Run one job's `sql` under its `run_as` principal, via exactly the
/// `execute_sql`/`begin`/`commit` path any other caller uses — see the
/// module doc. Never panics: `begin`/`execute_sql`/`commit` failures are all
/// captured into the returned `Err` string, never propagated as a panic.
///
/// `run_as: None` is the embedded/superuser identity (`AuthPrincipal::
/// user(None)`) — the documented default, unrestricted, bypassing RLS like
/// any other superuser call. `run_as: Some(name)` runs exactly as
/// `Engine::execute_sql_as(Some(name), ..)` would: that principal's own
/// table grants and RLS policies apply.
async fn run_job_sql(engine: &EngineHandle, job: &CronJobDef) -> Result<(), String> {
    let principal = AuthPrincipal::user(job.run_as.clone());
    let xid = engine.begin(None).await.map_err(|e| e.to_string())?;
    match engine
        .execute_sql_as_principal(principal, xid, job.sql.clone())
        .await
    {
        Ok(_) => engine.commit(xid).await.map_err(|e| e.to_string()),
        Err(exec_err) => {
            // Best-effort — the transaction never committed, so an abort
            // failure here (e.g. engine already unavailable) doesn't change
            // the outcome the caller sees; the original `exec_err` is what
            // gets recorded either way.
            let _ = engine.abort(xid).await;
            Err(exec_err.to_string())
        }
    }
}

/// Run one already-claimed job to completion and record its outcome.
/// Isolated by construction: `tokio::spawn`ed independently per job (see
/// [`run_due`]), so this job's duration/failure can never delay or affect
/// any other job.
async fn run_one(engine: Arc<EngineHandle>, state: CronState, job: CronJobDef) {
    let result = run_job_sql(&engine, &job).await;
    metrics::counter!("unidb_cron_runs_total").increment(1);
    match &result {
        Ok(()) => tracing::debug!(job = %job.name, "cron job run succeeded"),
        Err(e) => {
            metrics::counter!("unidb_cron_failures_total").increment(1);
            tracing::warn!(job = %job.name, error = %e, "cron job run failed");
        }
    }
    state.finish(&job.name, result);
}

/// Find every enabled job whose cron schedule matches `now` (server local
/// time, minute granularity) and start each one. See the module doc for the
/// full non-blocking / no-overlap / no-backfill contract. This is the
/// injectable seam `tests/item144_cron.rs` drives directly; [`spawn_cron_worker`]'s
/// real loop calls it once per minute with `Local::now().naive_local()`.
///
/// A job whose stored `schedule` fails to re-parse (should never happen —
/// `Engine::upsert_cron_job` validates it at registration time — but the
/// worker must never trust storage blindly) is logged and skipped, not
/// treated as a crash.
pub async fn run_due(engine: &Arc<EngineHandle>, state: &CronState, now: NaiveDateTime) {
    let jobs = match engine.list_cron_jobs().await {
        Ok(jobs) => jobs,
        Err(e) => {
            tracing::warn!(error = %e, "cron worker: failed to list registered jobs this tick");
            return;
        }
    };
    let minute = now.minute();
    let hour = now.hour();
    let dom = now.day();
    let month = now.month();
    let dow = now.weekday().num_days_from_sunday();

    for job in jobs {
        if !job.enabled {
            continue;
        }
        let schedule = match crate::cron::CronSchedule::parse(&job.schedule) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    job = %job.name,
                    schedule = %job.schedule,
                    error = %e,
                    "cron worker: stored schedule failed to re-parse; skipping this tick"
                );
                continue;
            }
        };
        if !schedule.matches(minute, hour, dom, month, dow) {
            continue;
        }
        if !state.try_start(&job.name) {
            tracing::info!(job = %job.name, "cron job skipped: previous run still in flight (skipped_overlap)");
            metrics::counter!("unidb_cron_skipped_overlap_total").increment(1);
            continue;
        }
        tokio::spawn(run_one(engine.clone(), state.clone(), job));
    }
}

/// Spawn the background scheduler loop: sleeps until the next minute
/// boundary (server local time), then calls [`run_due`] — forever, until
/// `engine` can no longer be upgraded (the server is shutting down). Must be
/// called from inside a tokio runtime.
pub fn spawn_cron_worker(engine: Weak<EngineHandle>, state: CronState) {
    tokio::spawn(async move {
        loop {
            let now = Local::now();
            let ms_into_minute =
                u64::from(now.second()) * 1000 + u64::from(now.timestamp_subsec_millis());
            let sleep_ms = 60_000u64.saturating_sub(ms_into_minute).max(1);
            tokio::time::sleep(Duration::from_millis(sleep_ms)).await;

            let Some(handle) = engine.upgrade() else {
                return; // AppState/EngineHandle dropped — nothing left to serve
            };
            run_due(&handle, &state, Local::now().naive_local()).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_start_then_finish_releases_the_guard() {
        let state = CronState::new();
        assert!(state.try_start("job1"));
        // A second claim before `finish` is the no-overlap skip path.
        assert!(!state.try_start("job1"));
        state.finish("job1", Ok(()));
        // Released — the next tick can claim it again.
        assert!(state.try_start("job1"));
    }

    #[test]
    fn finish_records_status_and_increments_run_count() {
        let state = CronState::new();
        state.try_start("job1");
        state.finish("job1", Ok(()));
        let status = state.status("job1");
        assert_eq!(status.last_status.as_deref(), Some("ok"));
        assert_eq!(status.last_error, None);
        assert_eq!(status.run_count, 1);

        state.try_start("job1");
        state.finish("job1", Err("boom".to_string()));
        let status = state.status("job1");
        assert_eq!(status.last_status.as_deref(), Some("error"));
        assert_eq!(status.last_error.as_deref(), Some("boom"));
        assert_eq!(status.run_count, 2);
    }

    #[test]
    fn status_of_unknown_job_is_the_never_run_default() {
        let state = CronState::new();
        assert_eq!(state.status("nope"), CronRunStatus::default());
    }

    #[test]
    fn jobs_are_independent() {
        let state = CronState::new();
        assert!(state.try_start("a"));
        // A different job name is never blocked by another job's guard.
        assert!(state.try_start("b"));
        state.finish("a", Ok(()));
        state.finish("b", Err("x".to_string()));
        assert_eq!(state.status("a").last_status.as_deref(), Some("ok"));
        assert_eq!(state.status("b").last_status.as_deref(), Some("error"));
    }
}
