//! The writer-thread bridge (M5.a): one dedicated OS thread owns the
//! `Engine` for its entire life. Async callers (HTTP handlers, M5.b+) never
//! touch `Engine` directly — they call one of `EngineHandle`'s typed async
//! methods, which builds an [`EngineRequest`], sends it over an unbounded
//! `tokio::sync::mpsc` channel (a plain, non-blocking, synchronous `send`
//! — no `.await` needed for the send half, only for awaiting the reply),
//! and awaits a per-request `tokio::sync::oneshot` reply. The writer thread
//! itself never runs inside a tokio runtime — it loops on
//! `Receiver::blocking_recv()`, calling straight into `Engine`'s ordinary
//! synchronous API and replying on each request's oneshot sender. This
//! mirrors `index_worker.rs`'s spawn/channel/bounded-shutdown shape
//! exactly, generalized from "one background thread owning secondary
//! indexes" to "one background thread owning the whole `Engine`".
//!
//! `EngineHandle::spawn` opens the `Engine` **synchronously, on the
//! caller's thread**, before spawning the writer thread — a deliberate
//! choice, not an oversight: it lets an `Engine::open` failure (corrupt
//! control file, bad WAL, etc.) surface immediately as a `Result::Err`
//! from `spawn` itself, exactly like every other `Engine::open` call site
//! in this codebase, rather than requiring the first queued request to
//! somehow learn about a startup failure through the wrong channel. This
//! one-time synchronous cost happens once, at process startup, before any
//! request is served — a fundamentally different concern from blocking a
//! per-request async handler, which is the actual problem this bridge
//! solves.

use std::path::Path;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use crate::{
    catalog::IndexKind,
    error::{DbError, Result},
    format::Xid,
    graph::edges::Edge,
    heap::RowId,
    index_worker::IndexStatus,
    queue::Event,
    read_handle::ReadHandle,
    sql::executor::ExecResult,
    txn::IsolationLevel,
    Engine,
};

/// One variant per `Engine` method-category. Every variant except
/// `Shutdown` carries a `oneshot::Sender` for its reply, so the writer
/// thread's loop is a straight "receive, call, reply" dispatch with no
/// separate response-routing table needed.
pub enum EngineRequest {
    Begin {
        isolation: Option<IsolationLevel>,
        reply: oneshot::Sender<Result<Xid>>,
    },
    Commit {
        xid: Xid,
        reply: oneshot::Sender<Result<()>>,
    },
    Abort {
        xid: Xid,
        reply: oneshot::Sender<Result<()>>,
    },

    Insert {
        xid: Xid,
        data: Vec<u8>,
        reply: oneshot::Sender<Result<RowId>>,
    },
    Get {
        xid: Xid,
        row_id: RowId,
        reply: oneshot::Sender<Result<Vec<u8>>>,
    },
    Update {
        xid: Xid,
        row_id: RowId,
        new_data: Vec<u8>,
        reply: oneshot::Sender<Result<RowId>>,
    },
    Delete {
        xid: Xid,
        row_id: RowId,
        reply: oneshot::Sender<Result<()>>,
    },

    ExecuteSql {
        xid: Xid,
        sql: String,
        reply: oneshot::Sender<Result<Vec<ExecResult>>>,
    },
    ExecuteCypher {
        xid: Xid,
        query: String,
        reply: oneshot::Sender<Result<Vec<ExecResult>>>,
    },

    CreateEdge {
        xid: Xid,
        from_id: i64,
        to_id: i64,
        edge_type: String,
        props: String,
        reply: oneshot::Sender<Result<RowId>>,
    },
    DeleteEdge {
        xid: Xid,
        row_id: RowId,
        from_id: i64,
        reply: oneshot::Sender<Result<()>>,
    },
    EdgesFrom {
        xid: Xid,
        from_id: i64,
        reply: oneshot::Sender<Result<Vec<Edge>>>,
    },

    EnableEvents {
        table: String,
        reply: oneshot::Sender<Result<()>>,
    },
    PollEvents {
        xid: Xid,
        consumer: String,
        limit: usize,
        reply: oneshot::Sender<Result<Vec<Event>>>,
    },
    AckEvents {
        xid: Xid,
        consumer: String,
        up_to_seq: i64,
        reply: oneshot::Sender<Result<()>>,
    },
    VacuumEvents {
        xid: Xid,
        reply: oneshot::Sender<Result<usize>>,
    },

    SetColumnIndex {
        table: String,
        column: String,
        kind: Option<IndexKind>,
        reply: oneshot::Sender<Result<()>>,
    },
    IndexStatus {
        table: String,
        column: String,
        reply: oneshot::Sender<Option<IndexStatus>>,
    },

    Checkpoint {
        reply: oneshot::Sender<Result<()>>,
    },

    /// No reply — the worker loop breaks out and the thread ends. Sent
    /// explicitly by `shutdown()` rather than relying on dropping the
    /// sender, mirroring `IndexMsg::Shutdown`'s precedent in
    /// `index_worker.rs`.
    Shutdown,
}

pub struct EngineHandle {
    tx: mpsc::UnboundedSender<EngineRequest>,
    join: Option<JoinHandle<()>>,
    /// Concurrent read path (6b): reads bypass the writer thread's channel
    /// entirely and run on this `Send + Sync` handle over shared state, so
    /// many readers execute in parallel with each other and with the writer.
    read: ReadHandle,
}

impl EngineHandle {
    /// Open `Engine` on the calling thread (surfacing any open/recovery
    /// error immediately), then hand it off to a freshly spawned writer
    /// thread that owns it for the rest of its life.
    pub fn spawn(dir: &Path, page_size: u32) -> Result<Self> {
        let engine = Engine::open(dir, page_size)?;
        // Capture the concurrent read handle before the writer thread takes
        // ownership of the engine — it references only shared state, so it
        // stays valid for the engine's whole life.
        let read = engine.read_handle();
        let (tx, rx) = mpsc::unbounded_channel();
        let join = thread::Builder::new()
            .name("unidb-writer".into())
            .spawn(move || worker_loop(engine, rx))
            .expect("failed to spawn unidb writer thread");
        Ok(Self {
            tx,
            join: Some(join),
            read,
        })
    }

    /// Read one row by [`RowId`] on the concurrent read path (6b): no writer-
    /// thread round-trip, no xid, no WAL. Runs on a blocking pool thread since
    /// the read briefly locks shared state.
    pub async fn get_row(&self, row_id: RowId) -> Result<Vec<u8>> {
        let read = self.read.clone();
        tokio::task::spawn_blocking(move || read.get(row_id))
            .await
            .map_err(|_| DbError::EngineUnavailable)?
    }

    /// Build one `EngineRequest` via `build`, send it (a plain, immediate,
    /// non-blocking call — the channel is unbounded), then await its
    /// reply. A closed channel (writer thread gone, most likely panicked)
    /// maps to `DbError::EngineUnavailable` on both the send and the
    /// receive side.
    async fn dispatch<T: Send + 'static>(
        &self,
        build: impl FnOnce(oneshot::Sender<T>) -> EngineRequest,
    ) -> std::result::Result<T, DbError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(build(reply_tx))
            .map_err(|_| DbError::EngineUnavailable)?;
        reply_rx.await.map_err(|_| DbError::EngineUnavailable)
    }

    pub async fn begin(&self, isolation: Option<IsolationLevel>) -> Result<Xid> {
        self.dispatch(|reply| EngineRequest::Begin { isolation, reply })
            .await?
    }

    pub async fn commit(&self, xid: Xid) -> Result<()> {
        self.dispatch(|reply| EngineRequest::Commit { xid, reply })
            .await?
    }

    pub async fn abort(&self, xid: Xid) -> Result<()> {
        self.dispatch(|reply| EngineRequest::Abort { xid, reply })
            .await?
    }

    pub async fn insert(&self, xid: Xid, data: Vec<u8>) -> Result<RowId> {
        self.dispatch(|reply| EngineRequest::Insert { xid, data, reply })
            .await?
    }

    pub async fn get(&self, xid: Xid, row_id: RowId) -> Result<Vec<u8>> {
        self.dispatch(|reply| EngineRequest::Get { xid, row_id, reply })
            .await?
    }

    pub async fn update(&self, xid: Xid, row_id: RowId, new_data: Vec<u8>) -> Result<RowId> {
        self.dispatch(|reply| EngineRequest::Update {
            xid,
            row_id,
            new_data,
            reply,
        })
        .await?
    }

    pub async fn delete(&self, xid: Xid, row_id: RowId) -> Result<()> {
        self.dispatch(|reply| EngineRequest::Delete { xid, row_id, reply })
            .await?
    }

    pub async fn execute_sql(&self, xid: Xid, sql: String) -> Result<Vec<ExecResult>> {
        self.dispatch(|reply| EngineRequest::ExecuteSql { xid, sql, reply })
            .await?
    }

    pub async fn execute_cypher(&self, xid: Xid, query: String) -> Result<Vec<ExecResult>> {
        self.dispatch(|reply| EngineRequest::ExecuteCypher { xid, query, reply })
            .await?
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
        self.dispatch(|reply| EngineRequest::CreateEdge {
            xid,
            from_id,
            to_id,
            edge_type,
            props,
            reply,
        })
        .await?
    }

    pub async fn delete_edge(&self, xid: Xid, row_id: RowId, from_id: i64) -> Result<()> {
        self.dispatch(|reply| EngineRequest::DeleteEdge {
            xid,
            row_id,
            from_id,
            reply,
        })
        .await?
    }

    pub async fn edges_from(&self, xid: Xid, from_id: i64) -> Result<Vec<Edge>> {
        self.dispatch(|reply| EngineRequest::EdgesFrom {
            xid,
            from_id,
            reply,
        })
        .await?
    }

    pub async fn enable_events(&self, table: String) -> Result<()> {
        self.dispatch(|reply| EngineRequest::EnableEvents { table, reply })
            .await?
    }

    pub async fn poll_events(
        &self,
        xid: Xid,
        consumer: String,
        limit: usize,
    ) -> Result<Vec<Event>> {
        self.dispatch(|reply| EngineRequest::PollEvents {
            xid,
            consumer,
            limit,
            reply,
        })
        .await?
    }

    pub async fn ack_events(&self, xid: Xid, consumer: String, up_to_seq: i64) -> Result<()> {
        self.dispatch(|reply| EngineRequest::AckEvents {
            xid,
            consumer,
            up_to_seq,
            reply,
        })
        .await?
    }

    pub async fn vacuum_events(&self, xid: Xid) -> Result<usize> {
        self.dispatch(|reply| EngineRequest::VacuumEvents { xid, reply })
            .await?
    }

    pub async fn set_column_index(
        &self,
        table: String,
        column: String,
        kind: Option<IndexKind>,
    ) -> Result<()> {
        self.dispatch(|reply| EngineRequest::SetColumnIndex {
            table,
            column,
            kind,
            reply,
        })
        .await?
    }

    pub async fn index_status(&self, table: String, column: String) -> Option<IndexStatus> {
        self.dispatch(|reply| EngineRequest::IndexStatus {
            table,
            column,
            reply,
        })
        .await
        .unwrap_or(None)
    }

    pub async fn checkpoint(&self) -> Result<()> {
        self.dispatch(|reply| EngineRequest::Checkpoint { reply })
            .await?
    }

    /// Send `Shutdown` and join the writer thread, bounded so a stuck
    /// writer can never block server shutdown forever. Mirrors
    /// `IndexHandle::shutdown` (`index_worker.rs`) line-for-line.
    pub fn shutdown(&mut self) {
        let _ = self.tx.send(EngineRequest::Shutdown);
        let Some(join) = self.join.take() else {
            return;
        };
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let _ = thread::Builder::new().spawn(move || {
            let _ = join.join();
            let _ = done_tx.send(());
        });
        if done_rx.recv_timeout(Duration::from_secs(5)).is_err() {
            tracing::warn!("unidb writer thread did not shut down within timeout");
        }
    }
}

impl Drop for EngineHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Group commit (M9). The writer thread owns the sole `Engine` handle, so it
/// can safely defer every per-statement and per-commit fsync and force
/// durability just **once per drained request batch**. Under concurrent load
/// (many clients' `begin`/`execute`/`commit` messages interleaving on the
/// channel), a batch contains many transactions' commit records, and this
/// collapses what used to be one-or-two fsyncs *per statement* into one fsync
/// per batch — directly lifting the single-writer throughput ceiling.
///
/// Durability contract: a `Commit`/`Abort` reply is **withheld** until the
/// batch fsync completes, so a client never observes a committed transaction
/// whose WAL record isn't yet durable. Non-durability-bearing replies (reads,
/// and inserts inside a not-yet-committed txn) are sent immediately — their
/// durability is only promised at commit time. If the batch fsync fails, every
/// commit in that batch is reported as failed, since none of them are durable.
fn worker_loop(mut engine: Engine, mut rx: mpsc::UnboundedReceiver<EngineRequest>) {
    engine.set_deferred_sync(true);

    // Replies (and their tentative results) for commits/aborts whose
    // durability the end-of-batch fsync must cover before we ack them. Both
    // `Commit` and `Abort` reply with `Result<()>`, so they share one queue.
    let mut pending: Vec<(oneshot::Sender<Result<()>>, Result<()>)> = Vec::new();

    // Ack every pending commit/abort after forcing the WAL durable. On fsync
    // failure, downgrade all of them to an error — none are durable.
    fn flush_pending(
        engine: &mut Engine,
        pending: &mut Vec<(oneshot::Sender<Result<()>>, Result<()>)>,
    ) {
        if pending.is_empty() {
            return;
        }
        let sync_ok = match engine.sync_wal() {
            Ok(()) => true,
            Err(e) => {
                tracing::error!(error = %e, "group-commit fsync failed; failing the batch");
                false
            }
        };
        for (reply, result) in pending.drain(..) {
            let final_result = if sync_ok {
                result
            } else {
                Err(DbError::EngineUnavailable)
            };
            let _ = reply.send(final_result);
        }
    }

    'outer: loop {
        // Block for the first request, then greedily drain everything already
        // queued into one batch.
        let Some(first) = rx.blocking_recv() else {
            break;
        };
        let mut batch = vec![first];
        while let Ok(next) = rx.try_recv() {
            batch.push(next);
        }

        for req in batch {
            match req {
                EngineRequest::Shutdown => {
                    flush_pending(&mut engine, &mut pending);
                    break 'outer;
                }
                EngineRequest::Commit { xid, reply } => {
                    let result = engine.commit(xid);
                    pending.push((reply, result));
                }
                EngineRequest::Abort { xid, reply } => {
                    let result = engine.abort(xid);
                    pending.push((reply, result));
                }
                EngineRequest::Checkpoint { reply } => {
                    // A checkpoint flushes dirty pages and truncates the WAL,
                    // so anything deferred must be forced durable first.
                    flush_pending(&mut engine, &mut pending);
                    let _ = reply.send(engine.checkpoint());
                }
                other => dispatch_immediate(&mut engine, other),
            }
        }

        flush_pending(&mut engine, &mut pending);
    }
}

/// Handle every request whose reply can be sent immediately (reads, and writes
/// inside a not-yet-committed transaction). `Commit`/`Abort`/`Checkpoint`/
/// `Shutdown` are handled by the batching loop and never reach here.
fn dispatch_immediate(engine: &mut Engine, req: EngineRequest) {
    match req {
        EngineRequest::Shutdown
        | EngineRequest::Commit { .. }
        | EngineRequest::Abort { .. }
        | EngineRequest::Checkpoint { .. } => {
            unreachable!("batching loop handles these variants directly")
        }
        EngineRequest::Begin { isolation, reply } => {
            let result = match isolation {
                Some(iso) => engine.begin_with_isolation(iso),
                None => engine.begin(),
            };
            let _ = reply.send(result);
        }
        EngineRequest::Insert { xid, data, reply } => {
            let _ = reply.send(engine.insert(xid, &data));
        }
        EngineRequest::Get { xid, row_id, reply } => {
            let _ = reply.send(engine.get(xid, row_id));
        }
        EngineRequest::Update {
            xid,
            row_id,
            new_data,
            reply,
        } => {
            let _ = reply.send(engine.update(xid, row_id, &new_data));
        }
        EngineRequest::Delete { xid, row_id, reply } => {
            let _ = reply.send(engine.delete(xid, row_id));
        }
        EngineRequest::ExecuteSql { xid, sql, reply } => {
            let _ = reply.send(engine.execute_sql(xid, &sql));
        }
        EngineRequest::ExecuteCypher { xid, query, reply } => {
            let _ = reply.send(engine.execute_cypher(xid, &query));
        }
        EngineRequest::CreateEdge {
            xid,
            from_id,
            to_id,
            edge_type,
            props,
            reply,
        } => {
            let _ = reply.send(engine.create_edge(xid, from_id, to_id, &edge_type, &props));
        }
        EngineRequest::DeleteEdge {
            xid,
            row_id,
            from_id,
            reply,
        } => {
            let _ = reply.send(engine.delete_edge(xid, row_id, from_id));
        }
        EngineRequest::EdgesFrom {
            xid,
            from_id,
            reply,
        } => {
            let _ = reply.send(engine.edges_from(xid, from_id));
        }
        EngineRequest::EnableEvents { table, reply } => {
            let _ = reply.send(engine.enable_events(&table));
        }
        EngineRequest::PollEvents {
            xid,
            consumer,
            limit,
            reply,
        } => {
            let _ = reply.send(engine.poll_events(xid, &consumer, limit));
        }
        EngineRequest::AckEvents {
            xid,
            consumer,
            up_to_seq,
            reply,
        } => {
            let _ = reply.send(engine.ack_events(xid, &consumer, up_to_seq));
        }
        EngineRequest::VacuumEvents { xid, reply } => {
            let _ = reply.send(engine.vacuum_events(xid));
        }
        EngineRequest::SetColumnIndex {
            table,
            column,
            kind,
            reply,
        } => {
            let _ = reply.send(engine.set_column_index(&table, &column, kind));
        }
        EngineRequest::IndexStatus {
            table,
            column,
            reply,
        } => {
            let _ = reply.send(engine.index_status(&table, &column));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn assert_send<T: Send>() {}

    #[test]
    fn engine_is_send() {
        assert_send::<Engine>();
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
    async fn shutdown_joins_promptly_and_is_idempotent() {
        let dir = tempdir().unwrap();
        let mut handle = EngineHandle::spawn(dir.path(), 0).unwrap();

        let start = std::time::Instant::now();
        handle.shutdown();
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "shutdown must return well within its bound"
        );
        handle.shutdown(); // second call must be a harmless no-op

        // A fresh `Engine::open` against the same directory must succeed
        // immediately — shutdown must not leave the database in a state
        // that blocks a subsequent open (e.g. a still-held file lock).
        Engine::open(dir.path(), 0).unwrap();
    }
}
