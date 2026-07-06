// unsafe_code is denied crate-wide; mmap.rs is the sole exception (CLAUDE.md §4).
#![deny(unsafe_code)]

pub mod bufferpool;
pub mod catalog;
pub mod checkpoint;
pub mod concurrency_hooks;
pub mod control;
pub mod error;
pub mod format;
pub mod fulltext;
pub mod graph;
pub mod heap;
pub mod index_worker;
pub mod lockmgr;
pub mod mmap;
pub mod mvcc;
pub mod page;
pub mod queue;
pub mod recovery;
pub mod sql;
pub mod txn;
pub mod vector;
pub mod wal;

use std::path::{Path, PathBuf};

use crate::{
    bufferpool::BufferPool,
    catalog::{Catalog, CatalogCtx, ColumnDef, IndexKind},
    control::ControlData,
    error::Result,
    format::{Xid, DEFAULT_PAGE_SIZE},
    graph::{
        edges::{self, Edge},
        executor as graph_executor,
        index::{resolve_candidates_batched, EdgeIndex},
        parser::parse_cypher,
    },
    heap::Heap,
    index_worker::{IndexHandle, IndexMsg},
    lockmgr::LockManager,
    queue::{CONSUMERS_TABLE, EVENTS_TABLE},
    sql::{
        executor::{self, ExecCtx, ExecResult},
        logical::{apply_rls, Expr, Literal},
        parser::parse_sql,
    },
    txn::{IsolationLevel, TransactionManager, UndoAction},
    wal::Wal,
};

pub use crate::error::DbError;
pub use crate::heap::RowId;
pub use crate::sql::executor::ExecResult as SqlResult;
pub use crate::txn::IsolationLevel as Isolation;

const POOL_CAPACITY: usize = 256;

pub struct Engine {
    control: ControlData,
    pool: BufferPool,
    wal: Wal,
    heap: Heap,
    txn_mgr: TransactionManager,
    lock_mgr: LockManager,
    catalog: Catalog,
    control_path: PathBuf,
    _wal_path: PathBuf,
    index_worker: IndexHandle,
    edge_index: EdgeIndex,
    next_event_seq: u64,
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.index_worker.shutdown();
    }
}

impl Engine {
    /// Open (or create) a database at `dir`. Pass `page_size = 0` to use the default.
    pub fn open(dir: &Path, page_size: u32) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let ctrl_p = dir.join("control");
        let data_p = dir.join("data.db");
        let wal_p = dir.join("db.wal");

        let ps = if page_size == 0 {
            DEFAULT_PAGE_SIZE
        } else {
            page_size
        };
        let mut control = control::open_or_create(&ctrl_p, ps)?;
        let page_size_usize = control.page_size as usize;

        // Run recovery before opening normal operation.
        if wal_p.exists() && ctrl_p.exists() {
            recovery::recover(&ctrl_p, &data_p, &wal_p, page_size_usize, POOL_CAPACITY)?;
        }

        let mut pool = BufferPool::open(&data_p, page_size_usize, POOL_CAPACITY)?;
        let wal_tail = control.wal_tail_lsn;
        let mut wal = Wal::open(&wal_p, wal_tail)?;
        let heap = Heap::new(page_size_usize);

        // Resume the xid counter past the highest xid that ever began —
        // reusing an xid would corrupt MVCC visibility for existing tuples
        // (see MEMORY.md's design note).
        let existing_records = if wal_p.exists() {
            Wal::scan_file(&wal_p)?
        } else {
            Vec::new()
        };
        let next_xid = TransactionManager::recover_next_xid(&existing_records);
        let mut txn_mgr = TransactionManager::with_next_xid(next_xid);
        let mut lock_mgr = LockManager::new();

        let mut catalog = Catalog::load(&control, &mut pool)?;

        // `__edges__` always exists after open — before any user transaction
        // begins, so unlike ordinary `CREATE TABLE` there's no "ran inside a
        // transaction that later aborted" gap here (see MEMORY.md's M3.a
        // design note).
        {
            let mut cctx = CatalogCtx {
                pool: &mut pool,
                wal: &mut wal,
                control_path: &ctrl_p,
                control: &mut control,
                page_size: page_size_usize,
            };
            edges::ensure_edges_table(&mut catalog, &mut cctx)?;
            queue::ensure_queue_tables(&mut catalog, &mut cctx)?;
        }
        let edge_index = rebuild_edge_index(
            &catalog,
            &mut txn_mgr,
            &mut pool,
            &mut wal,
            &mut lock_mgr,
            page_size_usize,
        )?;
        let next_event_seq = derive_next_event_seq(
            &catalog,
            &mut txn_mgr,
            &mut pool,
            &mut wal,
            &mut lock_mgr,
            page_size_usize,
        )?;

        let index_worker = IndexHandle::spawn();
        rebuild_secondary_indexes(
            &catalog,
            &mut txn_mgr,
            &mut pool,
            &mut wal,
            &mut lock_mgr,
            page_size_usize,
            &index_worker,
        )?;

        tracing::info!(dir = %dir.display(), page_size = control.page_size, next_xid, "engine opened");
        Ok(Self {
            control,
            pool,
            wal,
            heap,
            txn_mgr,
            lock_mgr,
            catalog,
            control_path: ctrl_p,
            _wal_path: wal_p,
            index_worker,
            edge_index,
            next_event_seq,
        })
    }

    /// Parse and execute one or more `;`-separated SQL statements under
    /// `xid`, applying each table's RLS policy (if any) as a planner
    /// rewrite before execution. Returns one result per statement.
    pub fn execute_sql(&mut self, xid: Xid, sql: &str) -> Result<Vec<ExecResult>> {
        let page_size = self.control.page_size as usize;
        let plans = parse_sql(sql)?;
        let mut results = Vec::with_capacity(plans.len());
        for plan in plans {
            let plan = apply_rls(plan, &self.catalog);
            let mut ctx = ExecCtx {
                catalog: &mut self.catalog,
                txn_mgr: &mut self.txn_mgr,
                pool: &mut self.pool,
                wal: &mut self.wal,
                lock_mgr: &mut self.lock_mgr,
                control_path: &self.control_path,
                control: &mut self.control,
                page_size,
                xid,
                index_worker: Some(&self.index_worker),
                next_event_seq: &mut self.next_event_seq,
            };
            results.push(executor::execute(plan, &mut ctx)?);
        }
        Ok(results)
    }

    /// Parse and execute one Cypher query (M3.c): `MATCH (a)-[:TYPE]->(b)
    /// WHERE <predicate> RETURN <items>`. Mirrors `execute_sql`'s exact
    /// `ExecCtx` construction — single-statement only in v1, but returns
    /// `Vec<ExecResult>` for API symmetry and future multi-statement
    /// headroom.
    pub fn execute_cypher(&mut self, xid: Xid, query: &str) -> Result<Vec<ExecResult>> {
        let page_size = self.control.page_size as usize;
        let parsed = parse_cypher(query)?;
        let mut ctx = ExecCtx {
            catalog: &mut self.catalog,
            txn_mgr: &mut self.txn_mgr,
            pool: &mut self.pool,
            wal: &mut self.wal,
            lock_mgr: &mut self.lock_mgr,
            control_path: &self.control_path,
            control: &mut self.control,
            page_size,
            xid,
            index_worker: Some(&self.index_worker),
            next_event_seq: &mut self.next_event_seq,
        };
        let result = graph_executor::execute(parsed, &mut ctx, &self.edge_index)?;
        Ok(vec![result])
    }

    /// Attach a row-level-security policy to a table (M1: Rust API only,
    /// no `CREATE POLICY` SQL surface — see catalog.rs's module doc).
    pub fn set_rls_policy(&mut self, table: &str, policy: Expr) -> Result<()> {
        let page_size = self.control.page_size as usize;
        let mut ctx = crate::catalog::CatalogCtx {
            pool: &mut self.pool,
            wal: &mut self.wal,
            control_path: &self.control_path,
            control: &mut self.control,
            page_size,
        };
        self.catalog.set_rls_policy(table, policy, &mut ctx)
    }

    /// Attach (or clear) a secondary index on one column (M2: Rust API
    /// only — `CREATE INDEX` SQL surface lands in M2.c). No backfill of
    /// already-committed rows happens here; those get indexed on the next
    /// `Engine::open`'s rebuild-on-open rescan. M2.c's `CREATE INDEX`
    /// backfills immediately instead, reusing this same catalog primitive.
    pub fn set_column_index(
        &mut self,
        table: &str,
        column: &str,
        kind: Option<IndexKind>,
    ) -> Result<()> {
        let page_size = self.control.page_size as usize;
        let mut ctx = crate::catalog::CatalogCtx {
            pool: &mut self.pool,
            wal: &mut self.wal,
            control_path: &self.control_path,
            control: &mut self.control,
            page_size,
        };
        self.catalog.set_column_index(table, column, kind, &mut ctx)
    }

    /// Current build status of a secondary index, or `None` if no index has
    /// ever been built for `(table, column)` (never indexed, or not yet
    /// reached by the worker).
    pub fn index_status(&self, table: &str, column: &str) -> Option<index_worker::IndexStatus> {
        self.index_worker.status(table, column)
    }

    // ── M4.a: event capture opt-in ──────────────────────────────────────────

    /// Opt a table into event capture (M4): from this point on, every
    /// INSERT/UPDATE/DELETE on `table` also durably writes a row to
    /// `__events__` under the same transaction (see
    /// `sql/executor.rs::send_event_capture`). Rejects `__events__`/
    /// `__consumers__` themselves as targets — defense in depth alongside
    /// the same guard in `send_event_capture`, following M2.a's
    /// "validate in more than one place" precedent for `VECTOR(n)`.
    pub fn enable_events(&mut self, table: &str) -> Result<()> {
        if table == EVENTS_TABLE || table == CONSUMERS_TABLE {
            return Err(DbError::SqlPlan(format!(
                "cannot enable events on the system table '{table}' itself"
            )));
        }
        let page_size = self.control.page_size as usize;
        let mut ctx = crate::catalog::CatalogCtx {
            pool: &mut self.pool,
            wal: &mut self.wal,
            control_path: &self.control_path,
            control: &mut self.control,
            page_size,
        };
        self.catalog.set_events_enabled(table, true, &mut ctx)
    }

    /// Fetch up to `limit` events with `seq` greater than `consumer`'s
    /// durable offset, ascending by `seq`. A pure read: an unregistered
    /// consumer is treated as offset 0 in-memory only — no
    /// `__consumers__` row is written here. Only `ack_events` durably
    /// advances a consumer's progress (M4.b), mirroring Kafka's manual-
    /// commit model: if offsets advanced on fetch, a crash between fetch
    /// and the caller actually processing the batch would silently skip
    /// events. No predicate pushdown exists — cost scales with
    /// `__events__`'s total row count, not with consumer lag or `limit`
    /// (see queue/mod.rs's module doc and `Engine::vacuum_events`, M4.c,
    /// which is the actual lever for this cost).
    pub fn poll_events(
        &mut self,
        xid: Xid,
        consumer: &str,
        limit: usize,
    ) -> Result<Vec<queue::Event>> {
        let page_size = self.control.page_size as usize;
        let events_def = self.catalog.lookup(EVENTS_TABLE)?.clone();
        let consumers_def = self.catalog.lookup(CONSUMERS_TABLE)?.clone();
        let events_heap = Heap::from_pages(page_size, events_def.pages.clone());
        let consumers_heap = Heap::from_pages(page_size, consumers_def.pages.clone());
        let snapshot = self.txn_mgr.snapshot_for_statement(xid)?;

        let offset =
            queue::find_consumer_offset(&consumers_heap, &snapshot, xid, &mut self.pool, consumer)?
                .map(|(_, offset)| offset)
                .unwrap_or(0);

        let mut events = Vec::new();
        for (_, bytes) in events_heap.scan(&snapshot, xid, &mut self.pool)? {
            let row = executor::decode_row(&bytes, &events_def.columns)?;
            let (
                Literal::Int(seq),
                Literal::Int(row_xid),
                Literal::Text(table_name),
                Literal::Text(op),
            ) = (&row[0], &row[1], &row[2], &row[3])
            else {
                continue;
            };
            if *seq <= offset {
                continue;
            }
            let payload = match &row[4] {
                Literal::Json(s) => serde_json::from_str(s).unwrap_or(serde_json::Value::Null),
                _ => serde_json::Value::Null,
            };
            events.push(queue::Event {
                seq: *seq,
                xid: *row_xid,
                table_name: table_name.clone(),
                op: op.clone(),
                payload,
            });
        }
        events.sort_by_key(|e| e.seq);
        events.truncate(limit);
        Ok(events)
    }

    /// Durably advance `consumer`'s offset to `up_to_seq` — the only
    /// operation in M4.b that writes to `__consumers__`. If the consumer
    /// has never acked before, this is where its row is created
    /// (auto-registration becomes durable on first ack, not on first
    /// poll).
    pub fn ack_events(&mut self, xid: Xid, consumer: &str, up_to_seq: i64) -> Result<()> {
        let page_size = self.control.page_size as usize;
        let consumers_def = self.catalog.lookup(CONSUMERS_TABLE)?.clone();
        let mut heap = Heap::from_pages(page_size, consumers_def.pages.clone());
        let snapshot = self.txn_mgr.snapshot_for_statement(xid)?;
        let existing =
            queue::find_consumer_offset(&heap, &snapshot, xid, &mut self.pool, consumer)?;

        let encoded = executor::encode_row(&queue::consumer_row(consumer, up_to_seq));
        match existing {
            Some((row_id, _)) => {
                let new_row_id = heap.update(
                    row_id,
                    &encoded,
                    xid,
                    &mut self.pool,
                    &mut self.wal,
                    &mut self.lock_mgr,
                )?;
                self.txn_mgr.record_undo(
                    xid,
                    UndoAction::XmaxStamp {
                        page_id: row_id.page_id,
                        slot: row_id.slot,
                    },
                )?;
                self.txn_mgr.record_undo(
                    xid,
                    UndoAction::Insert {
                        page_id: new_row_id.page_id,
                        slot: new_row_id.slot,
                    },
                )?;
            }
            None => {
                let row_id = heap.insert(&encoded, xid, &mut self.pool, &mut self.wal)?;
                self.txn_mgr.record_undo(
                    xid,
                    UndoAction::Insert {
                        page_id: row_id.page_id,
                        slot: row_id.slot,
                    },
                )?;
            }
        }

        if heap.page_ids() != consumers_def.pages.as_slice() {
            let mut cctx = CatalogCtx {
                pool: &mut self.pool,
                wal: &mut self.wal,
                control_path: &self.control_path,
                control: &mut self.control,
                page_size,
            };
            self.catalog
                .set_pages(CONSUMERS_TABLE, heap.page_ids().to_vec(), &mut cctx)?;
        }
        Ok(())
    }

    // ── M3.a: graph edges ───────────────────────────────────────────────────

    /// Insert one edge record into `__edges__`. Reconstructs its own `Heap`
    /// handle from the catalog's persisted page list — deliberately not
    /// `self.heap`, which has no table concept and backs only the raw
    /// `insert`/`get`/`update`/`delete` API above.
    pub fn create_edge(
        &mut self,
        xid: Xid,
        from_id: i64,
        to_id: i64,
        edge_type: &str,
        props: &str,
    ) -> Result<RowId> {
        let page_size = self.control.page_size as usize;
        let table_def = self.catalog.lookup(edges::EDGES_TABLE)?.clone();
        let mut heap = Heap::from_pages(page_size, table_def.pages.clone());

        let encoded = executor::encode_row(&edges::edge_row(from_id, to_id, edge_type, props));
        let row_id = heap.insert(&encoded, xid, &mut self.pool, &mut self.wal)?;
        self.txn_mgr.record_undo(
            xid,
            UndoAction::Insert {
                page_id: row_id.page_id,
                slot: row_id.slot,
            },
        )?;

        if heap.page_ids() != table_def.pages.as_slice() {
            let mut cctx = CatalogCtx {
                pool: &mut self.pool,
                wal: &mut self.wal,
                control_path: &self.control_path,
                control: &mut self.control,
                page_size,
            };
            self.catalog
                .set_pages(edges::EDGES_TABLE, heap.page_ids().to_vec(), &mut cctx)?;
        }

        self.edge_index.insert(from_id, row_id);
        Ok(row_id)
    }

    /// Delete one edge record. `from_id` is taken as an explicit parameter
    /// (the caller already has it from whatever scan/`edges_from` call
    /// located the row) to avoid a redundant `Heap::get` just to find it.
    pub fn delete_edge(&mut self, xid: Xid, row_id: RowId, from_id: i64) -> Result<()> {
        let page_size = self.control.page_size as usize;
        let table_def = self.catalog.lookup(edges::EDGES_TABLE)?.clone();
        let mut heap = Heap::from_pages(page_size, table_def.pages.clone());

        heap.delete(
            row_id,
            xid,
            &mut self.pool,
            &mut self.wal,
            &mut self.lock_mgr,
        )?;
        self.txn_mgr.record_undo(
            xid,
            UndoAction::XmaxStamp {
                page_id: row_id.page_id,
                slot: row_id.slot,
            },
        )?;
        self.edge_index.remove(from_id, row_id);
        Ok(())
    }

    /// Traverse every edge out of `from_id`, MVCC-filtered against `xid`'s
    /// snapshot. `edge_index` is a candidate-fetcher, not a source of
    /// truth — every candidate `RowId` is re-resolved through the ordinary
    /// MVCC snapshot check (`resolve_candidates_batched`), so an edge whose
    /// creating transaction aborted never surfaces here even though the
    /// index may still reference it.
    pub fn edges_from(&mut self, xid: Xid, from_id: i64) -> Result<Vec<Edge>> {
        let table_def = self.catalog.lookup(edges::EDGES_TABLE)?.clone();
        let snapshot = self.txn_mgr.snapshot_for_statement(xid)?;
        let candidates = self.edge_index.candidates(from_id).to_vec();
        let resolved = resolve_candidates_batched(
            &candidates,
            &snapshot,
            xid,
            &mut self.pool,
            &table_def.columns,
        )?;

        let mut out = Vec::with_capacity(resolved.len());
        for (row_id, row) in resolved {
            let to_id = match &row[1] {
                Literal::Int(n) => *n,
                other => {
                    return Err(DbError::SqlPlan(format!(
                        "__edges__.to_id decoded as non-Int: {other:?}"
                    )))
                }
            };
            let edge_type = match &row[2] {
                Literal::Text(s) => s.clone(),
                other => {
                    return Err(DbError::SqlPlan(format!(
                        "__edges__.edge_type decoded as non-Text: {other:?}"
                    )))
                }
            };
            let props = match &row[3] {
                Literal::Json(s) => s.clone(),
                other => {
                    return Err(DbError::SqlPlan(format!(
                        "__edges__.props decoded as non-Json: {other:?}"
                    )))
                }
            };
            out.push(Edge {
                row_id,
                to_id,
                edge_type,
                props,
            });
        }
        Ok(out)
    }

    /// Begin a new transaction under READ COMMITTED (the default, D10).
    pub fn begin(&mut self) -> Result<Xid> {
        self.begin_with_isolation(IsolationLevel::ReadCommitted)
    }

    pub fn begin_with_isolation(&mut self, isolation: IsolationLevel) -> Result<Xid> {
        self.txn_mgr.begin(isolation, &mut self.wal)
    }

    pub fn commit(&mut self, xid: Xid) -> Result<()> {
        self.txn_mgr.commit(xid, &mut self.wal, &mut self.lock_mgr)
    }

    pub fn abort(&mut self, xid: Xid) -> Result<()> {
        self.txn_mgr.abort(
            xid,
            &mut self.pool,
            &mut self.heap,
            &mut self.wal,
            &mut self.lock_mgr,
        )
    }

    pub fn insert(&mut self, xid: Xid, data: &[u8]) -> Result<RowId> {
        let rid = self.heap.insert(data, xid, &mut self.pool, &mut self.wal)?;
        self.txn_mgr.record_undo(
            xid,
            UndoAction::Insert {
                page_id: rid.page_id,
                slot: rid.slot,
            },
        )?;
        Ok(rid)
    }

    pub fn get(&mut self, xid: Xid, row_id: RowId) -> Result<Vec<u8>> {
        let snapshot = self.txn_mgr.snapshot_for_statement(xid)?;
        self.heap.get(row_id, &snapshot, xid, &mut self.pool)
    }

    /// Update `row_id`, returning the new version's RowId (M1: UPDATE
    /// creates a new tuple version rather than overwriting in place, so the
    /// physical location may change; re-resolve via the returned RowId).
    pub fn update(&mut self, xid: Xid, row_id: RowId, new_data: &[u8]) -> Result<RowId> {
        let new_rid = self.heap.update(
            row_id,
            new_data,
            xid,
            &mut self.pool,
            &mut self.wal,
            &mut self.lock_mgr,
        )?;
        self.txn_mgr.record_undo(
            xid,
            UndoAction::XmaxStamp {
                page_id: row_id.page_id,
                slot: row_id.slot,
            },
        )?;
        self.txn_mgr.record_undo(
            xid,
            UndoAction::Insert {
                page_id: new_rid.page_id,
                slot: new_rid.slot,
            },
        )?;
        Ok(new_rid)
    }

    pub fn delete(&mut self, xid: Xid, row_id: RowId) -> Result<()> {
        self.heap.delete(
            row_id,
            xid,
            &mut self.pool,
            &mut self.wal,
            &mut self.lock_mgr,
        )?;
        self.txn_mgr.record_undo(
            xid,
            UndoAction::XmaxStamp {
                page_id: row_id.page_id,
                slot: row_id.slot,
            },
        )?;
        Ok(())
    }

    pub fn checkpoint(&mut self) -> Result<()> {
        checkpoint::run(
            &mut self.pool,
            &mut self.wal,
            &self.control_path,
            &mut self.control,
        )
    }

    /// Flush all dirty pages without a full checkpoint (used in tests).
    pub fn flush(&mut self) -> Result<()> {
        self.pool.flush_all(self.wal.durable_lsn)
    }
}

/// Rebuild the edge-list index from `__edges__`'s currently-committed rows.
/// Unlike `rebuild_secondary_indexes`, this is entirely synchronous — no
/// worker, no channel — since a `HashMap` insert is O(1) amortized, not
/// M2's O(n log n)-per-upsert HNSW rebuild cost. Uses the same ordinary
/// begin/scan/commit read-only transaction pattern for MVCC-correct
/// visibility of committed rows.
fn rebuild_edge_index(
    catalog: &Catalog,
    txn_mgr: &mut TransactionManager,
    pool: &mut BufferPool,
    wal: &mut Wal,
    lock_mgr: &mut LockManager,
    page_size: usize,
) -> Result<EdgeIndex> {
    let mut index = EdgeIndex::new();
    let table = catalog.lookup(edges::EDGES_TABLE)?;
    let heap = Heap::from_pages(page_size, table.pages.clone());
    let xid = txn_mgr.begin(IsolationLevel::ReadCommitted, wal)?;
    let snapshot = txn_mgr.snapshot_for_statement(xid)?;
    for (row_id, bytes) in heap.scan(&snapshot, xid, pool)? {
        let row = executor::decode_row(&bytes, &table.columns)?;
        if let Literal::Int(from_id) = row[0] {
            index.insert(from_id, row_id);
        }
    }
    txn_mgr.commit(xid, wal, lock_mgr)?;
    Ok(index)
}

/// Derive the next `seq` to assign in `__events__`, from its own
/// currently-committed rows — mirrors `TransactionManager::
/// recover_next_xid`'s "resume past the highest ever seen" approach and
/// `rebuild_edge_index`'s exact begin/scan/commit read-only transaction
/// template. Returns 1 if `__events__` is empty.
fn derive_next_event_seq(
    catalog: &Catalog,
    txn_mgr: &mut TransactionManager,
    pool: &mut BufferPool,
    wal: &mut Wal,
    lock_mgr: &mut LockManager,
    page_size: usize,
) -> Result<u64> {
    let table = catalog.lookup(EVENTS_TABLE)?;
    let heap = Heap::from_pages(page_size, table.pages.clone());
    let xid = txn_mgr.begin(IsolationLevel::ReadCommitted, wal)?;
    let snapshot = txn_mgr.snapshot_for_statement(xid)?;
    let mut max_seq: u64 = 0;
    for (_, bytes) in heap.scan(&snapshot, xid, pool)? {
        let row = executor::decode_row(&bytes, &table.columns)?;
        if let Literal::Int(seq) = row[0] {
            max_seq = max_seq.max(seq as u64);
        }
    }
    txn_mgr.commit(xid, wal, lock_mgr)?;
    Ok(max_seq + 1)
}

/// Scan every table's currently-committed rows for any column carrying an
/// `IndexKind` (`Hnsw` or `FullText`) and enqueue them to the
/// (already-spawned) background worker, so a fresh `Engine::open` ends up
/// with rebuilt indexes rather than empty ones. Runs entirely on the
/// foreground thread against the engine's own `pool`/`heap`/`catalog` — the
/// worker thread itself never gets a `BufferPool` handle (see
/// `index_worker.rs`'s module doc). Uses an ordinary begin/scan/commit
/// read-only transaction, exactly like a `SELECT`, to get MVCC-correct
/// visibility of committed rows. Shares `executor::build_indexed_columns`
/// with `exec_create_index`'s own backfill rather than duplicating the
/// column-type-to-`IndexedColumn` mapping.
fn rebuild_secondary_indexes(
    catalog: &Catalog,
    txn_mgr: &mut TransactionManager,
    pool: &mut BufferPool,
    wal: &mut Wal,
    lock_mgr: &mut LockManager,
    page_size: usize,
    handle: &IndexHandle,
) -> Result<()> {
    for table in catalog.tables() {
        let indexed_cols: Vec<&ColumnDef> =
            table.columns.iter().filter(|c| c.index.is_some()).collect();
        if indexed_cols.is_empty() {
            continue;
        }

        let heap = Heap::from_pages(page_size, table.pages.clone());
        let xid = txn_mgr.begin(IsolationLevel::ReadCommitted, wal)?;
        let snapshot = txn_mgr.snapshot_for_statement(xid)?;
        for (row_id, bytes) in heap.scan(&snapshot, xid, pool)? {
            let row = executor::decode_row(&bytes, &table.columns)?;
            let cols = executor::build_indexed_columns(table, &indexed_cols, &row);
            if !cols.is_empty() {
                handle.send(IndexMsg::Upsert {
                    table: table.name.clone(),
                    record: row_id,
                    indexed_cols: cols,
                });
            }
        }
        txn_mgr.commit(xid, wal, lock_mgr)?;

        for col in &indexed_cols {
            handle.send(IndexMsg::MarkReady {
                table: table.name.clone(),
                column: col.name.clone(),
                kind: col.index.expect("indexed_cols is filtered to Some(_)"),
            });
        }
    }
    Ok(())
}

/// Initialize a `tracing_subscriber` with `RUST_LOG` env filter.
/// Call once at the start of your binary or test suite.
pub fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn open_insert_get_roundtrip() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        let rid = engine.insert(xid, b"hello world").unwrap();
        let data = engine.get(xid, rid).unwrap();
        assert_eq!(data, b"hello world");
        engine.commit(xid).unwrap();
    }

    #[test]
    fn update_and_verify() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        let rid = engine.insert(xid, b"initial_value").unwrap();
        let new_rid = engine.update(xid, rid, b"updated").unwrap();
        assert_eq!(engine.get(xid, new_rid).unwrap(), b"updated");
        engine.commit(xid).unwrap();
    }

    #[test]
    fn delete_makes_row_gone() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        let rid = engine.insert(xid, b"transient").unwrap();
        engine.delete(xid, rid).unwrap();
        assert!(engine.get(xid, rid).is_err());
        engine.commit(xid).unwrap();
    }

    #[test]
    fn reopen_after_flush_recovers_data() {
        let dir = tempdir().unwrap();
        let rid = {
            let mut engine = Engine::open(dir.path(), 0).unwrap();
            let xid = engine.begin().unwrap();
            let rid = engine.insert(xid, b"durable").unwrap();
            engine.commit(xid).unwrap();
            engine.flush().unwrap();
            rid
        };
        let mut engine2 = Engine::open(dir.path(), 0).unwrap();
        let xid2 = engine2.begin().unwrap();
        assert_eq!(engine2.get(xid2, rid).unwrap(), b"durable");
    }

    #[test]
    fn read_committed_sees_other_txns_committed_write() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        let a = engine.begin().unwrap();
        let rid = engine.insert(a, b"v1").unwrap();
        engine.commit(a).unwrap();

        let b = engine.begin().unwrap();
        assert_eq!(engine.get(b, rid).unwrap(), b"v1");
        engine.commit(b).unwrap();
    }

    #[test]
    fn repeatable_read_does_not_see_write_committed_after_begin() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        let a = engine.begin().unwrap();
        let rid = engine.insert(a, b"v1").unwrap();
        engine.commit(a).unwrap();

        // b begins under RR before a's write... actually a already committed
        // above, so instead: b begins RR, then c writes and commits, and b's
        // fixed snapshot must not see c's write even after it commits.
        let b = engine
            .begin_with_isolation(Isolation::RepeatableRead)
            .unwrap();
        assert_eq!(engine.get(b, rid).unwrap(), b"v1"); // sees a's already-committed write

        let c = engine.begin().unwrap();
        let new_rid = engine.update(c, rid, b"v2").unwrap();
        engine.commit(c).unwrap();

        // b's RR snapshot predates c's commit, so it must still see v1 at
        // the original row_id (walking the version chain stops at v1).
        assert_eq!(engine.get(b, rid).unwrap(), b"v1");
        // A fresh READ COMMITTED transaction sees the new committed version.
        let d = engine.begin().unwrap();
        assert_eq!(engine.get(d, new_rid).unwrap(), b"v2");
        engine.commit(b).unwrap();
        engine.commit(d).unwrap();
    }

    #[test]
    fn rollback_undoes_insert() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        let a = engine.begin().unwrap();
        let rid = engine.insert(a, b"oops").unwrap();
        engine.abort(a).unwrap();

        let b = engine.begin().unwrap();
        assert!(engine.get(b, rid).is_err());
    }

    #[test]
    fn xid_counter_survives_reopen() {
        let dir = tempdir().unwrap();
        let first_xid = {
            let mut engine = Engine::open(dir.path(), 0).unwrap();
            let xid = engine.begin().unwrap();
            engine.insert(xid, b"row").unwrap();
            engine.commit(xid).unwrap();
            engine.flush().unwrap();
            xid
        };
        let mut engine2 = Engine::open(dir.path(), 0).unwrap();
        let next_xid = engine2.begin().unwrap();
        assert!(next_xid > first_xid, "reopened engine must not reuse xids");
    }

    // ── M1.b: SI abort-on-conflict (D12) ────────────────────────────────────

    #[test]
    fn concurrent_update_aborts_second_writer_immediately() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        let setup_xid = engine.begin().unwrap();
        let rid = engine.insert(setup_xid, b"row").unwrap();
        engine.commit(setup_xid).unwrap();

        // Two transactions both try to update the same row. Per D12, SI's
        // conflict handling is "abort immediately," not "block and wait" —
        // the second writer must fail right at the write call, not at
        // commit time (see txn.rs::commit's doc comment: because the lock
        // is held for the whole transaction lifetime, there's no separate
        // race window that a commit-time recheck would need to catch).
        let a = engine.begin().unwrap();
        let new_rid = engine.update(a, rid, b"a-wins").unwrap();

        let b = engine.begin().unwrap();
        let err = engine.update(b, rid, b"b-loses");
        assert!(
            matches!(err, Err(DbError::WriteConflict { .. })),
            "second writer must abort immediately on conflict, got {:?}",
            err
        );

        engine.commit(a).unwrap();
        engine.abort(b).unwrap();

        // a's write is the one that stuck.
        let c = engine.begin().unwrap();
        assert_eq!(engine.get(c, new_rid).unwrap(), b"a-wins");
    }

    #[test]
    fn commit_releases_lock_for_next_writer() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        let setup_xid = engine.begin().unwrap();
        let rid = engine.insert(setup_xid, b"row").unwrap();
        engine.commit(setup_xid).unwrap();

        let a = engine.begin().unwrap();
        let new_rid = engine.update(a, rid, b"a-wins").unwrap();
        engine.commit(a).unwrap();

        // Now that a released its lock, a fresh writer can update the
        // *new* version without any conflict.
        let b = engine.begin().unwrap();
        engine.update(b, new_rid, b"b-after-a").unwrap();
        engine.commit(b).unwrap();
    }

    #[test]
    fn abort_releases_lock_for_next_writer() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        let setup_xid = engine.begin().unwrap();
        let rid = engine.insert(setup_xid, b"row").unwrap();
        engine.commit(setup_xid).unwrap();

        let a = engine.begin().unwrap();
        engine.update(a, rid, b"a-abandoned").unwrap();
        engine.abort(a).unwrap();

        // a's abort released the lock (and undid the write), so b can
        // update the still-live original row.
        let b = engine.begin().unwrap();
        engine.update(b, rid, b"b-wins").unwrap();
        engine.commit(b).unwrap();

        let c = engine.begin().unwrap();
        assert!(engine.get(c, rid).is_err()); // superseded by b's update
    }

    // ── M1.c: SQL end-to-end ─────────────────────────────────────────────────

    #[test]
    fn execute_sql_full_round_trip() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();

        let xid = engine.begin().unwrap();
        engine
            .execute_sql(
                xid,
                "CREATE TABLE accounts (id INT, name TEXT, balance INT)",
            )
            .unwrap();
        engine
            .execute_sql(
                xid,
                "INSERT INTO accounts (id, name, balance) VALUES (1, 'alice', 100)",
            )
            .unwrap();
        engine
            .execute_sql(
                xid,
                "INSERT INTO accounts (id, name, balance) VALUES (2, 'bob', 50)",
            )
            .unwrap();
        engine.commit(xid).unwrap();

        let xid2 = engine.begin().unwrap();
        let results = engine
            .execute_sql(xid2, "SELECT * FROM accounts WHERE id = 1")
            .unwrap();
        assert_eq!(results.len(), 1);
        assert!(matches!(&results[0], SqlResult::Rows(rows) if rows.len() == 1));

        engine
            .execute_sql(xid2, "UPDATE accounts SET balance = 200 WHERE id = 1")
            .unwrap();
        let reselect = engine
            .execute_sql(xid2, "SELECT balance FROM accounts WHERE id = 1")
            .unwrap();
        match &reselect[0] {
            SqlResult::Rows(rows) => {
                assert_eq!(rows[0][0], crate::sql::logical::Literal::Int(200))
            }
            other => panic!("expected Rows, got {other:?}"),
        }

        engine
            .execute_sql(xid2, "DELETE FROM accounts WHERE id = 2")
            .unwrap();
        engine.commit(xid2).unwrap();

        let xid3 = engine.begin().unwrap();
        let remaining = engine.execute_sql(xid3, "SELECT * FROM accounts").unwrap();
        assert!(matches!(&remaining[0], SqlResult::Rows(rows) if rows.len() == 1));
    }

    // ── M2.a: VECTOR(n) end-to-end ──────────────────────────────────────────

    #[test]
    fn execute_sql_vector_round_trip() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();

        let xid = engine.begin().unwrap();
        engine
            .execute_sql(xid, "CREATE TABLE t (id INT, embedding VECTOR(4))")
            .unwrap();
        engine
            .execute_sql(
                xid,
                "INSERT INTO t (id, embedding) VALUES (1, [0.1, 0.2, 0.3, 0.4])",
            )
            .unwrap();
        engine.commit(xid).unwrap();

        let xid2 = engine.begin().unwrap();
        let results = engine
            .execute_sql(xid2, "SELECT * FROM t WHERE id = 1")
            .unwrap();
        match &results[0] {
            SqlResult::Rows(rows) => {
                assert_eq!(
                    rows[0][1],
                    crate::sql::logical::Literal::Vector(vec![0.1, 0.2, 0.3, 0.4])
                );
            }
            other => panic!("expected Rows, got {other:?}"),
        }
    }

    #[test]
    fn execute_sql_vector_dimension_mismatch_rejected() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();

        let xid = engine.begin().unwrap();
        engine
            .execute_sql(xid, "CREATE TABLE t (id INT, embedding VECTOR(4))")
            .unwrap();
        let err = engine
            .execute_sql(xid, "INSERT INTO t (id, embedding) VALUES (1, [0.1, 0.2])")
            .unwrap_err();
        assert!(matches!(err, DbError::SqlPlan(_)));
    }

    // ── M2.b: background index worker ───────────────────────────────────────

    fn wait_for_status(
        engine: &Engine,
        table: &str,
        column: &str,
        want: index_worker::IndexStatus,
    ) {
        let start = std::time::Instant::now();
        loop {
            if engine.index_status(table, column) == Some(want) {
                return;
            }
            if start.elapsed() > std::time::Duration::from_secs(2) {
                panic!(
                    "index status for {table}.{column} never reached {want:?}, last seen {:?}",
                    engine.index_status(table, column)
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    #[test]
    fn live_insert_into_indexed_column_enqueues_upsert() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();

        let xid = engine.begin().unwrap();
        engine
            .execute_sql(xid, "CREATE TABLE t (id INT, embedding VECTOR(2))")
            .unwrap();
        engine
            .set_column_index("t", "embedding", Some(crate::catalog::IndexKind::Hnsw))
            .unwrap();
        engine
            .execute_sql(xid, "INSERT INTO t (id, embedding) VALUES (1, [0.1, 0.2])")
            .unwrap();
        engine.commit(xid).unwrap();

        wait_for_status(
            &engine,
            "t",
            "embedding",
            index_worker::IndexStatus::Building { rows_done: 1 },
        );

        let guard = engine.index_worker.indexes.read().unwrap();
        let entry = guard
            .get(&("t".to_string(), "embedding".to_string()))
            .unwrap();
        let index_worker::SecondaryIndex::Vector(v) = &entry.index else {
            panic!("expected a vector index");
        };
        assert_eq!(v.len(), 1);
    }

    #[test]
    fn reopen_rebuilds_index_from_committed_rows() {
        let dir = tempdir().unwrap();
        {
            let mut engine = Engine::open(dir.path(), 0).unwrap();
            let xid = engine.begin().unwrap();
            engine
                .execute_sql(xid, "CREATE TABLE t (id INT, embedding VECTOR(2))")
                .unwrap();
            engine
                .execute_sql(xid, "INSERT INTO t (id, embedding) VALUES (1, [1.0, 1.0])")
                .unwrap();
            engine
                .execute_sql(xid, "INSERT INTO t (id, embedding) VALUES (2, [2.0, 2.0])")
                .unwrap();
            engine.commit(xid).unwrap();
            // Index attached *after* the rows were committed with no live
            // worker watching — proves rebuild-on-open, not live upsert,
            // is what populates the index this time.
            engine
                .set_column_index("t", "embedding", Some(crate::catalog::IndexKind::Hnsw))
                .unwrap();
            engine.flush().unwrap();
        }

        let engine2 = Engine::open(dir.path(), 0).unwrap();
        wait_for_status(&engine2, "t", "embedding", index_worker::IndexStatus::Ready);

        let guard = engine2.index_worker.indexes.read().unwrap();
        let entry = guard
            .get(&("t".to_string(), "embedding".to_string()))
            .unwrap();
        let index_worker::SecondaryIndex::Vector(v) = &entry.index else {
            panic!("expected a vector index");
        };
        assert_eq!(v.len(), 2);
    }

    #[test]
    fn engine_drop_shuts_down_worker_without_hanging() {
        let dir = tempdir().unwrap();
        let engine = Engine::open(dir.path(), 0).unwrap();
        drop(engine); // must return promptly, not hang on the worker thread
    }

    // ── M2.c: CREATE INDEX (full-text) ──────────────────────────────────────

    #[test]
    fn create_index_fulltext_backfills_immediately_and_is_queryable() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();

        let xid = engine.begin().unwrap();
        engine
            .execute_sql(xid, "CREATE TABLE t (id INT, body TEXT)")
            .unwrap();
        engine
            .execute_sql(
                xid,
                "INSERT INTO t (id, body) VALUES (1, 'rust database engine')",
            )
            .unwrap();
        engine
            .execute_sql(
                xid,
                "INSERT INTO t (id, body) VALUES (2, 'python web framework')",
            )
            .unwrap();
        engine.commit(xid).unwrap();

        let xid2 = engine.begin().unwrap();
        let result = engine
            .execute_sql(xid2, "CREATE INDEX idx ON t USING FULLTEXT (body)")
            .unwrap();
        assert_eq!(result[0], SqlResult::CreatedIndex);
        engine.commit(xid2).unwrap();

        // Backfill happens immediately (unlike set_column_index's Rust-API
        // path), so this should reach Ready without needing a reopen.
        wait_for_status(&engine, "t", "body", index_worker::IndexStatus::Ready);

        let guard = engine.index_worker.indexes.read().unwrap();
        let entry = guard.get(&("t".to_string(), "body".to_string())).unwrap();
        let index_worker::SecondaryIndex::FullText(idx) = &entry.index else {
            panic!("expected a full-text index");
        };
        let rust_hits = idx.search("rust");
        let python_hits = idx.search("python");
        assert_eq!(rust_hits.len(), 1);
        assert_eq!(python_hits.len(), 1);
        assert_ne!(rust_hits, python_hits);
        assert!(idx.search("nonexistent").is_empty());
    }

    #[test]
    fn create_index_rejects_type_mismatch_via_sql() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();

        let xid = engine.begin().unwrap();
        engine
            .execute_sql(xid, "CREATE TABLE t (id INT, body TEXT)")
            .unwrap();
        let err = engine
            .execute_sql(xid, "CREATE INDEX idx ON t USING HNSW (body)")
            .unwrap_err();
        assert!(matches!(err, DbError::SqlPlan(_)));
    }

    // ── M2.d: NEAR ───────────────────────────────────────────────────────────

    #[test]
    fn near_query_returns_nearest_neighbors_in_order() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();

        let xid = engine.begin().unwrap();
        engine
            .execute_sql(xid, "CREATE TABLE t (id INT, embedding VECTOR(2))")
            .unwrap();
        engine
            .execute_sql(xid, "CREATE INDEX idx ON t USING HNSW (embedding)")
            .unwrap();
        engine
            .execute_sql(xid, "INSERT INTO t (id, embedding) VALUES (1, [0.0, 0.0])")
            .unwrap();
        engine
            .execute_sql(
                xid,
                "INSERT INTO t (id, embedding) VALUES (2, [100.0, 100.0])",
            )
            .unwrap();
        engine
            .execute_sql(xid, "INSERT INTO t (id, embedding) VALUES (3, [0.1, 0.1])")
            .unwrap();
        engine.commit(xid).unwrap();

        wait_for_status(&engine, "t", "embedding", index_worker::IndexStatus::Ready);

        let xid2 = engine.begin().unwrap();
        let results = engine
            .execute_sql(
                xid2,
                "SELECT id FROM t WHERE NEAR(embedding, [0.0, 0.0], 2)",
            )
            .unwrap();
        match &results[0] {
            SqlResult::Rows(rows) => {
                assert_eq!(rows.len(), 2);
                assert_eq!(rows[0][0], crate::sql::logical::Literal::Int(1));
                assert_eq!(rows[1][0], crate::sql::logical::Literal::Int(3));
            }
            other => panic!("expected Rows, got {other:?}"),
        }
    }

    #[test]
    fn near_composes_with_ordinary_where_predicate() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();

        let xid = engine.begin().unwrap();
        engine
            .execute_sql(
                xid,
                "CREATE TABLE t (id INT, tag TEXT, embedding VECTOR(2))",
            )
            .unwrap();
        engine
            .execute_sql(xid, "CREATE INDEX idx ON t USING HNSW (embedding)")
            .unwrap();
        engine
            .execute_sql(
                xid,
                "INSERT INTO t (id, tag, embedding) VALUES (1, 'a', [0.0, 0.0])",
            )
            .unwrap();
        engine
            .execute_sql(
                xid,
                "INSERT INTO t (id, tag, embedding) VALUES (2, 'b', [0.1, 0.1])",
            )
            .unwrap();
        engine.commit(xid).unwrap();

        wait_for_status(&engine, "t", "embedding", index_worker::IndexStatus::Ready);

        let xid2 = engine.begin().unwrap();
        let results = engine
            .execute_sql(
                xid2,
                "SELECT id FROM t WHERE NEAR(embedding, [0.0, 0.0], 5) AND tag = 'b'",
            )
            .unwrap();
        match &results[0] {
            SqlResult::Rows(rows) => {
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0][0], crate::sql::logical::Literal::Int(2));
            }
            other => panic!("expected Rows, got {other:?}"),
        }
    }

    #[test]
    fn sql_survives_reopen() {
        let dir = tempdir().unwrap();
        {
            let mut engine = Engine::open(dir.path(), 0).unwrap();
            let xid = engine.begin().unwrap();
            engine.execute_sql(xid, "CREATE TABLE t (id INT)").unwrap();
            engine
                .execute_sql(xid, "INSERT INTO t (id) VALUES (7)")
                .unwrap();
            engine.commit(xid).unwrap();
            engine.flush().unwrap();
        }
        let mut engine2 = Engine::open(dir.path(), 0).unwrap();
        let xid = engine2.begin().unwrap();
        let result = engine2.execute_sql(xid, "SELECT * FROM t").unwrap();
        match &result[0] {
            SqlResult::Rows(rows) => assert_eq!(rows.len(), 1),
            other => panic!("expected Rows, got {other:?}"),
        }
    }

    #[test]
    fn rls_policy_filters_rows() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        engine
            .execute_sql(xid, "CREATE TABLE t (id INT, owner TEXT)")
            .unwrap();
        engine
            .execute_sql(xid, "INSERT INTO t (id, owner) VALUES (1, 'alice')")
            .unwrap();
        engine
            .execute_sql(xid, "INSERT INTO t (id, owner) VALUES (2, 'bob')")
            .unwrap();
        engine.commit(xid).unwrap();

        let policy = crate::sql::logical::Expr::BinOp {
            op: crate::sql::logical::CmpOp::Eq,
            lhs: Box::new(crate::sql::logical::Expr::Column("owner".to_string())),
            rhs: Box::new(crate::sql::logical::Expr::Literal(
                crate::sql::logical::Literal::Text("alice".to_string()),
            )),
        };
        engine.set_rls_policy("t", policy).unwrap();

        let xid2 = engine.begin().unwrap();
        let result = engine.execute_sql(xid2, "SELECT * FROM t").unwrap();
        match &result[0] {
            SqlResult::Rows(rows) => assert_eq!(rows.len(), 1),
            other => panic!("expected Rows, got {other:?}"),
        }
    }

    // ── M3.a: graph edges ────────────────────────────────────────────────────

    #[test]
    fn edges_table_exists_and_is_ordinary_sql_queryable() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        engine.create_edge(xid, 1, 2, "KNOWS", "{}").unwrap();
        engine.commit(xid).unwrap();

        let xid2 = engine.begin().unwrap();
        let result = engine
            .execute_sql(xid2, "SELECT * FROM __edges__ WHERE from_id = 1")
            .unwrap();
        match &result[0] {
            SqlResult::Rows(rows) => assert_eq!(rows.len(), 1),
            other => panic!("expected Rows, got {other:?}"),
        }
    }

    #[test]
    fn create_edge_then_edges_from_returns_it() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        engine.create_edge(xid, 1, 2, "KNOWS", "{}").unwrap();
        engine.create_edge(xid, 1, 3, "KNOWS", "{}").unwrap();
        engine.commit(xid).unwrap();

        let xid2 = engine.begin().unwrap();
        let mut edges = engine.edges_from(xid2, 1).unwrap();
        edges.sort_by_key(|e| e.to_id);
        assert_eq!(edges.len(), 2);
        assert_eq!(edges[0].to_id, 2);
        assert_eq!(edges[0].edge_type, "KNOWS");
        assert_eq!(edges[1].to_id, 3);
    }

    #[test]
    fn delete_edge_removes_from_index_and_traversal() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        let row_id = engine.create_edge(xid, 1, 2, "KNOWS", "{}").unwrap();
        engine.commit(xid).unwrap();

        let xid2 = engine.begin().unwrap();
        engine.delete_edge(xid2, row_id, 1).unwrap();
        engine.commit(xid2).unwrap();

        let xid3 = engine.begin().unwrap();
        assert!(engine.edges_from(xid3, 1).unwrap().is_empty());
    }

    #[test]
    fn edges_from_on_from_id_with_no_edges_returns_empty() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        assert!(engine.edges_from(xid, 999).unwrap().is_empty());
    }

    #[test]
    fn edge_index_rebuilds_on_reopen() {
        let dir = tempdir().unwrap();
        {
            let mut engine = Engine::open(dir.path(), 0).unwrap();
            let xid = engine.begin().unwrap();
            engine.create_edge(xid, 1, 2, "KNOWS", "{}").unwrap();
            engine.create_edge(xid, 1, 3, "LIKES", "{}").unwrap();
            engine.commit(xid).unwrap();
            engine.flush().unwrap();
        }

        let mut engine2 = Engine::open(dir.path(), 0).unwrap();
        let xid = engine2.begin().unwrap();
        let edges = engine2.edges_from(xid, 1).unwrap();
        assert_eq!(edges.len(), 2);
    }

    // ── M3.c: Cypher subset ──────────────────────────────────────────────────

    #[test]
    fn execute_cypher_match_where_return_uses_index_fast_path() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        engine.create_edge(xid, 1, 2, "KNOWS", "{}").unwrap();
        engine.create_edge(xid, 1, 3, "KNOWS", "{}").unwrap();
        engine.create_edge(xid, 99, 100, "KNOWS", "{}").unwrap();
        engine.commit(xid).unwrap();

        let xid2 = engine.begin().unwrap();
        let results = engine
            .execute_cypher(xid2, "MATCH (a)-[:KNOWS]->(b) WHERE a = 1 RETURN b")
            .unwrap();
        match &results[0] {
            SqlResult::Rows(rows) => {
                let mut to_ids: Vec<i64> = rows
                    .iter()
                    .map(|r| match &r[0] {
                        Literal::Int(n) => *n,
                        other => panic!("expected Int, got {other:?}"),
                    })
                    .collect();
                to_ids.sort();
                assert_eq!(to_ids, vec![2, 3]);
            }
            other => panic!("expected Rows, got {other:?}"),
        }
    }

    #[test]
    fn execute_cypher_filters_by_edge_type() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        engine.create_edge(xid, 1, 2, "KNOWS", "{}").unwrap();
        engine.create_edge(xid, 1, 3, "LIKES", "{}").unwrap();
        engine.commit(xid).unwrap();

        let xid2 = engine.begin().unwrap();
        let results = engine
            .execute_cypher(xid2, "MATCH (a)-[:LIKES]->(b) WHERE a = 1 RETURN b")
            .unwrap();
        match &results[0] {
            SqlResult::Rows(rows) => {
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0][0], Literal::Int(3));
            }
            other => panic!("expected Rows, got {other:?}"),
        }
    }

    #[test]
    fn execute_cypher_without_from_id_predicate_falls_back_to_full_scan() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        engine.create_edge(xid, 1, 2, "KNOWS", "{}").unwrap();
        engine.create_edge(xid, 5, 6, "KNOWS", "{}").unwrap();
        engine.commit(xid).unwrap();

        // No `a = ...` equality anywhere — `find_from_id_eq` finds nothing,
        // so this must go through the full-`__edges__`-scan fallback path,
        // not the index fast path, and still return every matching edge.
        let xid2 = engine.begin().unwrap();
        let results = engine
            .execute_cypher(xid2, "MATCH (a)-[:KNOWS]->(b) RETURN a, b")
            .unwrap();
        match &results[0] {
            SqlResult::Rows(rows) => assert_eq!(rows.len(), 2),
            other => panic!("expected Rows, got {other:?}"),
        }
    }

    #[test]
    fn execute_cypher_returns_edge_type_and_props() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        engine
            .create_edge(xid, 1, 2, "KNOWS", "{\"since\":2020}")
            .unwrap();
        engine.commit(xid).unwrap();

        let xid2 = engine.begin().unwrap();
        let results = engine
            .execute_cypher(
                xid2,
                "MATCH (a)-[:KNOWS]->(b) WHERE a = 1 RETURN b, type, props",
            )
            .unwrap();
        match &results[0] {
            SqlResult::Rows(rows) => {
                assert_eq!(rows[0][0], Literal::Int(2));
                assert_eq!(rows[0][1], Literal::Text("KNOWS".to_string()));
                assert_eq!(rows[0][2], Literal::Json("{\"since\":2020}".to_string()));
            }
            other => panic!("expected Rows, got {other:?}"),
        }
    }

    #[test]
    fn execute_cypher_rejects_property_access() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        let err = engine
            .execute_cypher(
                xid,
                "MATCH (a)-[:KNOWS]->(b) WHERE a.name = 'alice' RETURN b",
            )
            .unwrap_err();
        assert!(matches!(err, DbError::SqlUnsupported(_)));
    }

    // ── M4.a: event capture foundation ──────────────────────────────────────

    fn events_for_table(engine: &mut Engine, table: &str) -> Vec<Vec<Literal>> {
        let xid = engine.begin().unwrap();
        let results = engine
            .execute_sql(
                xid,
                &format!("SELECT * FROM __events__ WHERE table_name = '{table}'"),
            )
            .unwrap();
        match &results[0] {
            SqlResult::Rows(rows) => rows.clone(),
            other => panic!("expected Rows, got {other:?}"),
        }
    }

    #[test]
    fn queue_tables_exist_and_are_ordinary_sql_queryable() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        let events = engine.execute_sql(xid, "SELECT * FROM __events__").unwrap();
        assert_eq!(events, vec![SqlResult::Rows(vec![])]);
        let consumers = engine
            .execute_sql(xid, "SELECT * FROM __consumers__")
            .unwrap();
        assert_eq!(consumers, vec![SqlResult::Rows(vec![])]);
    }

    #[test]
    fn events_disabled_by_default_produces_no_event_rows() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        engine
            .execute_sql(xid, "CREATE TABLE t (id INT, name TEXT)")
            .unwrap();
        engine
            .execute_sql(xid, "INSERT INTO t (id, name) VALUES (1, 'a')")
            .unwrap();
        engine.commit(xid).unwrap();

        assert!(events_for_table(&mut engine, "t").is_empty());
    }

    #[test]
    fn enable_events_rejects_system_tables() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        assert!(matches!(
            engine.enable_events(queue::EVENTS_TABLE),
            Err(DbError::SqlPlan(_))
        ));
        assert!(matches!(
            engine.enable_events(queue::CONSUMERS_TABLE),
            Err(DbError::SqlPlan(_))
        ));
    }

    #[test]
    fn insert_on_events_enabled_table_captures_one_tagged_event() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        engine
            .execute_sql(xid, "CREATE TABLE t (id INT, name TEXT)")
            .unwrap();
        engine.commit(xid).unwrap();
        engine.enable_events("t").unwrap();

        let xid2 = engine.begin().unwrap();
        engine
            .execute_sql(xid2, "INSERT INTO t (id, name) VALUES (1, 'alice')")
            .unwrap();
        engine.commit(xid2).unwrap();

        let rows = events_for_table(&mut engine, "t");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][3], Literal::Text("insert".to_string()));
        let Literal::Json(payload) = &rows[0][4] else {
            panic!("expected Json payload, got {:?}", rows[0][4]);
        };
        let parsed: serde_json::Value = serde_json::from_str(payload).unwrap();
        assert_eq!(parsed["id"], serde_json::json!(1));
        assert_eq!(parsed["name"], serde_json::json!("alice"));
    }

    #[test]
    fn update_on_events_enabled_table_captures_new_value() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        engine
            .execute_sql(xid, "CREATE TABLE t (id INT, balance INT)")
            .unwrap();
        engine
            .execute_sql(xid, "INSERT INTO t (id, balance) VALUES (1, 100)")
            .unwrap();
        engine.commit(xid).unwrap();
        engine.enable_events("t").unwrap();

        let xid2 = engine.begin().unwrap();
        engine
            .execute_sql(xid2, "UPDATE t SET balance = 200 WHERE id = 1")
            .unwrap();
        engine.commit(xid2).unwrap();

        let rows = events_for_table(&mut engine, "t");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][3], Literal::Text("update".to_string()));
        let Literal::Json(payload) = &rows[0][4] else {
            panic!("expected Json payload");
        };
        let parsed: serde_json::Value = serde_json::from_str(payload).unwrap();
        assert_eq!(parsed["balance"], serde_json::json!(200));
    }

    #[test]
    fn delete_on_events_enabled_table_captures_pre_delete_row() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        engine
            .execute_sql(xid, "CREATE TABLE t (id INT, name TEXT)")
            .unwrap();
        engine
            .execute_sql(xid, "INSERT INTO t (id, name) VALUES (1, 'alice')")
            .unwrap();
        engine.commit(xid).unwrap();
        engine.enable_events("t").unwrap();

        let xid2 = engine.begin().unwrap();
        engine
            .execute_sql(xid2, "DELETE FROM t WHERE id = 1")
            .unwrap();
        engine.commit(xid2).unwrap();

        let rows = events_for_table(&mut engine, "t");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][3], Literal::Text("delete".to_string()));
        let Literal::Json(payload) = &rows[0][4] else {
            panic!("expected Json payload");
        };
        let parsed: serde_json::Value = serde_json::from_str(payload).unwrap();
        assert_eq!(parsed["name"], serde_json::json!("alice"));
    }

    #[test]
    fn aborted_transaction_event_is_self_visible_then_invisible_to_fresh_txn() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        engine.execute_sql(xid, "CREATE TABLE t (id INT)").unwrap();
        engine.commit(xid).unwrap();
        engine.enable_events("t").unwrap();

        let doomed = engine.begin().unwrap();
        engine
            .execute_sql(doomed, "INSERT INTO t (id) VALUES (1)")
            .unwrap();
        let self_view = engine
            .execute_sql(doomed, "SELECT * FROM __events__ WHERE table_name = 't'")
            .unwrap();
        match &self_view[0] {
            SqlResult::Rows(rows) => assert_eq!(
                rows.len(),
                1,
                "inserting transaction must see its own uncommitted event"
            ),
            other => panic!("expected Rows, got {other:?}"),
        }

        engine.abort(doomed).unwrap();

        assert!(
            events_for_table(&mut engine, "t").is_empty(),
            "aborted transaction's event leaked into a fresh transaction's view"
        );
    }

    #[test]
    fn event_seq_derivation_resumes_past_highest_seen_after_reopen() {
        let dir = tempdir().unwrap();
        {
            let mut engine = Engine::open(dir.path(), 0).unwrap();
            let xid = engine.begin().unwrap();
            engine.execute_sql(xid, "CREATE TABLE t (id INT)").unwrap();
            engine.commit(xid).unwrap();
            engine.enable_events("t").unwrap();

            let xid2 = engine.begin().unwrap();
            engine
                .execute_sql(xid2, "INSERT INTO t (id) VALUES (1)")
                .unwrap();
            engine
                .execute_sql(xid2, "INSERT INTO t (id) VALUES (2)")
                .unwrap();
            engine.commit(xid2).unwrap();
            engine.flush().unwrap();
        }

        let mut engine2 = Engine::open(dir.path(), 0).unwrap();
        let xid = engine2.begin().unwrap();
        engine2
            .execute_sql(xid, "INSERT INTO t (id) VALUES (3)")
            .unwrap();
        engine2.commit(xid).unwrap();

        let rows = events_for_table(&mut engine2, "t");
        let mut seqs: Vec<i64> = rows
            .iter()
            .map(|r| match r[0] {
                Literal::Int(n) => n,
                ref other => panic!("expected Int seq, got {other:?}"),
            })
            .collect();
        seqs.sort();
        assert_eq!(seqs, vec![1, 2, 3], "seq must not reuse after reopen");
    }

    // ── M4.b: poll/ack, consumer offsets ────────────────────────────────────

    fn setup_events_table(engine: &mut Engine, n: i64) {
        let xid = engine.begin().unwrap();
        engine.execute_sql(xid, "CREATE TABLE t (id INT)").unwrap();
        engine.commit(xid).unwrap();
        engine.enable_events("t").unwrap();

        let xid2 = engine.begin().unwrap();
        for i in 1..=n {
            engine
                .execute_sql(xid2, &format!("INSERT INTO t (id) VALUES ({i})"))
                .unwrap();
        }
        engine.commit(xid2).unwrap();
    }

    #[test]
    fn poll_events_does_not_advance_offset() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        setup_events_table(&mut engine, 3);

        let xid = engine.begin().unwrap();
        let first = engine.poll_events(xid, "c1", 10).unwrap();
        let second = engine.poll_events(xid, "c1", 10).unwrap();
        assert_eq!(first.len(), 3);
        assert_eq!(second.len(), 3);
        assert_eq!(first, second);
    }

    #[test]
    fn ack_events_advances_offset_so_next_poll_only_returns_newer() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        setup_events_table(&mut engine, 3);

        let xid = engine.begin().unwrap();
        let batch = engine.poll_events(xid, "c1", 10).unwrap();
        assert_eq!(batch.len(), 3);
        let up_to = batch[0].seq;
        engine.ack_events(xid, "c1", up_to).unwrap();
        engine.commit(xid).unwrap();

        let xid2 = engine.begin().unwrap();
        let remaining = engine.poll_events(xid2, "c1", 10).unwrap();
        assert_eq!(remaining.len(), 2);
        assert!(remaining.iter().all(|e| e.seq > up_to));
    }

    #[test]
    fn consumer_offsets_persist_across_reopen() {
        let dir = tempdir().unwrap();
        {
            let mut engine = Engine::open(dir.path(), 0).unwrap();
            setup_events_table(&mut engine, 3);
            let xid = engine.begin().unwrap();
            let batch = engine.poll_events(xid, "c1", 10).unwrap();
            engine.ack_events(xid, "c1", batch[1].seq).unwrap();
            engine.commit(xid).unwrap();
            engine.flush().unwrap();
        }

        let mut engine2 = Engine::open(dir.path(), 0).unwrap();
        let xid = engine2.begin().unwrap();
        let remaining = engine2.poll_events(xid, "c1", 10).unwrap();
        assert_eq!(remaining.len(), 1);
    }

    #[test]
    fn independent_consumers_do_not_interfere() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        setup_events_table(&mut engine, 3);

        let xid = engine.begin().unwrap();
        let batch = engine.poll_events(xid, "c1", 10).unwrap();
        engine.ack_events(xid, "c1", batch[2].seq).unwrap();
        engine.commit(xid).unwrap();

        let xid2 = engine.begin().unwrap();
        let c1_remaining = engine.poll_events(xid2, "c1", 10).unwrap();
        let c2_remaining = engine.poll_events(xid2, "c2", 10).unwrap();
        assert!(c1_remaining.is_empty());
        assert_eq!(c2_remaining.len(), 3);
    }

    #[test]
    fn poll_events_for_unregistered_consumer_starts_at_offset_zero_without_writing() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        setup_events_table(&mut engine, 2);

        let xid = engine.begin().unwrap();
        let batch = engine.poll_events(xid, "never-registered", 10).unwrap();
        assert_eq!(batch.len(), 2);

        let consumers = engine
            .execute_sql(xid, "SELECT * FROM __consumers__")
            .unwrap();
        match &consumers[0] {
            SqlResult::Rows(rows) => assert!(
                rows.is_empty(),
                "poll_events must not write a __consumers__ row"
            ),
            other => panic!("expected Rows, got {other:?}"),
        }
    }
}
