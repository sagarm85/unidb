// Large-object (big-file) storage — P3.d.
//
// Values too large to sit inline in an 8 KiB heap tuple are stored **out of
// line**, chunked, and **streamed** — never loaded whole into RAM. The design
// reuses the durable machinery already built rather than inventing an overflow-
// page format:
//
//   * Each large object is a sequence of fixed-size **chunk rows** in a `__lobs__`
//     system heap table `(lob_id, chunk_no, data)` — ordinary MVCC/WAL/crash-
//     recovered rows, exactly like `__edges__`/`__events__`. Writing the chunks
//     under the caller's `xid` makes the blob **atomic with the owning row**:
//     they commit or abort together, with zero new transaction machinery.
//   * A durable `DiskBTree` (P3.a) on `lob_id` maps a blob to its chunk `RowId`s,
//     so open/read is O(chunks-of-this-blob), not O(all blobs), and is itself
//     crash-recovered / never rebuilt on open.
//   * **Streaming**: `write_stream` pulls from a `Read` one chunk at a time and
//     inserts it; `read_stream` fetches one chunk row at a time and hands it to
//     a sink. A multi-GB value therefore costs one chunk (~8 KiB) of resident
//     memory at a time on both paths — the "without OOM" gate.
//
// Orphan reclamation: a blob whose owning row is gone is deleted via
// `delete` (stamps every chunk row's xmax under a txn); the ordinary heap
// vacuum (M10) then physically reclaims those dead chunk rows. See
// `Engine::vacuum`.
//
// Out of scope for P3.d (documented follow-ups): transparently toasting a large
// `BYTEA` column value in place of an inline store (this module is the explicit
// large-object API the toast path would call); streaming REST upload/download
// routes (`server/`); and a per-blob length/refcount header.

use std::sync::Mutex;

use crate::{
    btree_index::{DiskBTree, OrderedValue},
    bufferpool::BufferPool,
    catalog::{Catalog, CatalogCtx, ColumnDef, ColumnType, TableDef},
    error::{DbError, Result},
    format::{PageId, Xid},
    heap::{Heap, RowId},
    mvcc::Snapshot,
    sql::{
        executor::{decode_row, encode_row},
        logical::Literal,
    },
    txn::{TransactionManager, UndoAction},
    wal::Wal,
};

pub const LOBS_TABLE: &str = "__lobs__";

/// Payload bytes per chunk. Kept well under the 8 KiB page's usable space
/// (page header + slot + 24-byte tuple header + the row's own framing), so a
/// chunk row always fits on a fresh heap page.
pub const CHUNK_SIZE: usize = 7000;

/// The `__lobs__` system table: `(lob_id INT, chunk_no INT, data BYTEA)`.
pub fn lobs_table_def() -> TableDef {
    let col = |name: &str, ty: ColumnType| ColumnDef {
        name: name.to_string(),
        ty,
        index: None,
        index_root: None,
        dropped: false,
        constraints: Default::default(),
    };
    TableDef {
        name: LOBS_TABLE.to_string(),
        columns: vec![
            col("lob_id", ColumnType::Int64),
            col("chunk_no", ColumnType::Int64),
            col("data", ColumnType::Bytea),
        ],
        pages: Vec::new(),
        fsm_meta: None,
        rls_policy: None,
        events_enabled: false,
        serial_next: Default::default(),
        constraints: Default::default(),
        generation: 0,
    }
}

/// A handle to the large-object store, reconstructed from the `__lobs__` table's
/// durable `lob_id` index meta page (like `Heap::from_pages` / `DiskBTree::new`).
pub struct LobStore {
    index_meta: PageId,
    page_size: usize,
}

impl LobStore {
    pub fn new(index_meta: PageId, page_size: usize) -> Self {
        Self {
            index_meta,
            page_size,
        }
    }

    pub fn index_meta(&self) -> PageId {
        self.index_meta
    }

    fn index(&self) -> DiskBTree {
        DiskBTree::new(self.index_meta, self.page_size)
    }

    /// Stream a large object in from `reader`, chunking it into `__lobs__` rows
    /// under `xid` (so it is atomic with the caller's transaction). Returns the
    /// number of bytes written. Resident memory is one chunk at a time.
    #[allow(clippy::too_many_arguments)]
    pub fn write_stream<R: std::io::Read>(
        &self,
        xid: Xid,
        lob_id: i64,
        mut reader: R,
        lobs: &TableDef,
        heap: &Heap,
        pool: &BufferPool,
        wal: &Wal,
        txn_mgr: &TransactionManager,
    ) -> Result<u64> {
        let index = self.index();
        let mut buf = vec![0u8; CHUNK_SIZE];
        let mut chunk_no: i64 = 0;
        let mut total: u64 = 0;
        loop {
            // Fill a full chunk (a single `read` may return short).
            let mut filled = 0usize;
            while filled < CHUNK_SIZE {
                let n = reader.read(&mut buf[filled..]).map_err(DbError::Io)?;
                if n == 0 {
                    break;
                }
                filled += n;
            }
            if filled == 0 {
                break; // EOF on a chunk boundary
            }
            let row = vec![
                Literal::Int(lob_id),
                Literal::Int(chunk_no),
                Literal::Bytea(buf[..filled].to_vec()),
            ];
            let rid = heap.insert(&encode_row(&row), xid, pool, wal)?;
            txn_mgr.record_undo(
                xid,
                UndoAction::Insert {
                    page_id: rid.page_id,
                    slot: rid.slot,
                },
            )?;
            index.insert(OrderedValue::Int(lob_id), rid, pool, wal)?;
            total += filled as u64;
            chunk_no += 1;
            if filled < CHUNK_SIZE {
                break; // short read ⇒ EOF
            }
        }
        let _ = lobs;
        Ok(total)
    }

    /// The chunk `RowId`s of `lob_id`, ordered by `chunk_no`, visible under
    /// `snapshot`. Small (6 bytes each) even for a multi-GB blob, so this is the
    /// only whole-blob-sized allocation — the chunk *data* is never all resident.
    fn ordered_chunk_rids(
        &self,
        lob_id: i64,
        lobs: &TableDef,
        snapshot: &Snapshot,
        xid: Xid,
        pool: &BufferPool,
    ) -> Result<Vec<RowId>> {
        let candidates = self.index().search_eq(&OrderedValue::Int(lob_id), pool)?;
        let heap = Heap::open(self.page_size, lobs.fsm_meta, lobs.pages.clone());
        let mut ordered: Vec<(i64, RowId)> = Vec::new();
        for rid in candidates {
            let bytes = match heap.get(rid, snapshot, xid, pool) {
                Ok(b) => b,
                Err(DbError::NoVisibleVersion { .. }) => continue,
                Err(e) => return Err(e),
            };
            let row = decode_row(&bytes, &lobs.columns)?;
            if let Literal::Int(chunk_no) = row[1] {
                ordered.push((chunk_no, rid));
            }
        }
        ordered.sort_by_key(|(n, _)| *n);
        Ok(ordered.into_iter().map(|(_, r)| r).collect())
    }

    /// Stream a large object out, one chunk at a time, into `sink` — never
    /// holding more than a single chunk in memory. Returns bytes written.
    pub fn read_stream<W: std::io::Write>(
        &self,
        lob_id: i64,
        lobs: &TableDef,
        snapshot: &Snapshot,
        xid: Xid,
        pool: &BufferPool,
        mut sink: W,
    ) -> Result<u64> {
        let rids = self.ordered_chunk_rids(lob_id, lobs, snapshot, xid, pool)?;
        let heap = Heap::open(self.page_size, lobs.fsm_meta, lobs.pages.clone());
        let mut total = 0u64;
        for rid in rids {
            let bytes = match heap.get(rid, snapshot, xid, pool) {
                Ok(b) => b,
                Err(DbError::NoVisibleVersion { .. }) => continue,
                Err(e) => return Err(e),
            };
            let row = decode_row(&bytes, &lobs.columns)?;
            if let Literal::Bytea(data) = &row[2] {
                sink.write_all(data).map_err(DbError::Io)?;
                total += data.len() as u64;
            }
        }
        Ok(total)
    }

    /// Delete every chunk of `lob_id` under `xid` (MVCC delete — the heap vacuum
    /// physically reclaims the dead chunk rows later). Also scrubs the durable
    /// index entries. Returns the number of chunks deleted.
    #[allow(clippy::too_many_arguments)]
    pub fn delete(
        &self,
        xid: Xid,
        lob_id: i64,
        lobs: &TableDef,
        heap: &Heap,
        pool: &BufferPool,
        wal: &Wal,
        lock_mgr: &crate::lockmgr::LockManager,
        txn_mgr: &TransactionManager,
        snapshot: &Snapshot,
    ) -> Result<usize> {
        let rids = self.ordered_chunk_rids(lob_id, lobs, snapshot, xid, pool)?;
        let index = self.index();
        let mut n = 0;
        for rid in rids {
            heap.delete(rid, xid, pool, wal, lock_mgr)?;
            txn_mgr.record_undo(
                xid,
                UndoAction::XmaxStamp {
                    page_id: rid.page_id,
                    slot: rid.slot,
                },
            )?;
            index.remove(&OrderedValue::Int(lob_id), rid, pool, wal)?;
            n += 1;
        }
        Ok(n)
    }
}

/// Idempotently ensure `__lobs__` exists and has a durable `lob_id` index;
/// returns the index meta page. Called once at `Engine::open`, mirroring
/// `graph::edges::ensure_edge_index`.
#[allow(clippy::too_many_arguments)]
pub fn ensure_lobs_table(
    catalog: &mut Catalog,
    txn_mgr: &TransactionManager,
    pool: &BufferPool,
    wal: &Wal,
    lock_mgr: &crate::lockmgr::LockManager,
    control_path: &std::path::Path,
    control: &Mutex<crate::control::ControlData>,
    page_size: usize,
) -> Result<PageId> {
    // Create the table if missing.
    if catalog.lookup(LOBS_TABLE).is_err() {
        let mut cctx = CatalogCtx {
            pool,
            wal,
            control_path,
            control,
            page_size,
        };
        catalog.create_table(lobs_table_def(), &mut cctx)?;
    }

    // Already-built index? Reuse it (no rebuild).
    if let Some(meta) = catalog
        .lookup(LOBS_TABLE)?
        .columns
        .iter()
        .find(|c| c.name == "lob_id")
        .and_then(|c| c.index_root)
    {
        return Ok(meta);
    }

    // First-time creation: build the durable index + backfill any committed
    // chunk rows (empty on a fresh database).
    let tree = DiskBTree::create(pool, wal)?;
    let table = catalog.lookup(LOBS_TABLE)?.clone();
    let heap = Heap::open(page_size, table.fsm_meta, table.pages.clone());
    let xid = txn_mgr.begin(crate::txn::IsolationLevel::ReadCommitted, wal)?;
    let snapshot = txn_mgr.snapshot_for_statement(xid)?;
    for (rid, bytes) in heap.scan(&snapshot, xid, pool)? {
        let row = decode_row(&bytes, &table.columns)?;
        if let Literal::Int(lob_id) = row[0] {
            tree.insert(OrderedValue::Int(lob_id), rid, pool, wal)?;
        }
    }
    txn_mgr.commit(xid, wal, lock_mgr)?;

    let mut cctx = CatalogCtx {
        pool,
        wal,
        control_path,
        control,
        page_size,
    };
    catalog.set_column_index(
        LOBS_TABLE,
        "lob_id",
        Some(crate::catalog::IndexKind::BTree),
        &mut cctx,
    )?;
    catalog.set_column_index_root(LOBS_TABLE, "lob_id", Some(tree.meta_page()), &mut cctx)?;
    Ok(tree.meta_page())
}
