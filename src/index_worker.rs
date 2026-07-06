// Background secondary-index worker (M2.b): the engine's first background
// thread. It owns only in-memory index structures, built purely from
// channel messages sent by the foreground thread — it never touches
// `BufferPool`/`Wal`/`Heap`, none of which have synchronization primitives
// or are designed for concurrent access. Both the rebuild-on-open rescan
// (run on the foreground thread against the already-owned `Heap`/`Catalog`)
// and every live INSERT/UPDATE funnel through the same `IndexMsg::Upsert`
// channel, so the worker thread's job is uniformly "apply whatever arrives."
//
// The index is derived, rebuildable data with zero WAL footprint (CLAUDE.md
// M2 scope): losing it on crash just means rebuilding on next open, so there
// is no new durability contract here, only an eventual-consistency one.
//
// `SecondaryIndex` has `Vector` (M2.b) and `FullText` (M2.c) variants. The
// message/status plumbing below is keyed by `(table, column)`, not by index
// kind, so it generalized to the second kind with no changes to its shape.

use std::collections::HashMap;
use std::sync::{mpsc, Arc, RwLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::fulltext::InvertedIndex;
use crate::heap::RowId;
use crate::vector::VectorIndex;

#[derive(Debug, Clone)]
pub enum IndexedColumn {
    Vector { column: String, data: Vec<f32> },
    Text { column: String, data: String },
}

pub enum IndexMsg {
    Upsert {
        table: String,
        record: RowId,
        indexed_cols: Vec<IndexedColumn>,
    },
    /// Sent once, after every row from a rebuild-on-open or `CREATE INDEX`
    /// backfill has been enqueued, so the worker can flip `Building` to
    /// `Ready` at the point it has actually drained the backlog rather than
    /// the point the foreground finished *sending* it (those differ since
    /// processing is asynchronous).
    MarkReady {
        table: String,
        column: String,
    },
    Shutdown,
}

pub enum SecondaryIndex {
    Vector(VectorIndex),
    FullText(InvertedIndex),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexStatus {
    Building { rows_done: u64 },
    Ready,
}

pub struct IndexEntry {
    pub status: IndexStatus,
    pub index: SecondaryIndex,
}

/// Keyed by `(table, column)`. Shared with the foreground thread — the one
/// narrow, explicit exception to "the worker owns its structures alone"
/// (CLAUDE.md's M2 plan): a read lock lets future `NEAR` queries (M2.d) see
/// the current index without touching `BufferPool`/`Wal`/`Heap`.
pub type SharedIndexes = Arc<RwLock<HashMap<(String, String), IndexEntry>>>;

pub struct IndexHandle {
    tx: mpsc::Sender<IndexMsg>,
    join: Option<JoinHandle<()>>,
    pub indexes: SharedIndexes,
}

impl IndexHandle {
    pub fn spawn() -> Self {
        let (tx, rx) = mpsc::channel();
        let indexes: SharedIndexes = Arc::new(RwLock::new(HashMap::new()));
        let worker_indexes = Arc::clone(&indexes);
        let join = thread::Builder::new()
            .name("unidb-index-worker".into())
            .spawn(move || worker_loop(rx, worker_indexes))
            .expect("failed to spawn index worker thread");
        Self {
            tx,
            join: Some(join),
            indexes,
        }
    }

    /// A channel-send failure means the worker thread already exited (e.g.
    /// panicked). A stale/missing secondary index is a correctness
    /// non-issue — it's rebuilt on next open — not a durability one, so this
    /// logs and drops rather than propagating an error to the SQL caller.
    pub fn send(&self, msg: IndexMsg) {
        if self.tx.send(msg).is_err() {
            tracing::warn!("index worker channel closed; dropping message");
        }
    }

    pub fn status(&self, table: &str, column: &str) -> Option<IndexStatus> {
        self.indexes
            .read()
            .unwrap()
            .get(&(table.to_string(), column.to_string()))
            .map(|e| e.status)
    }

    /// Send `Shutdown` and join the worker thread, bounded so a stuck worker
    /// can never block engine close forever.
    pub fn shutdown(&mut self) {
        let _ = self.tx.send(IndexMsg::Shutdown);
        let Some(join) = self.join.take() else {
            return;
        };
        let (done_tx, done_rx) = mpsc::channel();
        // `std::thread::JoinHandle::join` has no timeout in std; run it on a
        // throwaway watcher thread and bound *our* wait via `recv_timeout`
        // instead. If the worker never finishes, the watcher just leaks
        // (harmless — the process is shutting down this engine either way).
        let _ = thread::Builder::new().spawn(move || {
            let _ = join.join();
            let _ = done_tx.send(());
        });
        if done_rx.recv_timeout(Duration::from_secs(5)).is_err() {
            tracing::warn!("index worker did not shut down within timeout");
        }
    }
}

fn worker_loop(rx: mpsc::Receiver<IndexMsg>, indexes: SharedIndexes) {
    for msg in rx {
        match msg {
            IndexMsg::Shutdown => break,
            IndexMsg::MarkReady { table, column } => {
                let mut guard = indexes.write().unwrap();
                if let Some(entry) = guard.get_mut(&(table, column)) {
                    entry.status = IndexStatus::Ready;
                }
            }
            IndexMsg::Upsert {
                table,
                record,
                indexed_cols,
            } => {
                for col in indexed_cols {
                    match col {
                        IndexedColumn::Vector { column, data } => {
                            let mut guard = indexes.write().unwrap();
                            let entry = guard.entry((table.clone(), column)).or_insert_with(|| {
                                IndexEntry {
                                    status: IndexStatus::Building { rows_done: 0 },
                                    index: SecondaryIndex::Vector(VectorIndex::new()),
                                }
                            });
                            if let IndexStatus::Building { rows_done } = &mut entry.status {
                                *rows_done += 1;
                            }
                            let SecondaryIndex::Vector(v) = &mut entry.index else {
                                continue;
                            };
                            v.upsert(record, data);
                        }
                        IndexedColumn::Text { column, data } => {
                            let mut guard = indexes.write().unwrap();
                            let entry = guard.entry((table.clone(), column)).or_insert_with(|| {
                                IndexEntry {
                                    status: IndexStatus::Building { rows_done: 0 },
                                    index: SecondaryIndex::FullText(InvertedIndex::new()),
                                }
                            });
                            if let IndexStatus::Building { rows_done } = &mut entry.status {
                                *rows_done += 1;
                            }
                            let SecondaryIndex::FullText(t) = &mut entry.index else {
                                continue;
                            };
                            t.upsert(record, &data);
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn rid(page: u32, slot: u16) -> RowId {
        RowId {
            page_id: page,
            slot,
        }
    }

    /// Block until `f()` returns true or ~1s elapses — the worker applies
    /// messages asynchronously, so tests must poll rather than assume
    /// immediate application.
    fn wait_for(mut f: impl FnMut() -> bool) {
        let start = Instant::now();
        while !f() {
            if start.elapsed() > Duration::from_secs(1) {
                panic!("condition not met within timeout");
            }
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn upsert_builds_index_and_marks_ready() {
        let mut handle = IndexHandle::spawn();
        handle.send(IndexMsg::Upsert {
            table: "t".into(),
            record: rid(1, 0),
            indexed_cols: vec![IndexedColumn::Vector {
                column: "embedding".into(),
                data: vec![0.1, 0.2],
            }],
        });
        wait_for(|| handle.status("t", "embedding").is_some());
        assert!(matches!(
            handle.status("t", "embedding"),
            Some(IndexStatus::Building { rows_done: 1 })
        ));

        handle.send(IndexMsg::MarkReady {
            table: "t".into(),
            column: "embedding".into(),
        });
        wait_for(|| handle.status("t", "embedding") == Some(IndexStatus::Ready));

        {
            let guard = handle.indexes.read().unwrap();
            let entry = guard
                .get(&("t".to_string(), "embedding".to_string()))
                .unwrap();
            let SecondaryIndex::Vector(v) = &entry.index else {
                panic!("expected a vector index");
            };
            assert_eq!(v.len(), 1);
        }

        handle.shutdown();
    }

    #[test]
    fn unrelated_index_status_is_none() {
        let mut handle = IndexHandle::spawn();
        assert_eq!(handle.status("nope", "nope"), None);
        handle.shutdown();
    }

    #[test]
    fn shutdown_actually_joins_worker_thread() {
        let mut handle = IndexHandle::spawn();
        handle.shutdown();
        // After shutdown, the join handle is consumed; a second call is a
        // no-op rather than a panic.
        handle.shutdown();
    }
}
