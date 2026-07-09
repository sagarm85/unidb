//! Autovacuum (A3): the background launcher that auto-triggers the existing,
//! already-safe M10 [`crate::Engine::vacuum`].
//!
//! This is the auto-trigger analogue of auto-checkpoint (P1.e): a `std::thread`
//! (deliberately **not** tokio — the engine core stays synchronous, §4) that
//! sleeps `naptime`, wakes, evaluates the A2 policy against the A1 dead/live
//! estimates, and calls `Engine::vacuum` when the trigger fires. Autovacuum
//! *only auto-triggers* reclamation — it does not re-implement it and does not
//! touch the vacuum horizon.
//!
//! ## Why concurrent background vacuum needs no new locking (the M3.b-style note)
//!
//! Running vacuum from a background thread, concurrently with foreground
//! writers, is safe on today's engine **for reasons that already hold** — this
//! checkpoint adds an *actor*, not a new locking regime:
//!
//! - **The engine is `Send + Sync` (P5.e).** Every storage component
//!   (`BufferPool`/`Wal`/`Heap`/`TransactionManager`/`LockManager`) exposes a
//!   `&self` API, so a second thread issuing `engine.vacuum()` is no different
//!   from the worker pool that already issues concurrent writes.
//! - **`Engine::vacuum` already serializes with the other structure-mutating
//!   write paths.** It takes the coarse `write_serial` lock (P5.e-3) that also
//!   guards edge/LOB/event/DDL writes, and it mutates heap pages under the same
//!   per-page latches every foreground mutation uses (M10). So an autovacuum
//!   pass interleaves with foreground work exactly as a *manual* `vacuum()` call
//!   already does — a case the M10 tests and the P5 concurrency tests cover.
//! - **The horizon is already concurrency-correct and slot-pinned.**
//!   `vacuum_horizon()` is the min `xmin` over all live writers **and** live
//!   `ReadHandle` readers (P5.c/M10.a), and is held back by replication slots
//!   (P6.b). A background caller observes the identical horizon; it can never
//!   reclaim a version a concurrent RR reader or a replication slot still needs.
//!   Autovacuum respects this unchanged — it computes nothing about visibility
//!   itself.
//! - **Crash-safety is unchanged.** `WAL_VACUUM` is redo-only/idempotent and
//!   `vacuum` self-syncs its records; a crash mid-autovacuum recovers exactly as
//!   a crash mid-manual-vacuum does (crash point P10), because it *is* the same
//!   code path fired at a different time.
//!
//! ## Lifetime / shutdown (the M2.b index-worker pattern, adapted)
//!
//! The worker holds a [`Weak`] reference to the engine, **never a strong
//! `Arc`**: a strong reference would form a reference cycle (engine owns the
//! join handle, thread owns the engine) that would keep the engine alive
//! forever and prevent `Engine::Drop` from ever running. Holding `Weak` and
//! upgrading briefly per tick breaks the cycle — when the last real
//! `Arc<Engine>` is dropped, the worker's `upgrade()` starts returning `None`
//! and it exits. The [`AutoVacuumHandle`] lives as an engine field, so dropping
//! the engine drops the handle, whose own `Drop` signals shutdown and joins the
//! thread (bounded, like M2.b, so a stuck pass can't hang teardown). A
//! `worker_id` guard covers the one race where the worker itself holds the last
//! strong reference (an external drop landing mid-pass): there the handle drop
//! runs *on* the worker thread, so it skips the self-join and lets the thread
//! unwind out normally.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::thread::{self, JoinHandle, ThreadId};
use std::time::Duration;

use crate::Engine;

/// Shutdown coordination shared between the engine-held handle and the worker
/// thread: a flag plus a condvar so the worker's `naptime` sleep is interrupted
/// immediately on shutdown rather than waiting out the full nap.
struct Shutdown {
    stop: Mutex<bool>,
    cv: Condvar,
}

impl Shutdown {
    fn signal(&self) {
        *self.stop.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.cv.notify_all();
    }
}

/// Owns the background autovacuum thread (A3). Stored as an [`Engine`] field so
/// that dropping the engine runs this `Drop` — the clean-shutdown hook.
pub(crate) struct AutoVacuumHandle {
    shutdown: Arc<Shutdown>,
    join: Option<JoinHandle<()>>,
    worker_id: ThreadId,
}

impl AutoVacuumHandle {
    /// Spawn the launcher for `engine`. The thread captures a `Weak<Engine>`
    /// (see the module doc) and the shared shutdown signal.
    pub(crate) fn spawn(engine: &Arc<Engine>) -> Self {
        let shutdown = Arc::new(Shutdown {
            stop: Mutex::new(false),
            cv: Condvar::new(),
        });
        let weak = Arc::downgrade(engine);
        let worker_shutdown = Arc::clone(&shutdown);
        let join = thread::Builder::new()
            .name("unidb-autovacuum".into())
            .spawn(move || worker_loop(weak, worker_shutdown))
            .expect("failed to spawn autovacuum thread");
        let worker_id = join.thread().id();
        Self {
            shutdown,
            join: Some(join),
            worker_id,
        }
    }
}

impl Drop for AutoVacuumHandle {
    fn drop(&mut self) {
        self.shutdown.signal();
        let Some(join) = self.join.take() else {
            return;
        };
        // Self-join guard: if the engine is being torn down *on the worker
        // thread itself* (an external drop that landed while this same thread
        // held the last strong ref mid-pass), joining would deadlock. The thread
        // is already unwinding to exit, so just return.
        if thread::current().id() == self.worker_id {
            return;
        }
        // Bounded join (M2.b): `std::thread::JoinHandle::join` has no timeout, so
        // run it on a throwaway watcher and bound *our* wait — a wedged vacuum
        // pass must not hang engine teardown forever.
        let (tx, rx) = std::sync::mpsc::channel();
        let _ = thread::Builder::new().spawn(move || {
            let _ = join.join();
            let _ = tx.send(());
        });
        if rx.recv_timeout(Duration::from_secs(5)).is_err() {
            tracing::warn!("autovacuum worker did not stop within 5s; detaching");
        }
    }
}

/// The launcher loop: sleep `naptime` (interruptible by shutdown), then check
/// the policy and vacuum if triggered. Exits when the engine is gone (`Weak`
/// fails to upgrade) or shutdown is signalled.
fn worker_loop(engine: Weak<Engine>, shutdown: Arc<Shutdown>) {
    tracing::debug!("autovacuum launcher started");
    loop {
        // Read the current naptime each cycle so `set_autovacuum_config` takes
        // effect on the next nap. Upgrade only briefly.
        let naptime = match engine.upgrade() {
            Some(e) => e.autovacuum_config().naptime,
            None => break, // engine dropped
        };

        // Interruptible sleep on the condvar.
        {
            let guard = shutdown.stop.lock().unwrap_or_else(|e| e.into_inner());
            if *guard {
                break;
            }
            let (guard, _) = shutdown
                .cv
                .wait_timeout(guard, naptime)
                .unwrap_or_else(|e| e.into_inner());
            if *guard {
                break;
            }
        }

        // Evaluate the policy and, if it fires, run one pass. Hold the strong
        // ref only for this critical section, then release it before sleeping
        // again so the worker is not the engine's owner while idle.
        let Some(e) = engine.upgrade() else {
            break;
        };
        if e.autovacuum_should_run() {
            match e.run_autovacuum_pass() {
                Ok(report) => tracing::debug!(
                    versions_reclaimed = report.versions_reclaimed,
                    horizon_blocked = report.horizon_blocked,
                    "autovacuum pass complete"
                ),
                Err(err) => tracing::warn!(error = %err, "autovacuum pass failed"),
            }
        }
        drop(e);
    }
    tracing::debug!("autovacuum launcher stopped");
}

/// Seconds since the Unix epoch (coarse; for the A4 `/metrics` timestamp).
pub(crate) fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Engine {
    /// Start the background autovacuum launcher (A3). Requires an `Arc<Engine>`
    /// because the worker holds a `Weak<Engine>` (see the module doc); a bare
    /// `Engine` from [`Engine::open`] has no background thread by construction —
    /// which is also what the deterministic tests want, and manual
    /// [`Engine::vacuum`] stays available everywhere.
    ///
    /// Idempotent and policy-gated: does nothing if a launcher is already
    /// running, or if the policy is disabled
    /// (`UNIDB_AUTOVACUUM_ENABLED=0`). The typical entry point is
    /// [`Engine::open_arc`], which opens + wraps + starts in one call.
    pub fn spawn_autovacuum(self: &Arc<Self>) {
        if !self.autovacuum_config().enabled {
            return;
        }
        let mut slot = self
            .autovacuum_handle
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if slot.is_some() {
            return;
        }
        *slot = Some(AutoVacuumHandle::spawn(self));
    }

    /// Whether the background launcher is currently running (A3/A4).
    pub fn autovacuum_running(&self) -> bool {
        self.autovacuum_handle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    /// Run one autovacuum pass and record its observability counters (A4). This
    /// is the background launcher's single call site into reclamation — it is
    /// exactly [`Engine::vacuum`] plus the run-count / last-run bookkeeping, so
    /// autovacuum reclaims through the same already-safe M10 path. Public so an
    /// operator (or a test) can force a counted pass without waiting on the
    /// launcher's naptime.
    pub fn run_autovacuum_pass(&self) -> crate::Result<crate::VacuumReport> {
        let report = self.vacuum()?;
        self.autovacuums_triggered.fetch_add(1, Ordering::Relaxed);
        self.last_autovacuum_epoch_secs
            .store(now_epoch_secs(), Ordering::Relaxed);
        Ok(report)
    }
}
