//! Stats-history ticker (item 34): a background `std::thread` that captures a
//! `StatsPoint` snapshot every 5 s into the engine's 300-point ring buffer,
//! enabling `GET /stats/history` to serve historical trend data without the
//! Studio accumulating state in `$state` that resets on every page reload.
//!
//! Uses the exact same lifecycle pattern as [`crate::autovacuum`]:
//!
//! - Holds a [`Weak<Engine>`] so the engine's `Drop` is never blocked.
//! - Interruptible `Condvar`-gated sleep so shutdown is immediate, not stuck
//!   waiting out a full 5 s tick.
//! - Bounded-join teardown: a wedged tick must not hang engine shutdown.
//! - Self-join guard for the case where the engine is dropped *on* the worker
//!   thread (the last strong ref was held mid-tick).
//!
//! The ticker is started only from [`crate::server::engine_handle::EngineHandle::spawn`],
//! so a bare [`crate::Engine::open`] (used by all deterministic tests) never
//! starts a background thread.

use std::sync::{Arc, Condvar, Mutex, Weak};
use std::thread::{self, JoinHandle, ThreadId};
use std::time::Duration;

use crate::Engine;

const TICK_INTERVAL: Duration = Duration::from_secs(5);

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

/// Owns the background stats-ticker thread. Stored as an [`Engine`] field so
/// that dropping the engine runs this `Drop` — the clean-shutdown hook.
pub(crate) struct StatsTickerHandle {
    shutdown: Arc<Shutdown>,
    join: Option<JoinHandle<()>>,
    worker_id: ThreadId,
}

impl StatsTickerHandle {
    pub(crate) fn spawn(engine: &Arc<Engine>) -> Self {
        let shutdown = Arc::new(Shutdown {
            stop: Mutex::new(false),
            cv: Condvar::new(),
        });
        let weak = Arc::downgrade(engine);
        let worker_shutdown = Arc::clone(&shutdown);
        let join = thread::Builder::new()
            .name("unidb-stats-ticker".into())
            .spawn(move || worker_loop(weak, worker_shutdown))
            .expect("failed to spawn stats ticker thread");
        let worker_id = join.thread().id();
        Self {
            shutdown,
            join: Some(join),
            worker_id,
        }
    }
}

impl Drop for StatsTickerHandle {
    fn drop(&mut self) {
        self.shutdown.signal();
        let Some(join) = self.join.take() else {
            return;
        };
        // Self-join guard: if the engine is being torn down *on the worker
        // thread itself*, joining would deadlock — just let the thread unwind.
        if thread::current().id() == self.worker_id {
            return;
        }
        // Bounded join: a wedged tick must not hang engine teardown forever.
        let (tx, rx) = std::sync::mpsc::channel();
        let _ = thread::Builder::new().spawn(move || {
            let _ = join.join();
            let _ = tx.send(());
        });
        if rx.recv_timeout(Duration::from_secs(5)).is_err() {
            tracing::warn!("stats ticker did not stop within 5s; detaching");
        }
    }
}

fn worker_loop(engine: Weak<Engine>, shutdown: Arc<Shutdown>) {
    tracing::debug!("stats ticker started");
    loop {
        {
            let guard = shutdown.stop.lock().unwrap_or_else(|e| e.into_inner());
            if *guard {
                break;
            }
            let (guard, _) = shutdown
                .cv
                .wait_timeout(guard, TICK_INTERVAL)
                .unwrap_or_else(|e| e.into_inner());
            if *guard {
                break;
            }
        }

        let Some(e) = engine.upgrade() else {
            break;
        };
        e.capture_stats_point();
        drop(e);
    }
    tracing::debug!("stats ticker stopped");
}

impl Engine {
    /// Spawn the background stats-history ticker (item 34). Mirrors
    /// [`Engine::spawn_autovacuum`]: requires an `Arc<Engine>` (the worker
    /// holds a `Weak`). Idempotent — a second call is a no-op.
    ///
    /// Only called from `EngineHandle::spawn` (server path) so bare
    /// `Engine::open()` handles (used by all deterministic tests) never
    /// start a background thread.
    pub fn spawn_stats_ticker(self: &Arc<Self>) {
        let mut slot = self
            .stats_ticker_handle
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if slot.is_some() {
            return;
        }
        *slot = Some(StatsTickerHandle::spawn(self));
    }
}
