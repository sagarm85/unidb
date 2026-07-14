//! The concurrent-writer bridge (P5.e-3). Since `Engine` is now `Send + Sync`
//! (P5.e-2), the server no longer funnels every write through one dedicated OS
//! thread. Instead `EngineHandle` holds an `Arc<Engine>` shared by **all**
//! request handlers, and each async method runs its (blocking) `Engine` call on
//! a tokio blocking-pool thread via [`tokio::task::spawn_blocking`]. Many
//! writers therefore execute in parallel across cores, coordinating only
//! through the engine's internal latches/locks (buffer-pool page latches, the
//! WAL append mutex, the row lock manager, MVCC snapshots).
//!
//! Durability under concurrency is handled by **group commit** inside the
//! engine: the handle opens the engine in deferred-sync mode (per-statement
//! mini-txn fsyncs are skipped), and `Engine::commit` forces the transaction's
//! commit record durable via `Wal::sync_up_to`, which coalesces concurrent
//! committers behind a single fsync (the leader-election barrier — see
//! `wal.rs`). So the more transactions commit at once, the fewer fsyncs they
//! collectively pay, and write throughput scales with concurrent writers rather
//! than hitting the old single-writer-thread ceiling.
//!
//! `EngineHandle::spawn` still opens the `Engine` **synchronously, on the
//! caller's thread**, so an `Engine::open` failure (corrupt control file, bad
//! WAL, etc.) surfaces immediately as `Result::Err` — exactly like every other
//! `Engine::open` call site — rather than being discovered by the first request.

use std::path::Path;
use std::sync::Arc;

use crate::{
    catalog::{IndexKind, IndexStatus},
    error::{DbError, Result},
    format::Xid,
    graph::edges::Edge,
    heap::RowId,
    queue::Event,
    read_handle::ReadHandle,
    sql::executor::ExecResult,
    txn::IsolationLevel,
    Engine,
};

pub struct EngineHandle {
    /// The one shared, `Sync` engine every handler drives concurrently. `None`
    /// only after [`shutdown`](EngineHandle::shutdown) — post-shutdown calls
    /// then fail cleanly with [`DbError::EngineUnavailable`] instead of panicking.
    engine: Option<Arc<Engine>>,
    /// Concurrent read path (6b): reads bypass the engine's write coordination
    /// and run on this `Send + Sync` handle over shared state, so many readers
    /// execute in parallel with each other and with writers.
    read: ReadHandle,
}

impl EngineHandle {
    /// Open `Engine` on the calling thread (surfacing any open/recovery error
    /// immediately), enable group-commit deferral, and share it via `Arc`.
    pub fn spawn(dir: &Path, page_size: u32) -> Result<Self> {
        let engine = Engine::open(dir, page_size)?;
        // Group-committed force-log-at-commit is now the engine default (C1):
        // `Engine::open` defers per-statement fsyncs and `Engine::commit` forces
        // durability via the coalescing `Wal::sync_up_to` barrier. No explicit
        // `set_deferred_sync` call is needed here anymore.
        let read = engine.read_handle();
        let engine = Arc::new(engine);
        // A3: start the background autovacuum launcher for the served instance
        // (default-on, policy-gated). The worker holds a `Weak<Engine>`, so this
        // Arc's eventual drop still tears the engine down cleanly.
        engine.spawn_autovacuum();
        Ok(Self {
            engine: Some(engine),
            read,
        })
    }

    /// Clone the shared engine `Arc` for a blocking task, or fail if the handle
    /// has been shut down.
    fn engine(&self) -> Result<Arc<Engine>> {
        self.engine
            .as_ref()
            .cloned()
            .ok_or(DbError::EngineUnavailable)
    }

    /// Expose the shared engine `Arc` for app-layer services (e.g.
    /// `unidb-storage` in item 31) that need the same `Arc<Engine>` instance.
    /// Returns `Err(EngineUnavailable)` after shutdown.
    pub fn engine_arc(&self) -> Result<Arc<Engine>> {
        self.engine()
    }

    /// Run one blocking `Engine` call on a tokio blocking-pool thread. This is
    /// the single choke point that turns every synchronous `Engine` method into
    /// a concurrency-safe async one; N of these run in parallel across the pool.
    async fn on_engine<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Engine) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let engine = self.engine()?;
        // Carry the request's correlation id onto the blocking pool thread so
        // engine-core logging (slow-query, audit) can tag it (item 22, L2).
        let request_id = crate::server::correlation::current_request_id();
        tokio::task::spawn_blocking(move || {
            let _corr = crate::observability::set_request_id(request_id);
            f(&engine)
        })
        .await
        .map_err(|_| DbError::EngineUnavailable)?
    }

    /// Read one row by [`RowId`] on the concurrent read path (6b): no xid, no
    /// WAL. Runs on a blocking pool thread since the read briefly locks shared
    /// state.
    pub async fn get_row(&self, row_id: RowId) -> Result<Vec<u8>> {
        let read = self.read.clone();
        let request_id = crate::server::correlation::current_request_id();
        tokio::task::spawn_blocking(move || {
            let _corr = crate::observability::set_request_id(request_id);
            read.get(row_id)
        })
        .await
        .map_err(|_| DbError::EngineUnavailable)?
    }

    /// Execute read-only SQL (`SELECT`) on the concurrent read path (6b). The
    /// caller must have classified the SQL as concurrent-readable (see
    /// [`crate::read_handle::is_concurrent_read_sql`]); a non-read statement
    /// returns [`DbError::SqlPlan`].
    pub async fn execute_sql_read(&self, sql: String) -> Result<Vec<ExecResult>> {
        let read = self.read.clone();
        let request_id = crate::server::correlation::current_request_id();
        tokio::task::spawn_blocking(move || {
            let _corr = crate::observability::set_request_id(request_id);
            read.execute_sql(&sql)
        })
        .await
        .map_err(|_| DbError::EngineUnavailable)?
    }

    pub async fn begin(&self, isolation: Option<IsolationLevel>) -> Result<Xid> {
        self.on_engine(move |e| match isolation {
            Some(iso) => e.begin_with_isolation(iso),
            None => e.begin(),
        })
        .await
    }

    pub async fn commit(&self, xid: Xid) -> Result<()> {
        self.on_engine(move |e| e.commit(xid)).await
    }

    pub async fn abort(&self, xid: Xid) -> Result<()> {
        self.on_engine(move |e| e.abort(xid)).await
    }

    pub async fn insert(&self, xid: Xid, data: Vec<u8>) -> Result<RowId> {
        self.on_engine(move |e| e.insert(xid, &data)).await
    }

    pub async fn get(&self, xid: Xid, row_id: RowId) -> Result<Vec<u8>> {
        self.on_engine(move |e| e.get(xid, row_id)).await
    }

    pub async fn update(&self, xid: Xid, row_id: RowId, new_data: Vec<u8>) -> Result<RowId> {
        self.on_engine(move |e| e.update(xid, row_id, &new_data))
            .await
    }

    pub async fn delete(&self, xid: Xid, row_id: RowId) -> Result<()> {
        self.on_engine(move |e| e.delete(xid, row_id)).await
    }

    pub async fn execute_sql(&self, xid: Xid, sql: String) -> Result<Vec<ExecResult>> {
        self.on_engine(move |e| e.execute_sql(xid, &sql)).await
    }

    pub async fn execute_sql_params(
        &self,
        xid: Xid,
        sql: String,
        params: Vec<crate::sql::logical::Literal>,
    ) -> Result<Vec<ExecResult>> {
        self.on_engine(move |e| e.execute_sql_params(xid, &sql, &params))
            .await
    }

    pub async fn execute_cypher(&self, xid: Xid, query: String) -> Result<Vec<ExecResult>> {
        self.on_engine(move |e| e.execute_cypher(xid, &query)).await
    }

    /// Execute SQL as a named user (P6.e), enforcing privileges + handling auth
    /// DDL. `user == None` is the embedded superuser.
    pub async fn execute_sql_as(
        &self,
        user: Option<String>,
        xid: Xid,
        sql: String,
    ) -> Result<Vec<ExecResult>> {
        self.on_engine(move |e| e.execute_sql_as(user.as_deref(), xid, &sql))
            .await
    }

    /// Privilege pre-check for the read/param fast paths (P6.e).
    pub async fn authorize_sql(&self, user: Option<String>, sql: String) -> Result<()> {
        self.on_engine(move |e| e.authorize_sql(user.as_deref(), &sql))
            .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create_edge(
        &self,
        xid: Xid,
        from_id: i64,
        to_id: i64,
        edge_type: String,
        props: String,
    ) -> Result<RowId> {
        self.on_engine(move |e| e.create_edge(xid, from_id, to_id, &edge_type, &props))
            .await
    }

    pub async fn delete_edge(&self, xid: Xid, row_id: RowId, from_id: i64) -> Result<()> {
        self.on_engine(move |e| e.delete_edge(xid, row_id, from_id))
            .await
    }

    pub async fn edges_from(&self, xid: Xid, from_id: i64) -> Result<Vec<Edge>> {
        self.on_engine(move |e| e.edges_from(xid, from_id)).await
    }

    pub async fn enable_events(&self, table: String) -> Result<()> {
        self.on_engine(move |e| e.enable_events(&table)).await
    }

    pub async fn is_events_enabled(&self, table: String) -> Result<bool> {
        self.on_engine(move |e| e.is_events_enabled(&table)).await
    }

    pub async fn disable_events(&self, table: String) -> Result<()> {
        self.on_engine(move |e| e.disable_events(&table)).await
    }

    pub async fn events_head_seq(&self) -> Result<i64> {
        self.on_engine(|e| e.events_head_seq()).await
    }

    pub async fn poll_events(
        &self,
        xid: Xid,
        consumer: String,
        limit: usize,
    ) -> Result<Vec<Event>> {
        self.on_engine(move |e| e.poll_events(xid, &consumer, limit))
            .await
    }

    /// E1 ephemeral live-tail cursor (item 20): events past `after_seq`, no
    /// durable consumer touched. Backs `Last-Event-ID`/`from_seq` resume.
    pub async fn poll_events_after(
        &self,
        xid: Xid,
        after_seq: i64,
        limit: usize,
    ) -> Result<Vec<Event>> {
        self.on_engine(move |e| e.poll_events_after(xid, after_seq, limit))
            .await
    }

    /// Q2 (item 26): current commit generation — callers snapshot this before
    /// processing a batch so they can detect the NEXT commit even if it fires
    /// before the `wait_event_commit` call.
    pub fn event_commit_gen(&self) -> u64 {
        self.engine
            .as_ref()
            .map(|e| e.event_commit_gen())
            .unwrap_or(0)
    }

    /// Q2 (item 26): block (on a spawn_blocking thread) until a new commit
    /// occurs or `timeout` elapses, then return the new generation. Use this
    /// instead of a fixed sleep to reduce latency and CPU on idle streams.
    pub async fn wait_event_commit(&self, known_gen: u64, timeout: std::time::Duration) -> u64 {
        let Ok(engine) = self.engine() else {
            return known_gen;
        };
        tokio::task::spawn_blocking(move || engine.wait_event_commit_blocking(known_gen, timeout))
            .await
            .unwrap_or(known_gen)
    }

    pub async fn ack_events(&self, xid: Xid, consumer: String, up_to_seq: i64) -> Result<()> {
        self.on_engine(move |e| e.ack_events(xid, &consumer, up_to_seq))
            .await
    }

    pub async fn vacuum_events(&self, xid: Xid) -> Result<usize> {
        self.on_engine(move |e| e.vacuum_events(xid)).await
    }

    pub async fn set_column_index(
        &self,
        table: String,
        column: String,
        kind: Option<IndexKind>,
    ) -> Result<()> {
        self.on_engine(move |e| e.set_column_index(&table, &column, kind))
            .await
    }

    pub async fn index_status(&self, table: String, column: String) -> Option<IndexStatus> {
        let Ok(engine) = self.engine() else {
            return None;
        };
        tokio::task::spawn_blocking(move || engine.index_status(&table, &column))
            .await
            .unwrap_or(None)
    }

    pub async fn checkpoint(&self) -> Result<()> {
        self.on_engine(|e| e.checkpoint()).await
    }

    /// Superuser gate for admin routes (R3): `Ok` for the implicit superuser
    /// (no `sub`), a named `SUPERUSER`, or open/bootstrap mode.
    pub async fn ensure_superuser(&self, user: Option<String>) -> Result<()> {
        self.on_engine(move |e| e.ensure_superuser(user.as_deref()))
            .await
    }

    /// Install an RLS policy from a SQL predicate string (R3).
    pub async fn set_rls_policy_sql(&self, table: String, predicate: String) -> Result<()> {
        self.on_engine(move |e| e.set_rls_policy_sql(&table, &predicate))
            .await
    }

    /// `POST /admin/flush` (R3): force the WAL durable, then flush every
    /// dirty page. The WAL sync first keeps D5 satisfiable for pages whose
    /// records were deferred by group commit.
    pub async fn flush(&self) -> Result<()> {
        self.on_engine(|e| {
            e.sync_wal()?;
            e.flush()
        })
        .await
    }

    /// Snapshot every table's schema for `GET /tables` introspection (S1).
    pub async fn table_defs(&self) -> Result<Vec<crate::catalog::TableDef>> {
        self.on_engine(|e| Ok(e.table_defs())).await
    }

    /// A `pg_stat_*`-style activity + counter snapshot (P6.g).
    pub async fn stats(&self) -> Result<crate::EngineStats> {
        self.on_engine(|e| Ok(e.stats())).await
    }

    // ── Replication slots + WAL shipping (P6.b) ────────────────────────────────

    pub async fn create_replication_slot(
        &self,
        name: String,
        kind: crate::replication::SlotKind,
    ) -> Result<crate::replication::SlotInfo> {
        self.on_engine(move |e| e.create_replication_slot(&name, kind))
            .await
    }

    pub async fn drop_replication_slot(&self, name: String) -> Result<()> {
        self.on_engine(move |e| e.drop_replication_slot(&name))
            .await
    }

    pub async fn advance_replication_slot(&self, name: String, lsn: u64) -> Result<()> {
        self.on_engine(move |e| e.advance_replication_slot(&name, lsn))
            .await
    }

    pub async fn replication_slots(&self) -> Result<Vec<crate::replication::SlotInfo>> {
        self.on_engine(|e| Ok(e.replication_slots())).await
    }

    /// Ship the WAL record stream after `from_lsn` as framed bytes (P6.b), for a
    /// replica to decode + apply. Returns the primary's current tail LSN too, so
    /// the caller knows where the batch ends without decoding it.
    pub async fn ship_wal(&self, from_lsn: u64) -> Result<(u64, Vec<u8>)> {
        self.on_engine(move |e| Ok((e.wal_current_lsn(), e.ship_wal(from_lsn)?)))
            .await
    }

    /// Bulk-insert `rows` into `table` in one transaction (item 32).
    ///
    /// Builds a parameterized `INSERT INTO {table} ({cols}) VALUES ($1, …)`
    /// once, then loops `execute_prepared` for each row, and commits once.
    /// Returns the count of inserted rows. On any engine error the transaction
    /// is aborted — the entire batch is atomic (all-or-nothing).
    ///
    /// The column names and rows are validated by the caller (`bulk.rs`) before
    /// this is called, so identifiers here are already safe to interpolate.
    pub async fn bulk_insert(
        &self,
        table: String,
        columns: Vec<String>,
        rows: Vec<Vec<crate::sql::logical::Literal>>,
    ) -> Result<u64> {
        if rows.is_empty() {
            return Ok(0);
        }
        self.on_engine(move |engine| {
            let placeholders = (1..=columns.len())
                .map(|i| format!("${i}"))
                .collect::<Vec<_>>()
                .join(", ");
            let col_list = columns.join(", ");
            let sql = format!("INSERT INTO {table} ({col_list}) VALUES ({placeholders})");
            let prepared = engine.prepare(&sql)?;
            let xid = engine.begin()?;
            let mut inserted = 0u64;
            let result: Result<()> = (|| {
                for params in &rows {
                    engine.execute_prepared(xid, &prepared, params)?;
                    inserted += 1;
                }
                Ok(())
            })();
            match result {
                Ok(()) => {
                    engine.commit(xid)?;
                    Ok(inserted)
                }
                Err(e) => {
                    let _ = engine.abort(xid);
                    Err(e)
                }
            }
        })
        .await
    }

    /// Release the shared engine. Every write already made itself durable at
    /// commit (group commit forces the WAL fsync before `commit` returns), so
    /// there is nothing to flush here; dropping the last `Arc<Engine>` closes
    /// its files. Idempotent — a second call is a harmless no-op.
    pub fn shutdown(&mut self) {
        // Dropping the `Arc` releases the engine once no in-flight blocking task
        // still holds a clone. Belt-and-suspenders: force any deferred WAL bytes
        // durable first, in case a non-commit write path deferred a flush.
        if let Some(engine) = self.engine.take() {
            let _ = engine.sync_wal();
        }
    }
}

impl Drop for EngineHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tempfile::tempdir;

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn engine_is_send_sync() {
        assert_send_sync::<Engine>();
    }

    #[tokio::test]
    async fn round_trips_begin_insert_commit_get() {
        let dir = tempdir().unwrap();
        let handle = EngineHandle::spawn(dir.path(), 0).unwrap();

        let xid = handle.begin(None).await.unwrap();
        let row_id = handle.insert(xid, b"hello".to_vec()).await.unwrap();
        handle.commit(xid).await.unwrap();

        let xid2 = handle.begin(None).await.unwrap();
        let data = handle.get(xid2, row_id).await.unwrap();
        assert_eq!(data, b"hello");
    }

    #[tokio::test]
    async fn shutdown_releases_engine_and_is_idempotent() {
        let dir = tempdir().unwrap();
        let mut handle = EngineHandle::spawn(dir.path(), 0).unwrap();

        let start = std::time::Instant::now();
        handle.shutdown();
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "shutdown must return well within its bound"
        );
        handle.shutdown(); // second call must be a harmless no-op

        // Post-shutdown calls fail cleanly rather than panicking.
        assert!(matches!(
            handle.begin(None).await,
            Err(DbError::EngineUnavailable)
        ));

        // A fresh `Engine::open` against the same directory must succeed.
        Engine::open(dir.path(), 0).unwrap();
    }

    /// Many concurrent writers on the shared engine: no lost updates, no torn
    /// state, no deadlock hang. Each of N tasks inserts its own row in its own
    /// transaction; afterwards every row must be readable and distinct.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_writers_all_commit() {
        let dir = tempdir().unwrap();
        let handle = Arc::new(EngineHandle::spawn(dir.path(), 0).unwrap());

        let n = 200u32;
        let mut tasks = Vec::new();
        for i in 0..n {
            let h = handle.clone();
            tasks.push(tokio::spawn(async move {
                let xid = h.begin(None).await.unwrap();
                let rid = h
                    .insert(xid, format!("row-{i}").into_bytes())
                    .await
                    .unwrap();
                h.commit(xid).await.unwrap();
                rid
            }));
        }
        let mut rids = Vec::new();
        for t in tasks {
            rids.push(t.await.unwrap());
        }
        assert_eq!(rids.len() as u32, n);

        // Every committed row is durable and readable with its own contents.
        let xid = handle.begin(None).await.unwrap();
        for (i, rid) in rids.iter().enumerate() {
            let data = handle.get(xid, *rid).await.unwrap();
            assert_eq!(data, format!("row-{i}").into_bytes());
        }
        handle.commit(xid).await.unwrap();
    }
}
