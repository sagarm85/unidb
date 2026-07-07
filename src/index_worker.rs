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
// `SecondaryIndex` has `Vector` (M2.b), `FullText` (M2.c), `BTree` (M6),
// and `Csr` (M7) variants. The message/status plumbing below is keyed by
// `(table, column)`, not by index kind, so it generalized to each new kind
// with no changes to its shape.
//
// `Csr` is the one variant that doesn't apply its `Upsert` immediately —
// `CsrIndex::stage` just records the edge, and `worker_loop` debounces the
// actual `CsrIndex::rebuild()` call: it drains every currently-queued
// message (via `try_recv`) before rebuilding any CSR entry touched during
// that burst, coalescing N queued edge writes into one rebuild pass
// instead of one per message. This is a deliberate improvement over
// `VectorIndex`'s still-unfixed "rebuild on every single upsert" pattern
// (see `vector.rs`'s module doc) — CSR's rebuild cost is a function of the
// *entire* edge set, and edges are typically written far more frequently
// than vector upserts, so debouncing matters more here.

use std::collections::{HashMap, HashSet};
use std::sync::{mpsc, Arc, RwLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::btree_index::{BTreeIndex, OrderedValue};
use crate::catalog::IndexKind;
use crate::csr_index::CsrIndex;
use crate::fulltext::InvertedIndex;
use crate::heap::RowId;
use crate::vector::VectorIndex;

#[derive(Debug, Clone)]
pub enum IndexedColumn {
    Vector { column: String, data: Vec<f32> },
    Text { column: String, data: String },
    Ordered { column: String, data: OrderedValue },
    Edge { column: String, from_id: i64 },
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
    /// processing is asynchronous). Carries `kind` because a backfill over
    /// zero currently-committed rows never sends a single `Upsert` — this
    /// message alone must be able to create the (empty, `Ready`) entry.
    MarkReady {
        table: String,
        column: String,
        kind: IndexKind,
    },
    Shutdown,
}

pub enum SecondaryIndex {
    Vector(VectorIndex),
    FullText(InvertedIndex),
    BTree(BTreeIndex),
    Csr(CsrIndex),
}

// `serde::Serialize` for the M5 REST server (`GET /indexes/:table/:column/
// status`) — see `heap::RowId`'s doc comment for why this isn't
// feature-gated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
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

fn new_index_for_kind(kind: IndexKind) -> SecondaryIndex {
    match kind {
        IndexKind::Hnsw => SecondaryIndex::Vector(VectorIndex::new()),
        IndexKind::FullText => SecondaryIndex::FullText(InvertedIndex::new()),
        IndexKind::BTree => SecondaryIndex::BTree(BTreeIndex::new()),
        IndexKind::Csr => SecondaryIndex::Csr(CsrIndex::new()),
    }
}

/// One `(table, column)` key touched by a CSR-backed `Upsert` during the
/// current drain cycle — collected by `apply_msg`, rebuilt exactly once by
/// `rebuild_dirty` after the burst has been fully applied.
type DirtyCsrKeys = HashSet<(String, String)>;

/// Apply one message, returning `false` iff it was `Shutdown` (the caller
/// must stop the loop). Every non-CSR variant behaves exactly as before —
/// applied immediately, no debouncing. CSR's `Upsert` handling only
/// stages the edge and records the key in `dirty`; the actual
/// `CsrIndex::rebuild()` happens in `rebuild_dirty`, once per drain cycle.
fn apply_msg(msg: IndexMsg, indexes: &SharedIndexes, dirty: &mut DirtyCsrKeys) -> bool {
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
                        let SecondaryIndex::Vector(v) = &mut entry.index else {
                            continue;
                        };
                        v.upsert(record, data);
                    }
                    IndexedColumn::Text { column, data } => {
                        let mut guard = indexes.write().unwrap();
                        let entry =
                            guard
                                .entry((table.clone(), column))
                                .or_insert_with(|| IndexEntry {
                                    status: IndexStatus::Building { rows_done: 0 },
                                    index: new_index_for_kind(IndexKind::FullText),
                                });
                        if let IndexStatus::Building { rows_done } = &mut entry.status {
                            *rows_done += 1;
                        }
                        let SecondaryIndex::FullText(t) = &mut entry.index else {
                            continue;
                        };
                        t.upsert(record, &data);
                    }
                    IndexedColumn::Ordered { column, data } => {
                        let mut guard = indexes.write().unwrap();
                        let entry =
                            guard
                                .entry((table.clone(), column))
                                .or_insert_with(|| IndexEntry {
                                    status: IndexStatus::Building { rows_done: 0 },
                                    index: new_index_for_kind(IndexKind::BTree),
                                });
                        if let IndexStatus::Building { rows_done } = &mut entry.status {
                            *rows_done += 1;
                        }
                        let SecondaryIndex::BTree(b) = &mut entry.index else {
                            continue;
                        };
                        b.upsert(record, data);
                    }
                    IndexedColumn::Edge { column, from_id } => {
                        let mut guard = indexes.write().unwrap();
                        let key = (table.clone(), column);
                        let entry = guard.entry(key.clone()).or_insert_with(|| IndexEntry {
                            status: IndexStatus::Building { rows_done: 0 },
                            index: new_index_for_kind(IndexKind::Csr),
                        });
                        if let IndexStatus::Building { rows_done } = &mut entry.status {
                            *rows_done += 1;
                        }
                        let SecondaryIndex::Csr(csr) = &mut entry.index else {
                            continue;
                        };
                        csr.stage(from_id, record);
                        dirty.insert(key);
                    }
                }
            }
        }
    }
    true
}

/// Rebuild every CSR entry touched during the current drain cycle exactly
/// once, then clear the dirty set.
fn rebuild_dirty(indexes: &SharedIndexes, dirty: &mut DirtyCsrKeys) {
    if dirty.is_empty() {
        return;
    }
    let mut guard = indexes.write().unwrap();
    for key in dirty.iter() {
        if let Some(entry) = guard.get_mut(key) {
            if let SecondaryIndex::Csr(csr) = &mut entry.index {
                csr.rebuild();
            }
        }
    }
    dirty.clear();
}

fn worker_loop(rx: mpsc::Receiver<IndexMsg>, indexes: SharedIndexes) {
    loop {
        let Ok(msg) = rx.recv() else {
            return; // sender dropped
        };
        let mut dirty: DirtyCsrKeys = HashSet::new();
        if !apply_msg(msg, &indexes, &mut dirty) {
            return; // Shutdown
        }

        // Drain every currently-queued message before rebuilding any dirty
        // CSR index — coalesces a burst of edge writes arriving together
        // into one rebuild pass instead of one per message (see module
        // doc). Falls back to the blocking `recv()` above once the channel
        // is momentarily empty.
        loop {
            match rx.try_recv() {
                Ok(msg) => {
                    if !apply_msg(msg, &indexes, &mut dirty) {
                        rebuild_dirty(&indexes, &mut dirty);
                        return;
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    rebuild_dirty(&indexes, &mut dirty);
                    return;
                }
            }
        }
        rebuild_dirty(&indexes, &mut dirty);
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
            kind: IndexKind::Hnsw,
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
    fn edge_upserts_eventually_produce_correct_csr_candidates() {
        let mut handle = IndexHandle::spawn();
        handle.send(IndexMsg::Upsert {
            table: "__edges__".into(),
            record: rid(1, 0),
            indexed_cols: vec![IndexedColumn::Edge {
                column: "from_id".into(),
                from_id: 42,
            }],
        });
        handle.send(IndexMsg::Upsert {
            table: "__edges__".into(),
            record: rid(1, 1),
            indexed_cols: vec![IndexedColumn::Edge {
                column: "from_id".into(),
                from_id: 42,
            }],
        });
        handle.send(IndexMsg::MarkReady {
            table: "__edges__".into(),
            column: "from_id".into(),
            kind: IndexKind::Csr,
        });
        wait_for(|| handle.status("__edges__", "from_id") == Some(IndexStatus::Ready));

        let guard = handle.indexes.read().unwrap();
        let entry = guard
            .get(&("__edges__".to_string(), "from_id".to_string()))
            .unwrap();
        let SecondaryIndex::Csr(csr) = &entry.index else {
            panic!("expected a CSR index");
        };
        let mut candidates = csr.candidates(42).to_vec();
        candidates.sort_by_key(|r| r.slot);
        assert_eq!(candidates, vec![rid(1, 0), rid(1, 1)]);
        drop(guard);
        handle.shutdown();
    }

    /// M7's debounce proof: a burst of `Upsert` messages sent back-to-back
    /// (no gap for the worker to drain between sends) must coalesce into
    /// far fewer `CsrIndex::rebuild()` calls than messages sent — not
    /// exactly one, since the sender and worker threads race in ways a
    /// test can't fully pin down, but meaningfully less than N, proving
    /// real coalescing rather than "did nothing."
    #[test]
    fn burst_of_edge_upserts_coalesces_into_far_fewer_rebuilds_than_messages() {
        let mut handle = IndexHandle::spawn();
        const N: i64 = 200;
        for i in 0..N {
            handle.send(IndexMsg::Upsert {
                table: "__edges__".into(),
                record: rid(1, (i % 1000) as u16),
                indexed_cols: vec![IndexedColumn::Edge {
                    column: "from_id".into(),
                    from_id: i,
                }],
            });
        }
        handle.send(IndexMsg::MarkReady {
            table: "__edges__".into(),
            column: "from_id".into(),
            kind: IndexKind::Csr,
        });
        wait_for(|| handle.status("__edges__", "from_id") == Some(IndexStatus::Ready));
        // The Ready flip and the final rebuild can race by one message, so
        // give the last rebuild a brief moment to land before reading the
        // count.
        std::thread::sleep(Duration::from_millis(20));

        let guard = handle.indexes.read().unwrap();
        let entry = guard
            .get(&("__edges__".to_string(), "from_id".to_string()))
            .unwrap();
        let SecondaryIndex::Csr(csr) = &entry.index else {
            panic!("expected a CSR index");
        };
        // Every from_id (0..N) must be findable regardless of how many
        // rebuild passes it took.
        for i in 0..N {
            assert!(
                !csr.candidates(i).is_empty(),
                "from_id {i} missing from CSR after settling"
            );
        }
        let rebuilds = csr.rebuild_count();
        assert!(
            rebuilds < N as usize,
            "expected debouncing to coalesce {N} messages into far fewer than {N} rebuilds, got {rebuilds}"
        );
        drop(guard);
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

    /// Regression test: a `MarkReady` for a column with zero rows at
    /// index-creation time (e.g. `CREATE INDEX` on an empty table) must
    /// still create a `Ready` entry — previously it silently no-opped
    /// because no `Upsert` had ever created the entry, and a later live
    /// upsert would re-create it stuck in `Building` forever.
    #[test]
    fn mark_ready_on_never_upserted_column_creates_ready_entry() {
        let mut handle = IndexHandle::spawn();
        handle.send(IndexMsg::MarkReady {
            table: "t".into(),
            column: "embedding".into(),
            kind: IndexKind::Hnsw,
        });
        wait_for(|| handle.status("t", "embedding") == Some(IndexStatus::Ready));

        // A live upsert after that must not regress status back to Building.
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
