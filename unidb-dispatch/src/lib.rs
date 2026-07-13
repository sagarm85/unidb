//! # unidb-dispatch — downstream event dispatcher (backlog item 20, Epic E2)
//!
//! A small **app-layer** service that consumes unidb's M4 WAL-derived event
//! stream from a **durable consumer offset** and fans it out to sinks:
//! webhooks (with retry + a dead-letter table dogfooded back into unidb),
//! in-process rooms (the primitive a studio WebSocket/SSE layer subscribes to),
//! and — deliberately not built — a Kafka bridge (E2c: "only if a real consumer
//! demands it"; none does, so it is a documented non-goal here, not dead code).
//!
//! ## What it does NOT do
//!
//! It adds **no engine surface**. It embeds `Arc<Engine>` and drives the
//! existing `enable_events` / `poll_events` / `ack_events` / `vacuum_events`
//! API — the same at-least-once, offset-durable contract M4 already ships.
//! `tokio`/`reqwest` live in *this* crate only, so the engine's default build
//! stays sync (CLAUDE.md invariant).
//!
//! ## Delivery semantics (the contract)
//!
//! **At-least-once.** A cycle is: `poll_events(consumer)` → for each event, fan
//! out to every matching subscription → **only then** `ack_events(consumer,
//! max_seq)`. The ack is a durable unidb write. So:
//!
//! - A crash *after* an event commits but *before* it is acked ⇒ the event is
//!   still in `__events__` (it was captured atomically in the triggering
//!   transaction's WAL append) and is redelivered after restart. **Zero loss.**
//! - A crash *between* delivery and ack ⇒ redelivery of an already-delivered
//!   event. Consumers must dedupe on `seq` (the monotonic offset). This is the
//!   standard Kafka manual-commit shape M4 chose on purpose.
//!
//! A failing webhook does not stall the pipeline: it is retried per the
//! [`RetryPolicy`], then written to the dead-letter table, and the offset still
//! advances — a poison event cannot wedge the stream.
//!
//! ## Slow-consumer vs vacuum horizon
//!
//! `vacuum_events` only reclaims events every registered consumer has acked
//! past (the M4 durability contract). A dispatcher that falls behind therefore
//! *pins retention*. [`CycleReport::backlogged`] flags when a poll came back
//! full (backlog ≥ `poll_limit`) and the loop logs a `WARN` — the "consumer too
//! far behind" signal the spec asks to surface loudly.

use std::future::Future;
use std::sync::{
    atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
    Arc,
};
use std::time::Duration;

use tracing::warn;
use unidb::{queue::Event, DbError, Engine, EventWake};

pub mod dlq;
pub mod filter;
pub mod sink;

pub use filter::Filter;
pub use sink::{CollectingSink, RoomSink, Sink, SinkError, WebhookSink};

/// How a failed sink delivery is retried before dead-lettering.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Total attempts, including the first. `1` ⇒ no retry.
    pub max_attempts: u32,
    /// Base backoff; attempt *k* waits `base * 2^(k-1)` (exponential).
    pub base_backoff: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base_backoff: Duration::from_millis(50),
        }
    }
}

/// One fan-out target plus the filter deciding which events reach it.
struct Subscription {
    filter: Filter,
    sink: Arc<dyn Sink>,
}

/// Live counters for observability and tests.
#[derive(Debug, Default)]
pub struct DispatchStats {
    /// Successful sink deliveries (counts each subscription separately).
    pub delivered: AtomicU64,
    /// Events written to the dead-letter table after exhausting retries.
    pub dead_lettered: AtomicU64,
    /// Highest offset durably acked so far (`-1` = nothing acked yet).
    pub last_acked_seq: AtomicI64,
    /// Poll→dispatch→ack cycles that returned at least one event.
    pub cycles: AtomicU64,
}

/// Outcome of a single [`Dispatcher::run_once`] cycle.
#[derive(Debug, Clone, Default)]
pub struct CycleReport {
    /// Events polled this cycle.
    pub polled: usize,
    /// Successful deliveries (summed across matching subscriptions).
    pub delivered: usize,
    /// Events dead-lettered this cycle.
    pub dead_lettered: usize,
    /// The offset acked at the end of the cycle, if any events were processed.
    pub acked_up_to: Option<i64>,
    /// The poll returned a full batch (backlog ≥ `poll_limit`) — the "consumer
    /// too far behind" signal.
    pub backlogged: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    #[error("engine error: {0}")]
    Engine(#[from] DbError),
    #[error("dispatcher blocking task failed to join")]
    Join,
}

/// Build a [`Dispatcher`]. Purely additive — no engine call happens until the
/// dispatcher actually runs, so `build` cannot fail.
pub struct DispatcherBuilder {
    engine: Arc<Engine>,
    consumer: String,
    subscriptions: Vec<Subscription>,
    poll_limit: usize,
    poll_interval: Duration,
    retry: RetryPolicy,
    dlq_table: String,
    lag_warn_threshold: usize,
    /// Q2 (item 26): optional push-notification handle. When set, `run` blocks
    /// on the condvar (with `poll_interval` as a timeout fallback) instead of
    /// sleeping on a fixed timer — idle dispatchers do zero polling work.
    event_wake: Option<Arc<EventWake>>,
}

impl DispatcherBuilder {
    /// Add a fan-out target with its filter/projection.
    pub fn subscribe(mut self, filter: Filter, sink: Arc<dyn Sink>) -> Self {
        self.subscriptions.push(Subscription { filter, sink });
        self
    }

    /// Max events fetched per poll. Also the backlog threshold behind
    /// [`CycleReport::backlogged`].
    pub fn poll_limit(mut self, limit: usize) -> Self {
        self.poll_limit = limit.max(1);
        self
    }

    /// Sleep between polls in [`Dispatcher::run`].
    pub fn poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    pub fn retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// Name of the dead-letter table (created on first dead-letter).
    pub fn dlq_table(mut self, table: impl Into<String>) -> Self {
        self.dlq_table = table.into();
        self
    }

    /// Log a WARN when a poll comes back with at least this many events.
    pub fn lag_warn_threshold(mut self, n: usize) -> Self {
        self.lag_warn_threshold = n;
        self
    }

    /// Q2 (item 26): enable push-notification wake. Pass `engine.event_wake()`
    /// so the dispatcher blocks on the condvar instead of sleeping on a timer —
    /// idle dispatchers do zero polling work, and commit→delivery latency drops.
    /// `poll_interval` remains the maximum wait duration (fallback/catch-up).
    pub fn event_wake(mut self, wake: Arc<EventWake>) -> Self {
        self.event_wake = Some(wake);
        self
    }

    pub fn build(self) -> Dispatcher {
        Dispatcher {
            engine: self.engine,
            consumer: self.consumer,
            subscriptions: self.subscriptions,
            poll_limit: self.poll_limit,
            poll_interval: self.poll_interval,
            retry: self.retry,
            dlq_table: self.dlq_table,
            lag_warn_threshold: self.lag_warn_threshold,
            event_wake: self.event_wake,
            stats: Arc::new(DispatchStats {
                last_acked_seq: AtomicI64::new(-1),
                ..Default::default()
            }),
            dlq_ensured: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// Consumes the event stream from a durable offset and fans it out.
pub struct Dispatcher {
    engine: Arc<Engine>,
    consumer: String,
    subscriptions: Vec<Subscription>,
    poll_limit: usize,
    poll_interval: Duration,
    retry: RetryPolicy,
    dlq_table: String,
    lag_warn_threshold: usize,
    event_wake: Option<Arc<EventWake>>,
    stats: Arc<DispatchStats>,
    dlq_ensured: Arc<AtomicBool>,
}

impl Dispatcher {
    pub fn builder(engine: Arc<Engine>, consumer: impl Into<String>) -> DispatcherBuilder {
        DispatcherBuilder {
            engine,
            consumer: consumer.into(),
            subscriptions: Vec::new(),
            poll_limit: 256,
            poll_interval: Duration::from_millis(200),
            retry: RetryPolicy::default(),
            dlq_table: "dispatch_dead_letter".to_string(),
            lag_warn_threshold: 0,
            event_wake: None,
        }
    }

    pub fn stats(&self) -> &Arc<DispatchStats> {
        &self.stats
    }

    pub fn consumer(&self) -> &str {
        &self.consumer
    }

    /// One poll → fan-out → ack cycle. Returns even when the batch is empty.
    /// Errors only on an engine-level failure (poll/ack/dead-letter write);
    /// a *sink* failure never fails the cycle — it is retried and dead-lettered.
    pub async fn run_once(&self) -> Result<CycleReport, DispatchError> {
        let batch = self.poll().await?;
        if batch.is_empty() {
            return Ok(CycleReport::default());
        }
        self.stats.cycles.fetch_add(1, Ordering::Relaxed);

        let mut report = CycleReport {
            polled: batch.len(),
            backlogged: batch.len() >= self.poll_limit,
            ..Default::default()
        };

        let mut max_seq: Option<i64> = None;
        for event in &batch {
            for sub in &self.subscriptions {
                if !sub.filter.matches(event) {
                    continue;
                }
                let projected = sub.filter.apply(event);
                let delivered = self.deliver_with_retry(sub, &projected).await?;
                if delivered {
                    report.delivered += 1;
                } else {
                    report.dead_lettered += 1;
                }
            }
            max_seq = Some(max_seq.map_or(event.seq, |m| m.max(event.seq)));
        }

        if let Some(seq) = max_seq {
            self.ack(seq).await?;
            self.stats.last_acked_seq.store(seq, Ordering::Relaxed);
            report.acked_up_to = Some(seq);
        }

        if report.backlogged
            || (self.lag_warn_threshold > 0 && report.polled >= self.lag_warn_threshold)
        {
            warn!(
                consumer = %self.consumer,
                polled = report.polled,
                poll_limit = self.poll_limit,
                "dispatch consumer is behind: a full/large poll batch pins the \
                 vacuum horizon (unvacuumable events accumulate until this \
                 consumer acks past them)"
            );
        }

        Ok(report)
    }

    /// Drive [`run_once`](Self::run_once) until `shutdown` resolves.
    ///
    /// **Q2 (item 26) push mode** — if an [`EventWake`] handle was wired via
    /// [`DispatcherBuilder::event_wake`], the loop blocks on the engine's
    /// commit condvar instead of sleeping on a fixed timer. The condvar wait
    /// uses `poll_interval` as a maximum timeout (the fallback/catch-up path),
    /// so the fallback poll still fires even when no commit arrives.
    ///
    /// **Poll-only mode** — without an `EventWake` handle the behaviour is
    /// unchanged from the pre-Q2 dispatcher: one `run_once` every
    /// `poll_interval`. Existing callers that don't call `.event_wake()` are
    /// unaffected.
    pub async fn run(&self, shutdown: impl Future<Output = ()>) {
        tokio::pin!(shutdown);
        match &self.event_wake {
            None => {
                // Poll-only path: unchanged from pre-Q2.
                loop {
                    tokio::select! {
                        _ = &mut shutdown => break,
                        _ = tokio::time::sleep(self.poll_interval) => {
                            if let Err(e) = self.run_once().await {
                                warn!(consumer = %self.consumer, error = %e, "dispatch cycle failed; retrying next tick");
                            }
                        }
                    }
                }
            }
            Some(wake) => {
                // Q2 push-notification path: block on condvar, run on wake.
                // `poll_interval` is the timeout so we fall back to polling
                // even when no commit arrives (catch-up / idle correctness).
                let mut known_gen = wake.generation();
                loop {
                    let wake_clone = wake.clone();
                    let timeout = self.poll_interval;
                    // Wrap the blocking condvar wait in spawn_blocking so the
                    // tokio reactor stays free during the wait.
                    let next_gen = tokio::select! {
                        _ = &mut shutdown => break,
                        g = tokio::task::spawn_blocking(move || {
                            wake_clone.wait_blocking(known_gen, timeout)
                        }) => g.unwrap_or(known_gen),
                    };
                    known_gen = next_gen;
                    if let Err(e) = self.run_once().await {
                        warn!(consumer = %self.consumer, error = %e, "dispatch cycle failed; retrying next wake");
                    }
                }
            }
        }
    }

    /// Try one delivery with retry+backoff. Returns `Ok(true)` on success,
    /// `Ok(false)` if the event was dead-lettered after exhausting retries.
    /// Only an engine-level dead-letter *write* failure surfaces as `Err`.
    async fn deliver_with_retry(
        &self,
        sub: &Subscription,
        event: &Event,
    ) -> Result<bool, DispatchError> {
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            match sub.sink.deliver(event).await {
                Ok(()) => {
                    self.stats.delivered.fetch_add(1, Ordering::Relaxed);
                    return Ok(true);
                }
                Err(err) => {
                    if attempt >= self.retry.max_attempts {
                        self.dead_letter(sub.sink.name(), event, attempt as i64, &err)
                            .await?;
                        self.stats.dead_lettered.fetch_add(1, Ordering::Relaxed);
                        warn!(
                            sink = sub.sink.name(),
                            seq = event.seq,
                            attempts = attempt,
                            error = %err,
                            "delivery exhausted retries; dead-lettered"
                        );
                        return Ok(false);
                    }
                    let backoff = self.retry.base_backoff * 2u32.saturating_pow(attempt - 1);
                    tokio::time::sleep(backoff).await;
                }
            }
        }
    }

    // ── engine calls (each on the blocking pool; each its own transaction) ──

    async fn poll(&self) -> Result<Vec<Event>, DispatchError> {
        let engine = self.engine.clone();
        let consumer = self.consumer.clone();
        let limit = self.poll_limit;
        spawn(move || {
            let xid = engine.begin()?;
            let out = engine.poll_events(xid, &consumer, limit);
            // A read-only poll: commit to release the snapshot regardless.
            let _ = engine.commit(xid);
            out
        })
        .await
    }

    async fn ack(&self, up_to_seq: i64) -> Result<(), DispatchError> {
        let engine = self.engine.clone();
        let consumer = self.consumer.clone();
        spawn(move || {
            let xid = engine.begin()?;
            match engine.ack_events(xid, &consumer, up_to_seq) {
                Ok(()) => engine.commit(xid),
                Err(e) => {
                    let _ = engine.abort(xid);
                    Err(e)
                }
            }
        })
        .await
    }

    async fn ensure_dlq(&self) -> Result<(), DispatchError> {
        if self.dlq_ensured.load(Ordering::Acquire) {
            return Ok(());
        }
        let engine = self.engine.clone();
        let table = self.dlq_table.clone();
        spawn(move || {
            let xid = engine.begin()?;
            match dlq::ensure_dlq_table(&engine, &table, xid) {
                Ok(()) => engine.commit(xid),
                Err(e) => {
                    let _ = engine.abort(xid);
                    Err(e)
                }
            }
        })
        .await?;
        self.dlq_ensured.store(true, Ordering::Release);
        Ok(())
    }

    async fn dead_letter(
        &self,
        sink_name: &str,
        event: &Event,
        attempts: i64,
        err: &SinkError,
    ) -> Result<(), DispatchError> {
        // DDL and DML are separate transactions (DDL is not composed inside a
        // data transaction) — ensure the table first, then insert the row.
        self.ensure_dlq().await?;
        let engine = self.engine.clone();
        let table = self.dlq_table.clone();
        let sink_name = sink_name.to_string();
        let error = err.to_string();
        let seq = event.seq;
        let ev_xid = event.xid;
        let table_name = event.table_name.clone();
        let op = event.op.clone();
        let payload = event.payload.clone();
        spawn(move || {
            let xid = engine.begin()?;
            let dl = dlq::DeadLetter {
                seq,
                xid: ev_xid,
                table_name: &table_name,
                op: &op,
                sink: &sink_name,
                attempts,
                error: &error,
                payload: &payload,
            };
            match dlq::insert_dead_letter(&engine, &table, xid, &dl) {
                Ok(()) => engine.commit(xid),
                Err(e) => {
                    let _ = engine.abort(xid);
                    Err(e)
                }
            }
        })
        .await
    }
}

/// Run one blocking `Engine` call on the tokio blocking pool, mapping a join
/// failure to [`DispatchError::Join`] — the same choke-point pattern the server
/// uses (`server::engine_handle`).
async fn spawn<T, F>(f: F) -> Result<T, DispatchError>
where
    F: FnOnce() -> Result<T, DbError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| DispatchError::Join)?
        .map_err(DispatchError::from)
}
