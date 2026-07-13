//! # unidb-logical — logical replication (backlog item 28, R2)
//!
//! Consumes the item-26 event stream from a **primary** engine and applies
//! row-level changes (`INSERT`/`UPDATE`/`DELETE`) to a **target** engine,
//! implementing table-subset logical replication with at-least-once delivery.
//!
//! ## Architecture
//!
//! This crate wraps [`unidb_dispatch::Dispatcher`] (item 20) with a
//! [`LogicalApplySink`] that translates each [`unidb::queue::Event`] into SQL
//! and executes it against the target engine. At-least-once delivery,
//! offset-durable consumer positions, and retry/DLQ are all inherited from the
//! Dispatcher — no reinvention.
//!
//! ## Event payload sufficiency
//!
//! - `INSERT` events carry the full new row image → directly applied.
//! - `DELETE` events carry the full old row image → `DELETE WHERE key = …`.
//! - `UPDATE` events carry the **new** row image only (the old key is not
//!   captured). Logical apply reconstructs via `DELETE WHERE key = new_key` +
//!   `INSERT new_row`. Correct when the key column is immutable (standard practice).
//!   **Known gap:** if the key column itself is updated, the old row won't be found.
//!   Filed as an item-26 follow-up: capturing `(old_key, new_row)` in UPDATE events
//!   resolves this without a WAL-format change.
//!
//! ## Requirements
//!
//! - Target schema must be pre-created before replication starts (no DDL).
//! - Each replicated table needs a [`TableSpec`] with a `key_column` for
//!   `UPDATE`/`DELETE` to work. INSERTs are applied without a key.
//! - Events for tables not in `tables` are silently skipped.
//!
//! ## Delivery semantics
//!
//! At-least-once. Consumer offset is durably stored in `__consumers__` on the
//! primary. After a primary restart, the replicator resumes from the last acked
//! offset — no committed change is lost, but the most-recently-delivered batch may
//! be re-applied (idempotency on the target is the caller's responsibility).

use std::{collections::HashMap, future::Future, sync::Arc};

use tracing::{debug, warn};
use unidb::{queue::Event, Engine};
use unidb_dispatch::{
    sink::{Sink, SinkError},
    CycleReport, DispatchError, DispatchStats, Dispatcher, Filter, RetryPolicy,
};

mod apply;
pub use apply::build_apply_sql;

/// Per-table replication specification.
pub struct TableSpec {
    /// Table name on the primary (must exist with the same name on the target).
    pub table: String,
    /// Column used to identify rows for `UPDATE`/`DELETE`. Must be unique (or
    /// primary-key-level) on the target; the replicator does not enforce this.
    pub key_column: String,
}

/// Builder for [`LogicalReplicator`].
pub struct LogicalReplicatorBuilder {
    primary: Arc<Engine>,
    target: Arc<Engine>,
    consumer: String,
    tables: Vec<TableSpec>,
    poll_limit: usize,
    retry: RetryPolicy,
}

impl LogicalReplicatorBuilder {
    pub fn poll_limit(mut self, limit: usize) -> Self {
        self.poll_limit = limit;
        self
    }

    pub fn retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    pub fn build(self) -> LogicalReplicator {
        let table_map: HashMap<String, TableSpec> = self
            .tables
            .into_iter()
            .map(|s| (s.table.clone(), s))
            .collect();

        let sink = Arc::new(LogicalApplySink {
            target: self.target,
            tables: table_map,
        });

        let dispatcher = Dispatcher::builder(self.primary, self.consumer)
            .subscribe(Filter::all(), sink)
            .poll_limit(self.poll_limit)
            .retry(self.retry)
            .build();

        LogicalReplicator { dispatcher }
    }
}

/// Drives logical replication from a primary to a target engine.
///
/// Wraps [`unidb_dispatch::Dispatcher`] with a [`LogicalApplySink`] that
/// translates events to SQL and applies them to the target engine.
pub struct LogicalReplicator {
    dispatcher: Dispatcher,
}

impl LogicalReplicator {
    /// Create a builder.
    ///
    /// `consumer_name` must be unique per replication target — it is the key
    /// used to persist the acked offset in `__consumers__` on the primary.
    pub fn builder(
        primary: Arc<Engine>,
        target: Arc<Engine>,
        consumer_name: impl Into<String>,
        tables: Vec<TableSpec>,
    ) -> LogicalReplicatorBuilder {
        LogicalReplicatorBuilder {
            primary,
            target,
            consumer: consumer_name.into(),
            tables,
            poll_limit: 256,
            retry: RetryPolicy::default(),
        }
    }

    /// One poll → apply → ack cycle. Returns even when the batch is empty.
    pub async fn run_once(&self) -> Result<CycleReport, DispatchError> {
        self.dispatcher.run_once().await
    }

    /// Drive `run_once` until `shutdown` resolves.
    pub async fn run(&self, shutdown: impl Future<Output = ()>) {
        self.dispatcher.run(shutdown).await
    }

    pub fn stats(&self) -> &Arc<DispatchStats> {
        self.dispatcher.stats()
    }
}

// ── LogicalApplySink ─────────────────────────────────────────────────────────

/// Translates each event into SQL and executes it against the target engine.
struct LogicalApplySink {
    target: Arc<Engine>,
    /// Map from table name to its replication spec.
    tables: HashMap<String, TableSpec>,
}

#[async_trait::async_trait]
impl Sink for LogicalApplySink {
    fn name(&self) -> &str {
        "logical-apply"
    }

    async fn deliver(&self, event: &Event) -> Result<(), SinkError> {
        let spec = match self.tables.get(&event.table_name) {
            Some(s) => s,
            None => {
                debug!(
                    table = %event.table_name,
                    "logical: table not in replication scope, skipping"
                );
                return Ok(());
            }
        };

        let statements = build_apply_sql(event, spec)
            .map_err(|e| SinkError::new(format!("build apply SQL: {e}")))?;

        if statements.is_empty() {
            return Ok(());
        }

        let target = self.target.clone();
        tokio::task::spawn_blocking(move || {
            let xid = target
                .begin()
                .map_err(|e| SinkError::new(format!("begin txn: {e}")))?;

            for sql in &statements {
                if let Err(e) = target.execute_sql(xid, sql) {
                    let _ = target.abort(xid);
                    warn!(sql = %sql, error = %e, "logical apply: statement failed");
                    return Err(SinkError::new(format!("execute: {e}")));
                }
            }
            target
                .commit(xid)
                .map_err(|e| SinkError::new(format!("commit: {e}")))
        })
        .await
        .map_err(|_| SinkError::new("spawn_blocking join failed"))?
    }
}
