// Background secondary-index worker (M2.b): the engine's async index thread.
// It owns only in-memory index structures, built purely from channel messages
// sent by the foreground thread — it never touches `BufferPool`/`Wal`/`Heap`,
// none of which have synchronization primitives or are designed for concurrent
// access. Both the rebuild-on-open rescan and every live INSERT/UPDATE funnel
// through the same `IndexMsg::Upsert` channel, so the worker's job is uniformly
// "apply whatever arrives."
//
// **Since Phase 3 this worker serves only the vector (HNSW) index.** The B-Tree
// (P3.a), full-text/inverted (P3.b), and edge-adjacency (P3.b) indexes all
// became durable, synchronous, WAL-logged on-disk B+trees written on the writer
// thread (`sql/executor.rs::apply_durable_index_writes`, `graph/edges.rs`), so
// they no longer flow through here. The CSR graph index (M7) was retired in
// P3.b — it was consulted by no read path after the M7 traversal-uses-CSR
// revert, and adjacency is now served durably by the edge index. HNSW remains
// here because its graph construction is O(n log n) per upsert and genuinely
// benefits from running off the write path; it is still rebuilt on open (P3.c
// will make it durable too, at which point this worker can be removed).
//
// The index is derived, rebuildable data with zero WAL footprint: losing it on
// crash just means rebuilding on next open — an eventual-consistency contract,
// not a durability one.

use std::collections::HashMap;
use std::sync::{mpsc, Arc, RwLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::catalog::IndexKind;
use crate::heap::RowId;
use crate::vector::VectorIndex;

#[derive(Debug, Clone)]
pub enum IndexedColumn {
    Vector { column: String, data: Vec<f32> },
}

pub enum IndexMsg {
    Upsert {
        table: String,
        record: RowId,
        indexed_cols: Vec<IndexedColumn>,
    },
    /// Sent once, after every row from a rebuild-on-open or `CREATE INDEX`
    /// backfill has been enqueued, so the worker can flip `Building` to `Ready`
    /// at the point it has actually drained the backlog. Carries `kind` because
    /// a backfill over zero rows never sends a single `Upsert` — this message
    /// alone must be able to create the (empty, `Ready`) entry.
    MarkReady {
        table: String,
        column: String,
        kind: IndexKind,
    },
    Shutdown,
}

pub enum SecondaryIndex {
    Vector(VectorIndex),
}

// `serde::Serialize` for the M5 REST server (`GET /indexes/:table/:column/
// status`) — see `heap::RowId`'s doc comment for why this isn't feature-gated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum IndexStatus {
    Building { rows_done: u64 },
    Ready,
}

pub struct IndexEntry {
    pub status: IndexStatus,
    pub index: SecondaryIndex,
}

/// Keyed by `(table, column)`. Shared with the foreground thread so `NEAR`
/// queries can read the current index without touching `BufferPool`/`Wal`/
/// `Heap` (the one narrow exception to "the worker owns its structures alone").
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
    /// panicked). A stale/missing secondary index is a correctness non-issue —
    /// it's rebuilt on next open — so this logs and drops rather than
    /// propagating an error to the SQL caller.
    pub fn send(&self, msg: IndexMsg) {
        if self.tx.send(msg).is_err() {
            tracing::warn!("index worker channel closed; dropping message");
        }
    }

    /// Synchronously scrub every reclaimed `RowId` from the in-memory vector
    /// index for `table` (M10.c index vacuum). Runs on the caller's (writer)
    /// thread under the shared write lock — vacuum needs the removal to have
    /// *completed* before it hands the freed slot out for reuse. The durable
    /// indexes (BTree/FullText/edge) are scrubbed separately, directly on disk
    /// (`lib.rs::vacuum_inner`).
    pub fn remove_rows(&self, table: &str, rows: &[RowId]) {
        if rows.is_empty() {
            return;
        }
        let mut guard = self.indexes.write().unwrap();
        for ((t, _col), entry) in guard.iter_mut() {
            if t != table {
                continue;
            }
            for &rid in rows {
                match &mut entry.index {
                    SecondaryIndex::Vector(v) => v.remove(rid),
                }
            }
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
        // throwaway watcher thread and bound *our* wait via `recv_timeout`.
        let _ = thread::Builder::new().spawn(move || {
            let _ = join.join();
            let _ = done_tx.send(());
        });
        if done_rx.recv_timeout(Duration::from_secs(5)).is_err() {
            tracing::warn!("index worker did not shut down within timeout");
        }
    }
}

fn new_index_for_kind(kind: IndexKind) -> SecondaryIndex {
    match kind {
        IndexKind::Hnsw => SecondaryIndex::Vector(VectorIndex::new()),
        // BTree (P3.a), FullText (P3.b), and Csr (retired in P3.b) are never
        // handled by this worker — the executor/graph layer write them durably,
        // so these arms are unreachable by construction.
        other => {
            unreachable!(
                "{other:?} is a durable/retired index kind, not managed by the async worker"
            )
        }
    }
}

/// Apply one message, returning `false` iff it was `Shutdown`.
fn apply_msg(msg: IndexMsg, indexes: &SharedIndexes) -> bool {
    match msg {
        IndexMsg::Shutdown => return false,
        IndexMsg::MarkReady {
            table,
            column,
            kind,
        } => {
            let mut guard = indexes.write().unwrap();
            let entry = guard.entry((table, column)).or_insert_with(|| IndexEntry {
                status: IndexStatus::Building { rows_done: 0 },
                index: new_index_for_kind(kind),
            });
            entry.status = IndexStatus::Ready;
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
                        let entry =
                            guard
                                .entry((table.clone(), column))
                                .or_insert_with(|| IndexEntry {
                                    status: IndexStatus::Building { rows_done: 0 },
                                    index: new_index_for_kind(IndexKind::Hnsw),
                                });
                        if let IndexStatus::Building { rows_done } = &mut entry.status {
                            *rows_done += 1;
                        }
                        let SecondaryIndex::Vector(v) = &mut entry.index;
                        v.upsert(record, data);
                    }
                }
            }
        }
    }
    true
}

fn worker_loop(rx: mpsc::Receiver<IndexMsg>, indexes: SharedIndexes) {
    loop {
        let Ok(msg) = rx.recv() else {
            return; // sender dropped
        };
        if !apply_msg(msg, &indexes) {
            return; // Shutdown
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
    /// messages asynchronously, so tests must poll rather than assume immediate
    /// application.
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
            kind: IndexKind::Hnsw,
        });
        wait_for(|| handle.status("t", "embedding") == Some(IndexStatus::Ready));

        {
            let guard = handle.indexes.read().unwrap();
            let entry = guard
                .get(&("t".to_string(), "embedding".to_string()))
                .unwrap();
            let SecondaryIndex::Vector(v) = &entry.index;
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

    /// M10.c: `remove_rows` synchronously scrubs reclaimed RowIds from the
    /// in-memory vector index, so a vacuumed slot can be reused without a stale
    /// candidate surviving. (BTree/FullText/edge are durable since P3.a/P3.b and
    /// scrubbed directly on disk, tested in `btree_index.rs`/`lib.rs`.)
    #[test]
    fn remove_rows_scrubs_reclaimed_rowids_from_vector_index() {
        let mut handle = IndexHandle::spawn();
        handle.send(IndexMsg::Upsert {
            table: "t".into(),
            record: rid(4, 0),
            indexed_cols: vec![IndexedColumn::Vector {
                column: "embedding".into(),
                data: vec![1.0, 2.0],
            }],
        });
        handle.send(IndexMsg::MarkReady {
            table: "t".into(),
            column: "embedding".into(),
            kind: IndexKind::Hnsw,
        });
        wait_for(|| handle.status("t", "embedding") == Some(IndexStatus::Ready));
        wait_for(|| {
            let guard = handle.indexes.read().unwrap();
            matches!(
                &guard
                    .get(&("t".to_string(), "embedding".to_string()))
                    .unwrap()
                    .index,
                SecondaryIndex::Vector(v) if v.len() == 1
            )
        });

        handle.remove_rows("t", &[rid(4, 0)]);

        let guard = handle.indexes.read().unwrap();
        let SecondaryIndex::Vector(v) = &guard
            .get(&("t".to_string(), "embedding".to_string()))
            .unwrap()
            .index;
        assert_eq!(
            v.len(),
            0,
            "the reclaimed RowId must be gone from the index"
        );
        drop(guard);
        handle.shutdown();
    }

    #[test]
    fn shutdown_actually_joins_worker_thread() {
        let mut handle = IndexHandle::spawn();
        handle.shutdown();
        handle.shutdown();
    }

    /// Regression test: a `MarkReady` for a column with zero rows at
    /// index-creation time (e.g. `CREATE INDEX` on an empty table) must still
    /// create a `Ready` entry — previously it silently no-opped.
    #[test]
    fn mark_ready_on_never_upserted_column_creates_ready_entry() {
        let mut handle = IndexHandle::spawn();
        handle.send(IndexMsg::MarkReady {
            table: "t".into(),
            column: "embedding".into(),
            kind: IndexKind::Hnsw,
        });
        wait_for(|| handle.status("t", "embedding") == Some(IndexStatus::Ready));

        handle.send(IndexMsg::Upsert {
            table: "t".into(),
            record: rid(1, 0),
            indexed_cols: vec![IndexedColumn::Vector {
                column: "embedding".into(),
                data: vec![0.1, 0.2],
            }],
        });
        wait_for(|| {
            let guard = handle.indexes.read().unwrap();
            guard
                .get(&("t".to_string(), "embedding".to_string()))
                .map(|e| matches!(e.index, SecondaryIndex::Vector(ref v) if v.len() == 1))
                .unwrap_or(false)
        });
        assert_eq!(handle.status("t", "embedding"), Some(IndexStatus::Ready));

        handle.shutdown();
    }
}
