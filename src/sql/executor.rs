// SQL executor (M1.c): runs a `LogicalPlan` row-at-a-time against
// `Heap`/`Catalog`/`TransactionManager`. There is no separate "physical
// plan" IR — M1's grammar subset maps 1:1 from logical plan to execution
// step (single table, no joins), so a distinct physical layer would be a
// premature abstraction; `LogicalPlan` doubles as both once column names
// are resolved against the table's schema at execution time. Vectorized
// scans are explicitly a later concern per the architecture doc.
//
// Row representation on disk: a hand-rolled tag+value encoding per column
// (see `encode_row`/`decode_row`) — actual table rows *are* the page/WAL
// hot path D9's "no serde" rule is about, unlike the catalog's schema
// metadata (which does use serde; see catalog.rs's module doc).
//
// Known gap (tracked, not implemented): RC's EvalPlanQual-style
// re-evaluation path (D12) is not implemented here. UPDATE/DELETE conflicts
// propagate as `WriteConflict` regardless of isolation level — RC's
// "transparently retry against the newest committed data" contract is a
// follow-up once this becomes a real gap in practice, not a blocker for
// M1.c proving SQL works end-to-end.

use std::path::Path;

use serde_json::Value as JsonValue;

use crate::{
    btree_index::{DiskBTree, OrderedValue},
    bufferpool::{BufferPool, PageReader},
    catalog::{Catalog, CatalogCtx, ColumnDef, ColumnType, IndexKind, TableConstraints, TableDef},
    control::ControlData,
    disk_vector::DiskIvfIndex,
    error::{DbError, Result},
    format::{PageId, Xid},
    heap::{Heap, RowId},
    lockmgr::LockManager,
    mvcc::Snapshot,
    queue::{self, EVENTS_TABLE},
    txn::{IsolationLevel, TransactionManager, UndoAction},
    wal::Wal,
};

/// IVF tuning derived at `CREATE INDEX` time. `nlist` ≈ √rows (capped) trades
/// cell granularity against centroid RAM; `nprobe` favors recall (the Phase-3
/// gate) while staying sublinear. Both are stored in the index meta page.
fn ivf_params(nrows: usize) -> (usize, usize) {
    let nlist = ((nrows as f64).sqrt().round() as usize).clamp(1, 256);
    // Probe ~1/8 of the cells, floored so small indexes probe (almost) all cells
    // — i.e. degrade to exact search rather than risk missing the true top-k.
    let nprobe = (nlist / 8).max(8).min(nlist);
    (nlist, nprobe)
}

/// Lloyd's iterations for centroid training at `CREATE INDEX` — a handful
/// suffices for a stable partition (validated in the P3.c recall sweep).
const IVF_TRAIN_ITERS: usize = 8;

use super::datetime;
use super::logical::{CmpOp, Expr, Literal, LogicalPlan};

/// Everything the executor needs, bundled to avoid a long parameter list.
pub struct ExecCtx<'a> {
    pub catalog: &'a mut Catalog,
    pub txn_mgr: &'a mut TransactionManager,
    pub pool: &'a mut BufferPool,
    pub wal: &'a mut Wal,
    pub lock_mgr: &'a mut LockManager,
    pub control_path: &'a Path,
    pub control: &'a mut ControlData,
    pub page_size: usize,
    pub xid: Xid,
    /// Next `seq` to assign in `__events__` (M4). Lives here rather than as
    /// an extra function argument threaded through `execute()` — unlike
    /// M3.c's `edge_index` (needed by exactly one top-level entry point,
    /// `graph_executor::execute`), event capture must reach the deeply
    /// nested private `exec_insert`/`exec_update`/`exec_delete`. Incremented in
    /// place by `send_event_capture` on every captured event.
    pub next_event_seq: &'a mut u64,
}

/// Insert `row`'s durable-index column values into their on-disk structures
/// (P3.a/P3.b/P3.c). Synchronous, durable, WAL-logged on the writer thread —
/// every secondary index is now durable, so this is the single index-maintenance
/// path (the async worker is retired). The kinds differ only in how a row maps to
/// keys — **BTree** (P3.a) uses one key (the column's orderable value);
/// **FullText** (P3.b) uses one key per token of the tokenized text; **Hnsw**
/// (P3.c, the durable on-disk IVF-Flat index) assigns the vector to its nearest
/// cell and inserts `(cell, RowId)` into the cell posting list.
/// Called on every INSERT and on the new row version each UPDATE creates. The
/// old version's entries are left in place: they point at a now-superseded
/// (MVCC-invisible) tuple, so they are harmless stale hints that vacuum later
/// scrubs — exactly how the heap keeps the old version until vacuum. A NULL /
/// non-orderable value is skipped. A column flagged but never built (no
/// `index_root`) is skipped — queries fall back to a full scan, never wrong.
fn apply_durable_index_writes(
    table_def: &TableDef,
    row_id: RowId,
    row: &[Literal],
    ctx: &mut ExecCtx,
) -> Result<()> {
    for (idx, col) in table_def.columns.iter().enumerate() {
        if col.dropped {
            continue;
        }
        let Some(meta_page) = col.index_root else {
            continue;
        };
        match col.index {
            Some(IndexKind::BTree) => {
                if let Ok(value) = OrderedValue::try_from(&row[idx]) {
                    DiskBTree::new(meta_page, ctx.page_size)
                        .insert(value, row_id, ctx.pool, ctx.wal)?;
                }
            }
            Some(IndexKind::FullText) => {
                if let Literal::Text(text) = &row[idx] {
                    let tree = DiskBTree::new(meta_page, ctx.page_size);
                    for token in crate::fulltext::tokenize(text) {
                        tree.insert(OrderedValue::Text(token), row_id, ctx.pool, ctx.wal)?;
                    }
                }
            }
            Some(IndexKind::Hnsw) => {
                if let Literal::Vector(v) = &row[idx] {
                    DiskIvfIndex::open(meta_page, ctx.page_size)
                        .insert(row_id, v, ctx.pool, ctx.wal)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// If `table_def` has events enabled, durably capture one event row in
/// `__events__` under `ctx.xid` — a synchronous `heap.insert` followed
/// immediately by `record_undo`, exactly the two-line shape every other
/// write path in this file already uses. This is the call that makes M4's
/// "zero new abort-path code" claim true: the event row's fate is tied to
/// the surrounding transaction via the same MVCC/abort machinery as any
/// other row, with nothing new added to `txn.rs`. Checked once per
/// statement row, "zero cost if not opted in": this write is synchronous and
/// durable, since the event must commit atomically with the triggering write
/// (see queue/mod.rs's module doc for why a WAL-tailing design was rejected).
fn send_event_capture(
    table_def: &TableDef,
    op: &str,
    row: &[Literal],
    ctx: &mut ExecCtx,
) -> Result<()> {
    if !table_def.events_enabled {
        return Ok(());
    }
    let payload = queue::payload::row_to_json(row, &table_def.columns);
    let events_def = ctx.catalog.lookup(EVENTS_TABLE)?.clone();
    let mut heap = Heap::from_pages(ctx.page_size, events_def.pages.clone());

    let seq = *ctx.next_event_seq;
    *ctx.next_event_seq += 1;

    let encoded = encode_row(&queue::event_row(
        seq as i64,
        ctx.xid as i64,
        &table_def.name,
        op,
        &payload,
    ));
    let row_id = heap.insert(&encoded, ctx.xid, ctx.pool, ctx.wal)?;
    ctx.txn_mgr.record_undo(
        ctx.xid,
        UndoAction::Insert {
            page_id: row_id.page_id,
            slot: row_id.slot,
        },
    )?;

    persist_pages_if_changed(EVENTS_TABLE, &heap, &events_def.pages, ctx)?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExecResult {
    CreatedTable,
    CreatedIndex,
    Inserted {
        count: usize,
    },
    Rows(Vec<Vec<Literal>>),
    Updated {
        count: usize,
    },
    Deleted {
        count: usize,
    },
    /// `ALTER TABLE` succeeded (P2.c).
    AlteredTable,
    /// `DROP TABLE` succeeded (P2.c).
    DroppedTable,
    /// `TRUNCATE` succeeded (P2.c); `count` = rows removed.
    Truncated {
        count: usize,
    },
}

/// Build a `CatalogCtx` from `ExecCtx`'s individual fields (not from `&mut
/// ExecCtx` as a whole) so the borrow checker sees disjoint field borrows —
/// this lets `ctx.catalog` stay independently borrowable at each call site.
macro_rules! catalog_ctx {
    ($ctx:expr) => {
        CatalogCtx {
            pool: $ctx.pool,
            wal: $ctx.wal,
            control_path: $ctx.control_path,
            control: $ctx.control,
            page_size: $ctx.page_size,
        }
    };
}

pub fn execute(plan: LogicalPlan, ctx: &mut ExecCtx) -> Result<ExecResult> {
    match plan {
        LogicalPlan::CreateTable {
            name,
            columns,
            constraints,
        } => exec_create_table(name, columns, constraints, ctx),
        LogicalPlan::Insert {
            table,
            columns,
            values,
        } => exec_insert(&table, columns, values, ctx),
        LogicalPlan::Select {
            table,
            projection,
            predicate,
        } => exec_select(&table, &projection, &predicate, ctx),
        LogicalPlan::Update {
            table,
            assignments,
            predicate,
        } => exec_update(&table, &assignments, &predicate, ctx),
        LogicalPlan::Delete { table, predicate } => exec_delete(&table, &predicate, ctx),
        LogicalPlan::Query(spec) => crate::sql::query_exec::exec_query(&spec, ctx),
        LogicalPlan::CreateIndex {
            table,
            column,
            kind,
        } => exec_create_index(&table, &column, kind, ctx),
        LogicalPlan::AlterTableAddColumn { table, column } => {
            exec_alter_add_column(&table, column, ctx)
        }
        LogicalPlan::AlterTableDropColumn {
            table,
            column,
            if_exists,
        } => exec_alter_drop_column(&table, &column, if_exists, ctx),
        LogicalPlan::DropTable { table, if_exists } => exec_drop_table(&table, if_exists, ctx),
        LogicalPlan::Truncate { table } => exec_truncate(&table, ctx),
    }
}

/// Reject DDL against the engine-managed system tables (`__events__`,
/// `__consumers__`, `__edges__`) — they have no user-facing schema surface.
fn reject_system_table(table: &str) -> Result<()> {
    if table.starts_with("__") {
        return Err(DbError::SqlPlan(format!(
            "'{table}' is an engine-managed system table and cannot be altered or dropped"
        )));
    }
    Ok(())
}

fn exec_alter_add_column(table: &str, column: ColumnDef, ctx: &mut ExecCtx) -> Result<ExecResult> {
    reject_system_table(table)?;
    let mut cctx = catalog_ctx!(ctx);
    ctx.catalog.add_column(table, column, &mut cctx)?;
    Ok(ExecResult::AlteredTable)
}

fn exec_alter_drop_column(
    table: &str,
    column: &str,
    if_exists: bool,
    ctx: &mut ExecCtx,
) -> Result<ExecResult> {
    reject_system_table(table)?;
    let mut cctx = catalog_ctx!(ctx);
    match ctx.catalog.drop_column(table, column, &mut cctx) {
        Ok(()) => Ok(ExecResult::AlteredTable),
        // `IF EXISTS`: a missing column is not an error.
        Err(DbError::ColumnNotFound { .. }) if if_exists => Ok(ExecResult::AlteredTable),
        Err(e) => Err(e),
    }
}

fn exec_drop_table(table: &str, if_exists: bool, ctx: &mut ExecCtx) -> Result<ExecResult> {
    reject_system_table(table)?;
    let mut cctx = catalog_ctx!(ctx);
    match ctx.catalog.drop_table(table, &mut cctx) {
        Ok(()) => Ok(ExecResult::DroppedTable),
        Err(DbError::TableNotFound(_)) if if_exists => Ok(ExecResult::DroppedTable),
        Err(e) => Err(e),
    }
}

fn exec_truncate(table: &str, ctx: &mut ExecCtx) -> Result<ExecResult> {
    reject_system_table(table)?;
    let table_def = ctx.catalog.lookup(table)?.clone();
    // Count the live rows removed (under this statement's snapshot) for the
    // result, before the page list is cleared.
    let heap = Heap::from_pages(ctx.page_size, table_def.pages.clone());
    let snapshot = ctx.txn_mgr.snapshot_for_statement(ctx.xid)?;
    let count = heap.scan(&snapshot, ctx.xid, ctx.pool)?.len();
    let mut cctx = catalog_ctx!(ctx);
    ctx.catalog.truncate(table, &mut cctx)?;
    Ok(ExecResult::Truncated { count })
}

fn exec_create_table(
    name: String,
    columns: Vec<ColumnDef>,
    constraints: TableConstraints,
    ctx: &mut ExecCtx,
) -> Result<ExecResult> {
    // Identity/SERIAL columns (P2.d) must be Int64, and start their counter
    // at 1. Validate + seed here so `create_table` stays a pure catalog op.
    let mut serial_next = std::collections::HashMap::new();
    for col in &columns {
        if col.constraints.identity {
            if col.ty != ColumnType::Int64 {
                return Err(DbError::SqlPlan(format!(
                    "SERIAL/identity column '{}' must be an integer type",
                    col.name
                )));
            }
            serial_next.insert(col.name.clone(), 1);
        }
    }
    let def = TableDef {
        name,
        columns,
        pages: Vec::new(),
        rls_policy: None,
        events_enabled: false,
        serial_next,
        constraints,
    };
    let mut cctx = catalog_ctx!(ctx);
    ctx.catalog.create_table(def, &mut cctx)?;
    Ok(ExecResult::CreatedTable)
}

/// `CREATE INDEX ... ON table USING HNSW|IVF|FULLTEXT|BTREE (column)`: validate
/// the column's type is compatible with the requested index kind, persist the
/// catalog flag, then build a **durable** on-disk index synchronously from every
/// currently-committed row and record its stable meta page id in the catalog.
/// Since P3.c every index kind is durable and WAL-logged, so `Engine::open`
/// never rebuilds it (the O(1)-open moat).
fn exec_create_index(
    table: &str,
    column: &str,
    kind: IndexKind,
    ctx: &mut ExecCtx,
) -> Result<ExecResult> {
    let table_def = ctx.catalog.lookup(table)?.clone();
    let col = table_def
        .columns
        .iter()
        .find(|c| c.name == column)
        .ok_or_else(|| DbError::ColumnNotFound {
            table: table.to_string(),
            column: column.to_string(),
        })?;
    let vec_dim = match (&kind, &col.ty) {
        (IndexKind::Hnsw, ColumnType::Vector(d)) => *d,
        (IndexKind::FullText, ColumnType::Text) => 0,
        (IndexKind::BTree, ColumnType::Int64 | ColumnType::Text | ColumnType::Bool) => 0,
        (kind, ty) => {
            return Err(DbError::SqlPlan(format!(
                "{kind:?} index is not valid on column {column} of type {ty:?}"
            )))
        }
    };

    let mut cctx = catalog_ctx!(ctx);
    ctx.catalog
        .set_column_index(table, column, Some(kind), &mut cctx)?;

    let table_def = ctx.catalog.lookup(table)?.clone();
    let col_idx = table_def
        .columns
        .iter()
        .position(|c| c.name == column)
        .expect("column just validated above");
    let snapshot = ctx.txn_mgr.snapshot_for_statement(ctx.xid)?;

    let meta_page = if matches!(kind, IndexKind::Hnsw) {
        // P3.c: the durable on-disk IVF-Flat vector index. Collect the committed
        // vectors as the training sample, train centroids, then insert each row
        // into its cell. Training holds the sample in RAM transiently (one-time
        // build cost); the persisted index is bounded (centroids only).
        let heap = Heap::from_pages(ctx.page_size, table_def.pages.clone());
        let mut sample: Vec<(RowId, Vec<f32>)> = Vec::new();
        for (row_id, bytes) in heap.scan(&snapshot, ctx.xid, ctx.pool)? {
            let row = decode_row(&bytes, &table_def.columns)?;
            if let Literal::Vector(v) = &row[col_idx] {
                sample.push((row_id, v.clone()));
            }
        }
        let (nlist, nprobe) = ivf_params(sample.len());
        let sample_vecs: Vec<Vec<f32>> = sample.iter().map(|(_, v)| v.clone()).collect();
        let ivf = DiskIvfIndex::create(
            vec_dim as usize,
            &sample_vecs,
            nlist,
            nprobe,
            IVF_TRAIN_ITERS,
            crate::vector::Metric::Euclidean,
            ctx.pool,
            ctx.wal,
        )?;
        for (row_id, v) in &sample {
            ivf.insert(*row_id, v, ctx.pool, ctx.wal)?;
        }
        ivf.meta_page()
    } else {
        // P3.a/P3.b: a durable BTree/FullText index — a `DiskBTree` backfilled
        // from every committed row.
        let tree = DiskBTree::create(ctx.pool, ctx.wal)?;
        let heap = Heap::from_pages(ctx.page_size, table_def.pages.clone());
        for (row_id, bytes) in heap.scan(&snapshot, ctx.xid, ctx.pool)? {
            let row = decode_row(&bytes, &table_def.columns)?;
            match kind {
                IndexKind::BTree => {
                    if let Ok(value) = OrderedValue::try_from(&row[col_idx]) {
                        tree.insert(value, row_id, ctx.pool, ctx.wal)?;
                    }
                }
                IndexKind::FullText => {
                    if let Literal::Text(text) = &row[col_idx] {
                        for token in crate::fulltext::tokenize(text) {
                            tree.insert(OrderedValue::Text(token), row_id, ctx.pool, ctx.wal)?;
                        }
                    }
                }
                _ => unreachable!(),
            }
        }
        tree.meta_page()
    };

    let mut cctx = catalog_ctx!(ctx);
    ctx.catalog
        .set_column_index_root(table, column, Some(meta_page), &mut cctx)?;
    Ok(ExecResult::CreatedIndex)
}

/// Persist a table's page list back to the catalog if the heap grew during
/// this statement's execution.
fn persist_pages_if_changed(
    table: &str,
    heap: &Heap,
    original: &[PageId],
    ctx: &mut ExecCtx,
) -> Result<()> {
    if heap.page_ids() != original {
        let new_pages = heap.page_ids().to_vec();
        let mut cctx = catalog_ctx!(ctx);
        ctx.catalog.set_pages(table, new_pages, &mut cctx)?;
    }
    Ok(())
}

fn exec_insert(
    table: &str,
    columns: Option<Vec<String>>,
    values: Vec<Vec<Literal>>,
    ctx: &mut ExecCtx,
) -> Result<ExecResult> {
    let table_def = ctx.catalog.lookup(table)?.clone();
    // FK (M11): only referenced-table existence is enforced, and it's a
    // schema-level property — check it once per statement, not per row.
    enforce_referenced_tables_exist(&table_def, ctx.catalog)?;
    let mut heap = Heap::from_pages(ctx.page_size, table_def.pages.clone());

    let mut count = 0;
    for row_values in values {
        let ordered = order_values_by_columns(&table_def, &columns, row_values)?;
        // SERIAL/identity fill (P2.d): allocate the next counter value for any
        // identity column whose value was omitted/NULL, before DEFAULT fill.
        let seeded = fill_serials(table, &table_def, ordered, ctx)?;
        // DEFAULT fill happens on INSERT only (never UPDATE), before NOT
        // NULL / CHECK / coercion see the row.
        let filled = apply_defaults(&table_def, seeded);
        let coerced = coerce_and_validate_row(&table_def, filled)?;
        enforce_not_null(&table_def, &coerced)?;
        enforce_checks(&table_def, &coerced)?;
        // UNIQUE (M11): scan under a fresh per-row snapshot so earlier rows
        // inserted by *this same statement* (own writes, visible to own xid)
        // are counted — a duplicate within one multi-row INSERT is caught.
        let snapshot = ctx.txn_mgr.snapshot_for_statement(ctx.xid)?;
        enforce_unique(
            &table_def, &coerced, &heap, &snapshot, ctx.xid, ctx.pool, None,
        )?;
        let encoded = encode_row(&coerced);
        let row_id = heap.insert(&encoded, ctx.xid, ctx.pool, ctx.wal)?;
        ctx.txn_mgr.record_undo(
            ctx.xid,
            UndoAction::Insert {
                page_id: row_id.page_id,
                slot: row_id.slot,
            },
        )?;
        apply_durable_index_writes(&table_def, row_id, &coerced, ctx)?;
        send_event_capture(&table_def, "insert", &coerced, ctx)?;
        count += 1;
    }

    persist_pages_if_changed(table, &heap, &table_def.pages, ctx)?;
    Ok(ExecResult::Inserted { count })
}

fn exec_select(
    table: &str,
    projection: &[String],
    predicate: &Option<Expr>,
    ctx: &mut ExecCtx,
) -> Result<ExecResult> {
    let table_def = ctx.catalog.lookup(table)?.clone();

    if let Some(near) = predicate.as_ref().and_then(find_near) {
        return exec_select_near(&table_def, projection, predicate, near, ctx);
    }

    if let Some(hit) = predicate
        .as_ref()
        .and_then(|e| find_indexable_btree_predicate(e, &table_def))
    {
        if let Some(result) = try_exec_select_btree(&table_def, projection, predicate, hit, ctx)? {
            return Ok(result);
        }
    }

    let heap = Heap::from_pages(ctx.page_size, table_def.pages.clone());
    let snapshot = ctx.txn_mgr.snapshot_for_statement(ctx.xid)?;

    let mut out = Vec::new();
    let mut read_ids = Vec::new();
    for (row_id, bytes) in heap.scan(&snapshot, ctx.xid, ctx.pool)? {
        let row = decode_row(&bytes, &table_def.columns)?;
        if predicate_matches(predicate, &table_def.columns, &row)? {
            // P1.d: this row is part of the statement's read set (an SSI
            // rw-antidependency source). No-op unless `xid` is serializable.
            read_ids.push(row_id);
            out.push(project_row(projection, &table_def.columns, &row)?);
        }
    }
    ctx.txn_mgr.ssi_note_reads(ctx.xid, &read_ids);
    Ok(ExecResult::Rows(out))
}

/// Read-only, `PageReader`-generic SELECT for the concurrent read path (6b).
/// Reuses the same decode/predicate/project helpers as the writer-side
/// `exec_select`, but sources pages from any [`PageReader`] and its snapshot
/// from shared state — no `ExecCtx`, no writer thread. The NEAR / B-Tree index
/// fast paths are intentionally *not* taken here: a plain full scan is always
/// correct, and NEAR (which a full scan cannot answer — it needs the HNSW
/// index) stays on the writer path via [`plan_is_concurrent_read`].
pub(crate) fn exec_select_readonly<P: PageReader>(
    table: &str,
    projection: &[String],
    predicate: &Option<Expr>,
    catalog: &Catalog,
    snapshot: &Snapshot,
    self_xid: Xid,
    reader: &P,
) -> Result<ExecResult> {
    let table_def = catalog.lookup(table)?.clone();
    let heap = Heap::from_pages(reader.page_size(), table_def.pages.clone());
    let mut out = Vec::new();
    for (_, bytes) in heap.scan(snapshot, self_xid, reader)? {
        let row = decode_row(&bytes, &table_def.columns)?;
        if predicate_matches(predicate, &table_def.columns, &row)? {
            out.push(project_row(projection, &table_def.columns, &row)?);
        }
    }
    Ok(ExecResult::Rows(out))
}

/// Whether `plan` may run on the concurrent read path (6b): a plain `SELECT`
/// with no NEAR term. Everything else (writes, DDL, NEAR) routes to the
/// single writer thread, unchanged.
pub(crate) fn plan_is_concurrent_read(plan: &LogicalPlan) -> bool {
    match plan {
        LogicalPlan::Select { predicate, .. } => predicate.as_ref().and_then(find_near).is_none(),
        _ => false,
    }
}

/// Find a top-level (or top-level-AND'd) `Expr::Near` in a predicate.
/// Recurses only through `And`, matching the AND-only `WHERE` grammar — an
/// exhaustive search given there's no `OR`/nesting construct that could
/// hide a `Near` from this walk.
pub(crate) fn find_near(expr: &Expr) -> Option<(&str, &[f32], usize)> {
    match expr {
        Expr::Near { column, query, k } => Some((column.as_str(), query.as_slice(), *k)),
        Expr::And(lhs, rhs) => find_near(lhs).or_else(|| find_near(rhs)),
        _ => None,
    }
}

/// Find a top-level (or top-level-AND'd) `Column <op> Literal` comparison
/// (in either operand order) whose column has a `BTree` index (M6). Purely
/// an optimization hint, unlike `find_near`/`NEAR`: there is no explicit SQL
/// syntax to opt in, and returning `None` here — or `try_exec_select_btree`
/// later declining to use it — just means `exec_select` falls back to its
/// unchanged full-scan path, never an error.
fn find_indexable_btree_predicate<'a>(
    expr: &'a Expr,
    table_def: &TableDef,
) -> Option<(&'a str, CmpOp, &'a Literal)> {
    match expr {
        Expr::BinOp { op, lhs, rhs } => {
            let (column, op, literal) = match (lhs.as_ref(), rhs.as_ref()) {
                (Expr::Column(c), Expr::Literal(l)) => (c.as_str(), *op, l),
                (Expr::Literal(l), Expr::Column(c)) => (c.as_str(), flip_cmp_op(*op), l),
                _ => return None,
            };
            table_def
                .columns
                .iter()
                .find(|c| c.name == column && matches!(c.index, Some(IndexKind::BTree)))
                .map(|_| (column, op, literal))
        }
        Expr::And(lhs, rhs) => find_indexable_btree_predicate(lhs, table_def)
            .or_else(|| find_indexable_btree_predicate(rhs, table_def)),
        _ => None,
    }
}

/// Mirror a comparator when the column/literal appear swapped
/// (`5 < col` means the same thing as `col > 5`).
fn flip_cmp_op(op: CmpOp) -> CmpOp {
    match op {
        CmpOp::Lt => CmpOp::Gt,
        CmpOp::Le => CmpOp::Ge,
        CmpOp::Gt => CmpOp::Lt,
        CmpOp::Ge => CmpOp::Le,
        CmpOp::Eq => CmpOp::Eq,
        CmpOp::Ne => CmpOp::Ne,
    }
}

/// `BTree`-index-assisted execution, following `exec_select_near`'s exact
/// resolve-then-refilter template (candidate RowIds -> `heap.get` -> full
/// `predicate_matches`, so MVCC visibility/RLS/any remaining AND'd WHERE
/// terms all still apply). Returns `Ok(None)` whenever the index can't
/// safely serve this query right now — not yet `Ready` (an in-progress
/// backfill has only seen *some* rows, and treating that as complete would
/// silently return an incomplete result set, unlike `NEAR`'s inherently
/// approximate top-k), the entry's kind mismatches, or the literal being
/// compared isn't orderable — the caller falls back to a full scan in every
/// such case, never an error.
fn try_exec_select_btree(
    table_def: &TableDef,
    projection: &[String],
    predicate: &Option<Expr>,
    hit: (&str, CmpOp, &Literal),
    ctx: &mut ExecCtx,
) -> Result<Option<ExecResult>> {
    let (column, op, literal) = hit;
    let Ok(value) = OrderedValue::try_from(literal) else {
        return Ok(None);
    };
    // P3.a: the durable on-disk B+tree, reconstructed from the column's stable
    // meta page id (no rebuild, no `Ready` status to wait on — the tree is
    // always crash-consistent with committed data). A column flagged BTree but
    // never built (no `index_root`) falls back to a full scan.
    let Some(meta_page) = table_def
        .columns
        .iter()
        .find(|c| c.name == column)
        .and_then(|c| c.index_root)
    else {
        return Ok(None);
    };
    let tree = DiskBTree::new(meta_page, ctx.page_size);
    let candidate_ids: Vec<RowId> = match tree.search(op, &value, ctx.pool)? {
        Some(ids) => ids,
        None => return Ok(None),
    };

    let heap = Heap::from_pages(ctx.page_size, table_def.pages.clone());
    let snapshot = ctx.txn_mgr.snapshot_for_statement(ctx.xid)?;
    let mut out = Vec::new();
    for row_id in candidate_ids {
        let bytes = match heap.get(row_id, &snapshot, ctx.xid, ctx.pool) {
            Ok(b) => b,
            Err(DbError::NoVisibleVersion { .. }) => continue,
            Err(e) => return Err(e),
        };
        let row = decode_row(&bytes, &table_def.columns)?;
        if predicate_matches(predicate, &table_def.columns, &row)? {
            out.push(project_row(projection, &table_def.columns, &row)?);
        }
    }
    Ok(Some(ExecResult::Rows(out)))
}

/// `NEAR`'s over-fetch-then-filter execution: probe the durable IVF-Flat
/// index's nearest-cell posting lists for candidates, resolve each back to a
/// heap row, and run the row through the *same* `predicate_matches` every
/// ordinary scan uses. This is what makes MVCC visibility, RLS, and any
/// AND'd `WHERE` terms apply to `NEAR` results for free — every candidate
/// goes through the identical per-row check a full scan already uses (see
/// `eval_expr`'s `Expr::Near` arm for the other half of this story).
fn exec_select_near(
    table_def: &TableDef,
    projection: &[String],
    predicate: &Option<Expr>,
    near: (&str, &[f32], usize),
    ctx: &mut ExecCtx,
) -> Result<ExecResult> {
    let (column, query, k) = near;
    let col_idx = table_def
        .columns
        .iter()
        .position(|c| c.name == column)
        .ok_or_else(|| DbError::ColumnNotFound {
            table: table_def.name.clone(),
            column: column.to_string(),
        })?;
    let col = &table_def.columns[col_idx];
    if !matches!(col.index, Some(IndexKind::Hnsw)) || !matches!(col.ty, ColumnType::Vector(_)) {
        return Err(DbError::SqlPlan(format!(
            "column {column} has no vector index; see CREATE INDEX ... USING HNSW"
        )));
    }
    // P3.c: the durable on-disk IVF-Flat index, reconstructed from the column's
    // stable meta page id — no rebuild, no `Ready` status to wait on (the index
    // is always crash-consistent with committed data). A column flagged but never
    // built (no `index_root`) has zero candidates, not an error.
    let Some(meta_page) = col.index_root else {
        return Ok(ExecResult::Rows(Vec::new()));
    };

    // Probe the nearest cells' posting lists for candidate RowIds. Candidates are
    // then re-checked against the full predicate below (MVCC visibility, RLS, any
    // AND'd WHERE terms) and exact-re-ranked from the heap's stored vectors, so
    // the over-fetch-then-filter contract is identical to a full scan's per-row
    // check (see `eval_expr`'s `Expr::Near` arm for the other half).
    let ivf = DiskIvfIndex::open(meta_page, ctx.page_size);
    let (metric, candidate_ids) = ivf.candidates(query, None, ctx.pool)?;

    let heap = Heap::from_pages(ctx.page_size, table_def.pages.clone());
    let snapshot = ctx.txn_mgr.snapshot_for_statement(ctx.xid)?;
    let mut scored: Vec<(f32, Vec<Literal>)> = Vec::new();
    for row_id in candidate_ids {
        let bytes = match heap.get(row_id, &snapshot, ctx.xid, ctx.pool) {
            Ok(b) => b,
            // Not visible to this snapshot (superseded, or never committed —
            // e.g. an aborted insert whose durable index entry survives the
            // abort). Filtered out here, not an index-maintenance bug.
            Err(DbError::NoVisibleVersion { .. }) => continue,
            Err(e) => return Err(e),
        };
        let row = decode_row(&bytes, &table_def.columns)?;
        if !predicate_matches(predicate, &table_def.columns, &row)? {
            continue;
        }
        // Exact re-rank distance from the heap's stored vector (IVF-Flat has no
        // quantization error).
        let dist = match &row[col_idx] {
            Literal::Vector(v) => ivf_exact_distance(metric, query, v),
            _ => continue,
        };
        scored.push((dist, project_row(projection, &table_def.columns, &row)?));
    }
    scored.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(k);
    Ok(ExecResult::Rows(
        scored.into_iter().map(|(_, r)| r).collect(),
    ))
}

/// Exact distance for NEAR re-ranking, matching the index's `Metric` (must agree
/// with `disk_vector`'s internal distance so the re-rank is consistent with cell
/// assignment).
fn ivf_exact_distance(metric: crate::vector::Metric, a: &[f32], b: &[f32]) -> f32 {
    use crate::vector::Metric;
    match metric {
        Metric::Euclidean => a
            .iter()
            .zip(b)
            .map(|(x, y)| (x - y) * (x - y))
            .sum::<f32>()
            .sqrt(),
        Metric::Cosine => {
            let mut dot = 0.0f32;
            let mut na = 0.0f32;
            let mut nb = 0.0f32;
            for (x, y) in a.iter().zip(b) {
                dot += x * y;
                na += x * x;
                nb += y * y;
            }
            if na == 0.0 || nb == 0.0 {
                return 1.0;
            }
            1.0 - dot / (na.sqrt() * nb.sqrt())
        }
    }
}

fn exec_update(
    table: &str,
    assignments: &[(String, Expr)],
    predicate: &Option<Expr>,
    ctx: &mut ExecCtx,
) -> Result<ExecResult> {
    let table_def = ctx.catalog.lookup(table)?.clone();
    enforce_referenced_tables_exist(&table_def, ctx.catalog)?;
    let mut heap = Heap::from_pages(ctx.page_size, table_def.pages.clone());
    let snapshot = ctx.txn_mgr.snapshot_for_statement(ctx.xid)?;

    let matching = matching_rows(&heap, &snapshot, ctx, &table_def, predicate)?;
    // P1.d: the rows an UPDATE selects are part of its read set (SSI).
    let read_ids: Vec<RowId> = matching.iter().map(|(rid, _)| *rid).collect();
    ctx.txn_mgr.ssi_note_reads(ctx.xid, &read_ids);

    let mut count = 0;
    for (row_id, mut row) in matching {
        for (col, expr) in assignments {
            let new_val = eval_expr(expr, &table_def.columns, &row)?;
            set_column(&table_def.columns, &mut row, col, new_val)?;
        }
        let coerced = coerce_and_validate_row(&table_def, row)?;
        enforce_not_null(&table_def, &coerced)?;
        enforce_checks(&table_def, &coerced)?;
        // UNIQUE (M11): exclude the row's *current* version — the old tuple
        // is still visible to this snapshot until `heap.update` supersedes
        // it, so without the exclusion an unchanged unique value would
        // collide with itself.
        let usnap = ctx.txn_mgr.snapshot_for_statement(ctx.xid)?;
        enforce_unique(
            &table_def,
            &coerced,
            &heap,
            &usnap,
            ctx.xid,
            ctx.pool,
            Some(row_id),
        )?;
        let encoded = encode_row(&coerced);
        let new_row_id =
            match heap.update(row_id, &encoded, ctx.xid, ctx.pool, ctx.wal, ctx.lock_mgr) {
                Err(e @ DbError::WriteConflict { .. }) => return Err(classify_conflict(e, ctx)),
                other => other?,
            };
        // P1.d: writing supersedes the version at `row_id` — an SSI write of the
        // exact version a concurrent reader would have read.
        ctx.txn_mgr.ssi_note_write(ctx.xid, row_id);
        ctx.txn_mgr.record_undo(
            ctx.xid,
            UndoAction::XmaxStamp {
                page_id: row_id.page_id,
                slot: row_id.slot,
            },
        )?;
        ctx.txn_mgr.record_undo(
            ctx.xid,
            UndoAction::Insert {
                page_id: new_row_id.page_id,
                slot: new_row_id.slot,
            },
        )?;
        apply_durable_index_writes(&table_def, new_row_id, &coerced, ctx)?;
        send_event_capture(&table_def, "update", &coerced, ctx)?;
        count += 1;
    }

    persist_pages_if_changed(table, &heap, &table_def.pages, ctx)?;
    Ok(ExecResult::Updated { count })
}

fn exec_delete(table: &str, predicate: &Option<Expr>, ctx: &mut ExecCtx) -> Result<ExecResult> {
    let table_def = ctx.catalog.lookup(table)?.clone();
    let mut heap = Heap::from_pages(ctx.page_size, table_def.pages.clone());
    let snapshot = ctx.txn_mgr.snapshot_for_statement(ctx.xid)?;

    let matching = matching_rows(&heap, &snapshot, ctx, &table_def, predicate)?;
    // P1.d: the rows a DELETE selects are part of its read set (SSI).
    let read_ids: Vec<RowId> = matching.iter().map(|(rid, _)| *rid).collect();
    ctx.txn_mgr.ssi_note_reads(ctx.xid, &read_ids);

    let mut count = 0;
    for (row_id, row) in matching {
        // Captured before `heap.delete` runs — once the row is deleted
        // there is nothing left to build a payload from.
        send_event_capture(&table_def, "delete", &row, ctx)?;
        if let Err(e @ DbError::WriteConflict { .. }) =
            heap.delete(row_id, ctx.xid, ctx.pool, ctx.wal, ctx.lock_mgr)
        {
            return Err(classify_conflict(e, ctx));
        }
        // P1.d: deleting supersedes the version at `row_id` (SSI write).
        ctx.txn_mgr.ssi_note_write(ctx.xid, row_id);
        ctx.txn_mgr.record_undo(
            ctx.xid,
            UndoAction::XmaxStamp {
                page_id: row_id.page_id,
                slot: row_id.slot,
            },
        )?;
        count += 1;
    }

    persist_pages_if_changed(table, &heap, &table_def.pages, ctx)?;
    Ok(ExecResult::Deleted { count })
}

/// Classify a `Heap` write-write `WriteConflict` by isolation level (P1.d):
/// under `REPEATABLE READ` / `SERIALIZABLE` it is a serialization anomaly the
/// transaction cannot proceed through, surfaced as `SerializationFailure` (the
/// caller should retry). Under `READ COMMITTED` it is left as-is — the only way
/// this fires at RC is a genuine no-wait conflict against a *still-active*
/// writer (a committed superseder is instead re-read by RC's fresh per-
/// statement snapshot, so no spurious abort). True blocking-then-EvalPlanQual
/// for that active-writer case needs a lock wait queue (Phase 5).
fn classify_conflict(err: DbError, ctx: &ExecCtx) -> DbError {
    match ctx.txn_mgr.isolation(ctx.xid) {
        Some(IsolationLevel::RepeatableRead) | Some(IsolationLevel::Serializable) => {
            DbError::SerializationFailure { xid: ctx.xid }
        }
        _ => err,
    }
}

fn matching_rows(
    heap: &Heap,
    snapshot: &crate::mvcc::Snapshot,
    ctx: &mut ExecCtx,
    table_def: &TableDef,
    predicate: &Option<Expr>,
) -> Result<Vec<(RowId, Vec<Literal>)>> {
    heap.scan(snapshot, ctx.xid, ctx.pool)?
        .into_iter()
        .map(|(row_id, bytes)| Ok((row_id, decode_row(&bytes, &table_def.columns)?)))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .filter_map(
            |(row_id, row)| match predicate_matches(predicate, &table_def.columns, &row) {
                Ok(true) => Some(Ok((row_id, row))),
                Ok(false) => None,
                Err(e) => Some(Err(e)),
            },
        )
        .collect()
}

// ── row value handling ──────────────────────────────────────────────────────

fn order_values_by_columns(
    table: &TableDef,
    columns: &Option<Vec<String>>,
    values: Vec<Literal>,
) -> Result<Vec<Literal>> {
    match columns {
        // `INSERT INTO t VALUES (...)` — one value per *visible* column, in
        // declaration order. Dropped columns (P2.c tombstones) hold their slot
        // in the physical row but are always written NULL.
        None => {
            let visible = table.columns.iter().filter(|c| !c.dropped).count();
            if values.len() != visible {
                return Err(DbError::SqlPlan(format!(
                    "table '{}' has {} columns, but {} values were supplied",
                    table.name,
                    visible,
                    values.len()
                )));
            }
            let mut ordered = vec![Literal::Null; table.columns.len()];
            let mut vals = values.into_iter();
            for (slot, col) in ordered.iter_mut().zip(&table.columns) {
                if !col.dropped {
                    *slot = vals.next().expect("visible count checked above");
                }
            }
            Ok(ordered)
        }
        Some(cols) => {
            if cols.len() != values.len() {
                return Err(DbError::SqlPlan(
                    "INSERT column list and VALUES count don't match".into(),
                ));
            }
            let mut ordered = vec![Literal::Null; table.columns.len()];
            for (col_name, val) in cols.iter().zip(values) {
                let idx = column_index(table, col_name)?;
                ordered[idx] = val;
            }
            Ok(ordered)
        }
    }
}

fn column_index(table: &TableDef, name: &str) -> Result<usize> {
    table
        .columns
        .iter()
        .position(|c| c.name == name && !c.dropped)
        .ok_or_else(|| DbError::ColumnNotFound {
            table: table.name.clone(),
            column: name.to_string(),
        })
}

fn coerce_and_validate_row(table: &TableDef, values: Vec<Literal>) -> Result<Vec<Literal>> {
    table
        .columns
        .iter()
        .zip(values)
        .map(|(col, val)| coerce_value(&table.name, col, val))
        .collect()
}

fn coerce_value(table_name: &str, col: &ColumnDef, val: Literal) -> Result<Literal> {
    match (&col.ty, val) {
        (_, Literal::Null) => Ok(Literal::Null),
        (ColumnType::Int64, Literal::Int(n)) => Ok(Literal::Int(n)),
        (ColumnType::Text, Literal::Text(s)) => Ok(Literal::Text(s)),
        (ColumnType::Bool, Literal::Bool(b)) => Ok(Literal::Bool(b)),
        (ColumnType::Json, Literal::Text(s)) | (ColumnType::Json, Literal::Json(s)) => {
            serde_json::from_str::<JsonValue>(&s).map_err(|e| {
                DbError::SqlPlan(format!("invalid JSON for column '{}': {e}", col.name))
            })?;
            Ok(Literal::Json(s))
        }
        (ColumnType::Vector(n), Literal::Vector(v)) => {
            if v.len() != *n as usize {
                return Err(DbError::SqlPlan(format!(
                    "table '{table_name}' column '{}': expected a {n}-dimension vector, got {}",
                    col.name,
                    v.len()
                )));
            }
            Ok(Literal::Vector(v))
        }
        // Exact decimal (P2.a): rescale the literal to the column's declared
        // scale and precision. A plain integer literal is treated as a
        // scale-0 decimal so `INSERT ... VALUES (100)` fills a money column.
        (ColumnType::Decimal(p, s), Literal::Decimal(value, from_scale)) => {
            rescale_decimal(table_name, col, value, from_scale, *p, *s)
        }
        (ColumnType::Decimal(p, s), Literal::Int(n)) => {
            rescale_decimal(table_name, col, n as i128, 0, *p, *s)
        }
        (ColumnType::Timestamp, Literal::Timestamp(t)) => Ok(Literal::Timestamp(t)),
        // A timestamp arrives from SQL as a string literal — the parser has no
        // schema to know it is temporal, so it is coerced here.
        (ColumnType::Timestamp, Literal::Text(s)) => {
            Ok(Literal::Timestamp(datetime::parse_timestamp(&s)?))
        }
        // Float (P2.b): accept a float, an integer, or an exact decimal literal
        // — the last two widen into IEEE-754 (money literals in a FLOAT column).
        (ColumnType::Float, Literal::Float(f)) => Ok(Literal::Float(f)),
        (ColumnType::Float, Literal::Int(n)) => Ok(Literal::Float(n as f64)),
        (ColumnType::Float, Literal::Decimal(v, s)) => {
            Ok(Literal::Float(v as f64 / 10f64.powi(s as i32)))
        }
        // UUID / BYTEA / DATE / TIME arrive as string literals, parsed here.
        (ColumnType::Uuid, Literal::Uuid(b)) => Ok(Literal::Uuid(b)),
        (ColumnType::Uuid, Literal::Text(s)) => Ok(Literal::Uuid(parse_uuid(&s)?)),
        (ColumnType::Bytea, Literal::Bytea(b)) => Ok(Literal::Bytea(b)),
        (ColumnType::Bytea, Literal::Text(s)) => Ok(Literal::Bytea(parse_bytea(&s))),
        (ColumnType::Date, Literal::Date(d)) => Ok(Literal::Date(d)),
        (ColumnType::Date, Literal::Text(s)) => Ok(Literal::Date(datetime::parse_date(&s)?)),
        (ColumnType::Time, Literal::Time(t)) => Ok(Literal::Time(t)),
        (ColumnType::Time, Literal::Text(s)) => Ok(Literal::Time(datetime::parse_time(&s)?)),
        (expected, got) => Err(DbError::SqlPlan(format!(
            "table '{table_name}' column '{}': expected {expected:?}, got {got:?}",
            col.name
        ))),
    }
}

/// Parse UUID text — canonical hyphenated `8-4-4-4-12` or a bare 32 hex
/// digits — into 16 raw bytes (P2.b). Case-insensitive; rejects anything else.
fn parse_uuid(s: &str) -> Result<[u8; 16]> {
    let hex: String = s.chars().filter(|&c| c != '-').collect();
    if hex.len() != 32 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(DbError::SqlPlan(format!("invalid UUID literal: {s:?}")));
    }
    let mut out = [0u8; 16];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
            .map_err(|_| DbError::SqlPlan(format!("invalid UUID literal: {s:?}")))?;
    }
    Ok(out)
}

/// Render 16 bytes as canonical lowercase hyphenated UUID text (P2.b).
pub fn format_uuid(b: &[u8; 16]) -> String {
    let h: String = b.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

/// Interpret a BYTEA string literal (P2.b): a `\x`-prefixed hex string decodes
/// to those bytes (Postgres hex format); anything else is taken as the
/// string's raw UTF-8 bytes. A malformed `\x` body falls back to raw bytes
/// rather than erroring — BYTEA input is deliberately permissive.
fn parse_bytea(s: &str) -> Vec<u8> {
    if let Some(hex) = s.strip_prefix("\\x").or_else(|| s.strip_prefix("\\X")) {
        if hex.len() % 2 == 0 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            if let Some(bytes) = (0..hex.len() / 2)
                .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok())
                .collect::<Option<Vec<u8>>>()
            {
                return bytes;
            }
        }
    }
    s.as_bytes().to_vec()
}

/// Render bytes as a Postgres-style `\x`-prefixed hex string (P2.b) for the
/// JSON/DTO boundary.
pub fn format_bytea(b: &[u8]) -> String {
    let mut out = String::with_capacity(2 + b.len() * 2);
    out.push_str("\\x");
    for byte in b {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// `10^n` as an `i128`, or `None` on overflow (n up to 38 fits; beyond does
/// not). Small, exact, and dependency-free — the DECIMAL scaling primitive.
fn pow10(n: u8) -> Option<i128> {
    let mut acc: i128 = 1;
    for _ in 0..n {
        acc = acc.checked_mul(10)?;
    }
    Some(acc)
}

/// Number of significant decimal digits in `|value|` (0 has 1 digit here,
/// which is fine: a scale-`s` zero always fits any `precision >= s`).
fn digit_count(value: i128) -> u32 {
    let mut v = value.unsigned_abs();
    if v == 0 {
        return 1;
    }
    let mut n = 0;
    while v > 0 {
        v /= 10;
        n += 1;
    }
    n
}

/// Rescale a decimal literal `(value, from_scale)` to the column's declared
/// `(precision, scale)`, exactly — never rounding. Widening the scale
/// multiplies; narrowing is allowed only when the dropped digits are all zero
/// (`9.90` -> scale 1 = `9.9`, but `9.99` -> scale 1 is rejected). Enforces
/// the precision cap after rescaling so an out-of-range value fails the write
/// rather than being silently stored.
fn rescale_decimal(
    table_name: &str,
    col: &ColumnDef,
    value: i128,
    from_scale: u8,
    precision: u8,
    scale: u8,
) -> Result<Literal> {
    let overflow = || {
        DbError::SqlPlan(format!(
            "table '{table_name}' column '{}': decimal value out of range",
            col.name
        ))
    };
    let scaled = if from_scale == scale {
        value
    } else if from_scale < scale {
        let factor = pow10(scale - from_scale).ok_or_else(overflow)?;
        value.checked_mul(factor).ok_or_else(overflow)?
    } else {
        let factor = pow10(from_scale - scale).ok_or_else(overflow)?;
        if value % factor != 0 {
            return Err(DbError::SqlPlan(format!(
                "table '{table_name}' column '{}': value has more fractional digits than the column's scale {scale} allows",
                col.name
            )));
        }
        value / factor
    };
    if digit_count(scaled) > precision as u32 {
        return Err(DbError::SqlPlan(format!(
            "table '{table_name}' column '{}': value exceeds DECIMAL({precision}, {scale}) precision",
            col.name
        )));
    }
    Ok(Literal::Decimal(scaled, scale))
}

// ── constraint enforcement (M11) ─────────────────────────────────────────────
//
// Reuses machinery that already exists rather than adding new subsystems:
// UNIQUE scans the heap under the caller's MVCC snapshot (so it sees the
// transaction's own uncommitted writes, exactly like any read); CHECK runs
// through the same `eval_expr` predicate evaluator SELECT/WHERE use; DEFAULT
// fill and NOT NULL are plain per-column value checks.
//
// UNIQUE deliberately does NOT consult the B-Tree index, even when one exists on
// the column. A secondary-index entry is only ever a *hint* re-validated against
// MVCC downstream (it carries no visibility, and stale entries from aborted/
// superseded versions persist until vacuum), so trusting it for a *correctness*
// check could both miss a just-inserted duplicate and false-positive on a stale
// entry. A synchronous heap scan under the caller's snapshot is the only source
// guaranteed current for the writing transaction, so uniqueness is enforced
// against the heap; the B-Tree index stays a read-side query accelerator only.

/// Fill any NULL column value that has a `DEFAULT` with that default
/// (INSERT-only). We can't distinguish an explicitly-supplied `NULL` from an
/// omitted column once values are positionally ordered, so DEFAULT applies to
/// any NULL — a minor, documented divergence from strict SQL (`INSERT ...
/// VALUES (NULL)` into a defaulted column fills the default here).
/// Fill any identity/SERIAL column whose value is NULL (omitted) with the next
/// value from the table's durable counter (P2.d). An explicitly-supplied value
/// is left as-is (the counter is not advanced past it — matching Postgres
/// `SERIAL`, a documented sharp edge). Allocation persists the catalog, so the
/// sequence survives a crash/reopen at the last-handed-out value.
fn fill_serials(
    table: &str,
    table_def: &TableDef,
    mut row: Vec<Literal>,
    ctx: &mut ExecCtx,
) -> Result<Vec<Literal>> {
    for (i, col) in table_def.columns.iter().enumerate() {
        if col.constraints.identity && !col.dropped && matches!(row[i], Literal::Null) {
            let mut cctx = catalog_ctx!(ctx);
            let value = ctx.catalog.alloc_serial(table, &col.name, &mut cctx)?;
            row[i] = Literal::Int(value);
        }
    }
    Ok(row)
}

fn apply_defaults(table_def: &TableDef, mut row: Vec<Literal>) -> Vec<Literal> {
    for (i, col) in table_def.columns.iter().enumerate() {
        if col.dropped {
            continue;
        }
        if matches!(row[i], Literal::Null) {
            if let Some(default) = &col.constraints.default {
                row[i] = default.clone();
            }
        }
    }
    row
}

/// NOT NULL (and the NOT-NULL half of PRIMARY KEY).
fn enforce_not_null(table_def: &TableDef, row: &[Literal]) -> Result<()> {
    for (i, col) in table_def.columns.iter().enumerate() {
        if col.dropped {
            continue;
        }
        let required = col.constraints.not_null || col.constraints.primary_key;
        if required && matches!(row[i], Literal::Null) {
            return Err(DbError::NotNullViolation {
                table: table_def.name.clone(),
                column: col.name.clone(),
            });
        }
    }
    Ok(())
}

/// Column-level and table-level CHECK constraints.
///
/// CHECK inherits the engine's two-valued NULL semantics (see `compare`): a
/// comparison with a NULL operand evaluates to a non-true result and so
/// fails the check, which is stricter than standard SQL's "NULL ⇒ unknown ⇒
/// passes." Pair a CHECK with NOT NULL / DEFAULT if a nullable column must be
/// allowed to skip it. Documented limitation, consistent with WHERE/RLS.
fn enforce_checks(table_def: &TableDef, row: &[Literal]) -> Result<()> {
    let columns = &table_def.columns;
    let check_one = |expr: &Expr| -> Result<()> {
        if check_passes(expr, columns, row)? {
            Ok(())
        } else {
            Err(DbError::CheckViolation {
                table: table_def.name.clone(),
            })
        }
    };
    for col in columns {
        if col.dropped {
            continue;
        }
        if let Some(check) = &col.constraints.check {
            check_one(check)?;
        }
    }
    for check in &table_def.constraints.checks {
        check_one(check)?;
    }
    Ok(())
}

fn check_passes(expr: &Expr, columns: &[ColumnDef], row: &[Literal]) -> Result<bool> {
    match eval_expr(expr, columns, row)? {
        Literal::Bool(b) => Ok(b),
        // A NULL-valued CHECK expression is treated as passing (the closest
        // this two-valued evaluator gets to SQL's "unknown ⇒ pass").
        Literal::Null => Ok(true),
        other => Err(DbError::SqlPlan(format!(
            "CHECK expression must be boolean, got {other:?}"
        ))),
    }
}

/// Foreign-key enforcement (M11): referenced-table existence only — see
/// [`crate::catalog::ForeignKeyRef`]'s doc for the scope rationale.
fn enforce_referenced_tables_exist(table_def: &TableDef, catalog: &Catalog) -> Result<()> {
    let check = |ref_table: &str| -> Result<()> {
        if catalog.lookup(ref_table).is_err() {
            return Err(DbError::ForeignKeyViolation {
                table: table_def.name.clone(),
                ref_table: ref_table.to_string(),
            });
        }
        Ok(())
    };
    for col in &table_def.columns {
        if let Some(fk) = &col.constraints.references {
            check(&fk.table)?;
        }
    }
    for fk in &table_def.constraints.foreign_keys {
        check(&fk.ref_table)?;
    }
    Ok(())
}

/// Every UNIQUE column-set on the table: each column-level UNIQUE/PRIMARY KEY
/// as a one-column set, the table-level PRIMARY KEY as one set, and each
/// table-level UNIQUE(..) as its own set. Column names resolve to indices via
/// `column_index` so an unknown constraint column surfaces as an error rather
/// than a silently-dropped constraint.
fn unique_column_sets(table_def: &TableDef) -> Result<Vec<Vec<usize>>> {
    let mut sets: Vec<Vec<usize>> = Vec::new();
    for (i, col) in table_def.columns.iter().enumerate() {
        if !col.dropped && (col.constraints.unique || col.constraints.primary_key) {
            sets.push(vec![i]);
        }
    }
    if !table_def.constraints.primary_key.is_empty() {
        sets.push(names_to_indices(
            table_def,
            &table_def.constraints.primary_key,
        )?);
    }
    for cols in &table_def.constraints.unique {
        sets.push(names_to_indices(table_def, cols)?);
    }
    Ok(sets)
}

fn names_to_indices(table_def: &TableDef, names: &[String]) -> Result<Vec<usize>> {
    names.iter().map(|n| column_index(table_def, n)).collect()
}

/// Enforce every UNIQUE/PRIMARY KEY set by scanning the heap under `snapshot`.
/// A set with any NULL component in the new row is skipped (SQL treats NULLs
/// as distinct, so such a row never conflicts). `exclude` is the row being
/// updated in place, whose still-visible old version must not count as a
/// conflict with itself.
#[allow(clippy::too_many_arguments)]
fn enforce_unique(
    table_def: &TableDef,
    new_row: &[Literal],
    heap: &Heap,
    snapshot: &Snapshot,
    xid: Xid,
    pool: &mut BufferPool,
    exclude: Option<RowId>,
) -> Result<()> {
    let sets = unique_column_sets(table_def)?;
    // Only sets with no NULL component in the new row are active.
    let active: Vec<&Vec<usize>> = sets
        .iter()
        .filter(|set| set.iter().all(|&i| !matches!(new_row[i], Literal::Null)))
        .collect();
    if active.is_empty() {
        return Ok(());
    }

    for (row_id, bytes) in heap.scan(snapshot, xid, pool)? {
        if Some(row_id) == exclude {
            continue;
        }
        let existing = decode_row(&bytes, &table_def.columns)?;
        for set in &active {
            // `existing[i] == new_row[i]` is false whenever `existing[i]` is
            // NULL (different `Literal` variant), so existing NULLs never
            // conflict — no special-casing needed.
            if set.iter().all(|&i| existing[i] == new_row[i]) {
                let columns = set
                    .iter()
                    .map(|&i| table_def.columns[i].name.clone())
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(DbError::UniqueViolation {
                    table: table_def.name.clone(),
                    columns,
                });
            }
        }
    }
    Ok(())
}

pub(crate) fn project_row(
    projection: &[String],
    columns: &[ColumnDef],
    row: &[Literal],
) -> Result<Vec<Literal>> {
    // `SELECT *` returns every *visible* column — dropped columns (P2.c) keep
    // their physical slot but never surface to the client.
    if projection.is_empty() {
        return Ok(row
            .iter()
            .zip(columns)
            .filter(|(_, c)| !c.dropped)
            .map(|(v, _)| v.clone())
            .collect());
    }
    projection
        .iter()
        .map(|name| {
            let idx = columns
                .iter()
                .position(|c| &c.name == name && !c.dropped)
                .ok_or_else(|| DbError::ColumnNotFound {
                    table: String::new(),
                    column: name.clone(),
                })?;
            Ok(row[idx].clone())
        })
        .collect()
}

fn set_column(
    columns: &[ColumnDef],
    row: &mut [Literal],
    name: &str,
    value: Literal,
) -> Result<()> {
    let idx =
        columns
            .iter()
            .position(|c| c.name == name)
            .ok_or_else(|| DbError::ColumnNotFound {
                table: String::new(),
                column: name.to_string(),
            })?;
    row[idx] = value;
    Ok(())
}

// ── expression evaluation ────────────────────────────────────────────────────

pub(crate) fn predicate_matches(
    predicate: &Option<Expr>,
    columns: &[ColumnDef],
    row: &[Literal],
) -> Result<bool> {
    match predicate {
        None => Ok(true),
        Some(e) => as_bool(&eval_expr(e, columns, row)?),
    }
}

pub(crate) fn eval_expr(expr: &Expr, columns: &[ColumnDef], row: &[Literal]) -> Result<Literal> {
    match expr {
        Expr::Column(name) => {
            let idx = columns
                .iter()
                .position(|c| &c.name == name && !c.dropped)
                .ok_or_else(|| DbError::ColumnNotFound {
                    table: String::new(),
                    column: name.clone(),
                })?;
            Ok(row[idx].clone())
        }
        Expr::Literal(lit) => Ok(lit.clone()),
        Expr::BinOp { op, lhs, rhs } => {
            let l = eval_expr(lhs, columns, row)?;
            let r = eval_expr(rhs, columns, row)?;
            Ok(Literal::Bool(compare(*op, &l, &r)?))
        }
        Expr::And(lhs, rhs) => {
            let l = as_bool(&eval_expr(lhs, columns, row)?)?;
            let r = as_bool(&eval_expr(rhs, columns, row)?)?;
            Ok(Literal::Bool(l && r))
        }
        Expr::JsonExtract { expr, path } => {
            let json = json_of(eval_expr(expr, columns, row)?)?;
            let extracted = json.get(path).cloned().unwrap_or(JsonValue::Null);
            Ok(Literal::Json(extracted.to_string()))
        }
        Expr::JsonExtractText { expr, path } => {
            let json = json_of(eval_expr(expr, columns, row)?)?;
            match json.get(path) {
                Some(JsonValue::String(s)) => Ok(Literal::Text(s.clone())),
                Some(other) => Ok(Literal::Text(other.to_string())),
                None => Ok(Literal::Null),
            }
        }
        // `Expr::Near` only ever reaches `eval_expr` as part of re-checking
        // a candidate `exec_select_near` already fetched *from* the vector
        // index — the row was already filtered by proximity there. Treating
        // it as trivially satisfied here (rather than erroring or
        // re-evaluating distance against the whole table) is what lets the
        // *other* AND'd conditions (RLS, ordinary WHERE terms) still apply
        // through the exact same `predicate_matches` path a normal scan
        // uses — see `exec_select_near`.
        Expr::Near { .. } => Ok(Literal::Bool(true)),
    }
}

fn json_of(lit: Literal) -> Result<JsonValue> {
    let text = match lit {
        Literal::Json(s) | Literal::Text(s) => s,
        other => {
            return Err(DbError::SqlUnsupported(format!(
                "->/->> requires a JSON or text value, got {other:?}"
            )))
        }
    };
    serde_json::from_str(&text).map_err(|e| DbError::SqlPlan(format!("invalid JSON: {e}")))
}

pub(crate) fn as_bool(lit: &Literal) -> Result<bool> {
    match lit {
        Literal::Bool(b) => Ok(*b),
        Literal::Null => Ok(false),
        other => Err(DbError::SqlUnsupported(format!(
            "expected a boolean expression, got {other:?}"
        ))),
    }
}

/// Total-ish ordering between two literals for the Phase-4 sort / merge-join /
/// aggregate paths, built on the same type rules as [`compare`]. Returns `None`
/// for genuinely unorderable pairs (NULL operand, NaN float, or a type mismatch
/// `compare` itself rejects) — callers decide how to place those (NULLs sort
/// last, unmatched merge-join keys are skipped).
pub(crate) fn literal_ord(l: &Literal, r: &Literal) -> Option<std::cmp::Ordering> {
    use std::cmp::Ordering;
    if matches!(l, Literal::Null) || matches!(r, Literal::Null) {
        return None;
    }
    // Reuse `compare`: a == b, then a < b disambiguates Less/Greater. Any pair
    // `compare` errors on (incomparable types) yields `None`.
    match (compare(CmpOp::Eq, l, r), compare(CmpOp::Lt, l, r)) {
        (Ok(true), _) => Some(Ordering::Equal),
        (Ok(false), Ok(true)) => Some(Ordering::Less),
        (Ok(false), Ok(false)) => Some(Ordering::Greater),
        _ => None,
    }
}

pub(crate) fn compare(op: CmpOp, l: &Literal, r: &Literal) -> Result<bool> {
    if matches!(l, Literal::Null) || matches!(r, Literal::Null) {
        // Simplified NULL semantics: any comparison involving NULL is not
        // true. Real three-valued SQL logic (NULL propagation through
        // AND/OR/NOT) is out of scope for M1's subset.
        return Ok(false);
    }
    match (l, r) {
        (Literal::Int(a), Literal::Int(b)) => Ok(apply_cmp(op, a.cmp(b))),
        (Literal::Text(a), Literal::Text(b)) => Ok(apply_cmp(op, a.cmp(b))),
        (Literal::Bool(a), Literal::Bool(b)) => match op {
            CmpOp::Eq => Ok(a == b),
            CmpOp::Ne => Ok(a != b),
            _ => Err(DbError::SqlUnsupported(
                "ordering comparisons are not supported on booleans".into(),
            )),
        },
        // Exact decimal ordering (P2.a). Integers compare as scale-0 decimals,
        // so `WHERE price > 10` works against a `DECIMAL` column.
        (Literal::Decimal(a, sa), Literal::Decimal(b, sb)) => {
            Ok(apply_cmp(op, decimal_cmp(*a, *sa, *b, *sb)?))
        }
        (Literal::Decimal(a, sa), Literal::Int(b)) => {
            Ok(apply_cmp(op, decimal_cmp(*a, *sa, *b as i128, 0)?))
        }
        (Literal::Int(a), Literal::Decimal(b, sb)) => {
            Ok(apply_cmp(op, decimal_cmp(*a as i128, 0, *b, *sb)?))
        }
        // Timestamp ordering (P2.a). A string operand (`ts > '2024-01-01'`) is
        // parsed on demand so predicates read naturally in SQL.
        (Literal::Timestamp(a), Literal::Timestamp(b)) => Ok(apply_cmp(op, a.cmp(b))),
        (Literal::Timestamp(a), Literal::Text(b)) => {
            Ok(apply_cmp(op, a.cmp(&datetime::parse_timestamp(b)?)))
        }
        (Literal::Text(a), Literal::Timestamp(b)) => {
            Ok(apply_cmp(op, datetime::parse_timestamp(a)?.cmp(b)))
        }
        // Float ordering (P2.b), mixing with Int/Decimal via f64. A NaN
        // operand makes every comparison false (IEEE-754 unordered), matching
        // the NULL-operand convention above.
        (Literal::Float(_), _) | (_, Literal::Float(_)) => match (float_of(l), float_of(r)) {
            (Some(a), Some(b)) => Ok(match a.partial_cmp(&b) {
                Some(ord) => apply_cmp(op, ord),
                None => false,
            }),
            _ => Err(DbError::SqlUnsupported(format!(
                "cannot compare {l:?} with {r:?}"
            ))),
        },
        // UUID / DATE / TIME: equality + ordering by their natural key; a Text
        // operand is parsed on demand so `WHERE d > '2024-01-01'` reads well.
        (Literal::Uuid(a), Literal::Uuid(b)) => Ok(apply_cmp(op, a.cmp(b))),
        (Literal::Uuid(a), Literal::Text(b)) => Ok(apply_cmp(op, a.cmp(&parse_uuid(b)?))),
        (Literal::Text(a), Literal::Uuid(b)) => Ok(apply_cmp(op, parse_uuid(a)?.cmp(b))),
        (Literal::Bytea(a), Literal::Bytea(b)) => Ok(apply_cmp(op, a.cmp(b))),
        (Literal::Bytea(a), Literal::Text(b)) => Ok(apply_cmp(op, a.cmp(&parse_bytea(b)))),
        (Literal::Text(a), Literal::Bytea(b)) => Ok(apply_cmp(op, parse_bytea(a).cmp(b))),
        (Literal::Date(a), Literal::Date(b)) => Ok(apply_cmp(op, a.cmp(b))),
        (Literal::Date(a), Literal::Text(b)) => Ok(apply_cmp(op, a.cmp(&datetime::parse_date(b)?))),
        (Literal::Text(a), Literal::Date(b)) => Ok(apply_cmp(op, datetime::parse_date(a)?.cmp(b))),
        (Literal::Time(a), Literal::Time(b)) => Ok(apply_cmp(op, a.cmp(b))),
        (Literal::Time(a), Literal::Text(b)) => Ok(apply_cmp(op, a.cmp(&datetime::parse_time(b)?))),
        (Literal::Text(a), Literal::Time(b)) => Ok(apply_cmp(op, datetime::parse_time(a)?.cmp(b))),
        (a, b) => Err(DbError::SqlUnsupported(format!(
            "cannot compare {a:?} with {b:?}"
        ))),
    }
}

/// Coerce a numeric literal to `f64` for float comparisons (`Int`/`Decimal`/
/// `Float`), or `None` for non-numeric operands.
fn float_of(lit: &Literal) -> Option<f64> {
    match lit {
        Literal::Float(f) => Some(*f),
        Literal::Int(n) => Some(*n as f64),
        Literal::Decimal(v, s) => Some(*v as f64 / 10f64.powi(*s as i32)),
        _ => None,
    }
}

/// Compare two exact decimals of possibly different scales by aligning both to
/// the larger scale via cross-multiplication. Returns an error rather than a
/// wrong answer if either side overflows `i128` at the common scale.
fn decimal_cmp(a: i128, sa: u8, b: i128, sb: u8) -> Result<std::cmp::Ordering> {
    let overflow = || DbError::SqlUnsupported("decimal comparison overflowed i128".into());
    let (la, lb) = if sa == sb {
        (a, b)
    } else if sa < sb {
        (
            a.checked_mul(pow10(sb - sa).ok_or_else(overflow)?)
                .ok_or_else(overflow)?,
            b,
        )
    } else {
        (
            a,
            b.checked_mul(pow10(sa - sb).ok_or_else(overflow)?)
                .ok_or_else(overflow)?,
        )
    };
    Ok(la.cmp(&lb))
}

fn apply_cmp(op: CmpOp, ord: std::cmp::Ordering) -> bool {
    use std::cmp::Ordering::*;
    matches!(
        (op, ord),
        (CmpOp::Eq, Equal)
            | (CmpOp::Ne, Less | Greater)
            | (CmpOp::Lt, Less)
            | (CmpOp::Gt, Greater)
            | (CmpOp::Le, Less | Equal)
            | (CmpOp::Ge, Greater | Equal)
    )
}

// ── row encoding: [tag:1][value...] per column, in table-column order ──────
// Tags: 0=Null, 1=Int64 (8 bytes LE), 2=Text (4-byte LE len + UTF8),
// 3=Bool (1 byte), 4=Json (4-byte LE len + UTF8 text), 5=Vector
// (4-byte LE dimension + dimension * 4-byte LE f32), 6=Decimal (16-byte LE
// i128 unscaled value + 1-byte scale), 7=Timestamp (8-byte LE i64 micros),
// 8=Float (8-byte LE f64), 9=Uuid (16 raw bytes), 10=Bytea (4-byte LE len +
// bytes), 11=Date (4-byte LE i32 days), 12=Time (8-byte LE i64 micros).
//
// New tags are purely additive (D4, forward-compatible): a row written before
// P2.a never carries tag 6/7, so old rows still decode unchanged, and there is
// no `FORMAT_VERSION` bump — the tag set only *grows*. An older binary reading
// a tag-6/7 row fails safe with a "unknown tag" `DbError`, never a silent
// misread. (The `FORMAT_VERSION` gate is reserved for changes that make old
// files genuinely unreadable; a bump here would needlessly reject pre-P2.a
// databases and collide with the parallel Core lane's own version work.)

pub fn encode_row(values: &[Literal]) -> Vec<u8> {
    let mut buf = Vec::new();
    for v in values {
        match v {
            Literal::Null => buf.push(0),
            Literal::Int(n) => {
                buf.push(1);
                buf.extend_from_slice(&n.to_le_bytes());
            }
            Literal::Text(s) => {
                buf.push(2);
                buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
                buf.extend_from_slice(s.as_bytes());
            }
            Literal::Bool(b) => {
                buf.push(3);
                buf.push(*b as u8);
            }
            Literal::Json(s) => {
                buf.push(4);
                buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
                buf.extend_from_slice(s.as_bytes());
            }
            Literal::Vector(v) => {
                buf.push(5);
                buf.extend_from_slice(&(v.len() as u32).to_le_bytes());
                for f in v {
                    buf.extend_from_slice(&f.to_le_bytes());
                }
            }
            Literal::Decimal(value, scale) => {
                buf.push(6);
                buf.extend_from_slice(&value.to_le_bytes());
                buf.push(*scale);
            }
            Literal::Timestamp(micros) => {
                buf.push(7);
                buf.extend_from_slice(&micros.to_le_bytes());
            }
            Literal::Float(f) => {
                buf.push(8);
                buf.extend_from_slice(&f.to_le_bytes());
            }
            Literal::Uuid(bytes) => {
                buf.push(9);
                buf.extend_from_slice(bytes);
            }
            Literal::Bytea(b) => {
                buf.push(10);
                buf.extend_from_slice(&(b.len() as u32).to_le_bytes());
                buf.extend_from_slice(b);
            }
            Literal::Date(days) => {
                buf.push(11);
                buf.extend_from_slice(&days.to_le_bytes());
            }
            Literal::Time(micros) => {
                buf.push(12);
                buf.extend_from_slice(&micros.to_le_bytes());
            }
            // Unreachable: `bind_params` (P2.e) replaces every placeholder with
            // a concrete value, and `coerce_value` would reject any leftover
            // `Param` long before encoding. Encoded as NULL as a benign
            // no-panic fallback for the theoretically-impossible case.
            Literal::Param(_) => {
                debug_assert!(false, "unbound bind parameter reached encode_row");
                buf.push(0);
            }
        }
    }
    buf
}

pub fn decode_row(bytes: &[u8], columns: &[ColumnDef]) -> Result<Vec<Literal>> {
    let mut out = Vec::with_capacity(columns.len());
    let mut pos = 0usize;
    for col in columns {
        // A row written before an `ALTER TABLE ADD COLUMN` (P2.c) has no bytes
        // for the trailing new column(s). Fill each such column with its
        // coerced DEFAULT (so `ADD COLUMN ... DEFAULT x` shows `x` for old
        // rows) or NULL — no heap rewrite needed.
        if pos >= bytes.len() {
            let lit = match &col.constraints.default {
                Some(default) => coerce_value("", col, default.clone()).unwrap_or(Literal::Null),
                None => Literal::Null,
            };
            out.push(lit);
            continue;
        }
        let tag = *bytes
            .get(pos)
            .ok_or_else(|| DbError::SqlPlan("row decode error: truncated tag".into()))?;
        pos += 1;
        let lit = match tag {
            0 => Literal::Null,
            1 => {
                let end = pos + 8;
                let raw: [u8; 8] = bytes
                    .get(pos..end)
                    .ok_or_else(|| DbError::SqlPlan("row decode error: truncated int".into()))?
                    .try_into()
                    .unwrap();
                pos = end;
                Literal::Int(i64::from_le_bytes(raw))
            }
            2 | 4 => {
                let len_end = pos + 4;
                let len_raw: [u8; 4] = bytes
                    .get(pos..len_end)
                    .ok_or_else(|| DbError::SqlPlan("row decode error: truncated length".into()))?
                    .try_into()
                    .unwrap();
                let len = u32::from_le_bytes(len_raw) as usize;
                pos = len_end;
                let str_end = pos + len;
                let raw = bytes
                    .get(pos..str_end)
                    .ok_or_else(|| DbError::SqlPlan("row decode error: truncated string".into()))?;
                let s = String::from_utf8(raw.to_vec()).map_err(|e| {
                    DbError::SqlPlan(format!("row decode error: invalid utf8: {e}"))
                })?;
                pos = str_end;
                if tag == 2 {
                    Literal::Text(s)
                } else {
                    Literal::Json(s)
                }
            }
            3 => {
                let b = *bytes
                    .get(pos)
                    .ok_or_else(|| DbError::SqlPlan("row decode error: truncated bool".into()))?;
                pos += 1;
                Literal::Bool(b != 0)
            }
            5 => {
                let dim_end = pos + 4;
                let dim_raw: [u8; 4] = bytes
                    .get(pos..dim_end)
                    .ok_or_else(|| {
                        DbError::SqlPlan("row decode error: truncated vector dim".into())
                    })?
                    .try_into()
                    .unwrap();
                let dim = u32::from_le_bytes(dim_raw) as usize;
                pos = dim_end;
                if let ColumnType::Vector(n) = col.ty {
                    if dim != n as usize {
                        return Err(DbError::SqlPlan(format!(
                            "row decode error: column '{}' declares dimension {n}, but stored data has dimension {dim}",
                            col.name
                        )));
                    }
                }
                let mut values = Vec::with_capacity(dim);
                for _ in 0..dim {
                    let f_end = pos + 4;
                    let f_raw: [u8; 4] = bytes
                        .get(pos..f_end)
                        .ok_or_else(|| {
                            DbError::SqlPlan("row decode error: truncated vector element".into())
                        })?
                        .try_into()
                        .unwrap();
                    values.push(f32::from_le_bytes(f_raw));
                    pos = f_end;
                }
                Literal::Vector(values)
            }
            6 => {
                let val_end = pos + 16;
                let raw: [u8; 16] = bytes
                    .get(pos..val_end)
                    .ok_or_else(|| DbError::SqlPlan("row decode error: truncated decimal".into()))?
                    .try_into()
                    .unwrap();
                pos = val_end;
                let scale = *bytes.get(pos).ok_or_else(|| {
                    DbError::SqlPlan("row decode error: truncated decimal scale".into())
                })?;
                pos += 1;
                if let ColumnType::Decimal(_, col_scale) = col.ty {
                    if scale != col_scale {
                        return Err(DbError::SqlPlan(format!(
                            "row decode error: column '{}' declares scale {col_scale}, but stored data has scale {scale}",
                            col.name
                        )));
                    }
                }
                Literal::Decimal(i128::from_le_bytes(raw), scale)
            }
            7 => {
                let end = pos + 8;
                let raw: [u8; 8] = bytes
                    .get(pos..end)
                    .ok_or_else(|| {
                        DbError::SqlPlan("row decode error: truncated timestamp".into())
                    })?
                    .try_into()
                    .unwrap();
                pos = end;
                Literal::Timestamp(i64::from_le_bytes(raw))
            }
            8 => {
                let end = pos + 8;
                let raw: [u8; 8] = bytes
                    .get(pos..end)
                    .ok_or_else(|| DbError::SqlPlan("row decode error: truncated float".into()))?
                    .try_into()
                    .unwrap();
                pos = end;
                Literal::Float(f64::from_le_bytes(raw))
            }
            9 => {
                let end = pos + 16;
                let raw: [u8; 16] = bytes
                    .get(pos..end)
                    .ok_or_else(|| DbError::SqlPlan("row decode error: truncated uuid".into()))?
                    .try_into()
                    .unwrap();
                pos = end;
                Literal::Uuid(raw)
            }
            10 => {
                let len_end = pos + 4;
                let len_raw: [u8; 4] = bytes
                    .get(pos..len_end)
                    .ok_or_else(|| {
                        DbError::SqlPlan("row decode error: truncated bytea length".into())
                    })?
                    .try_into()
                    .unwrap();
                let len = u32::from_le_bytes(len_raw) as usize;
                pos = len_end;
                let end = pos + len;
                let raw = bytes
                    .get(pos..end)
                    .ok_or_else(|| DbError::SqlPlan("row decode error: truncated bytea".into()))?;
                pos = end;
                Literal::Bytea(raw.to_vec())
            }
            11 => {
                let end = pos + 4;
                let raw: [u8; 4] = bytes
                    .get(pos..end)
                    .ok_or_else(|| DbError::SqlPlan("row decode error: truncated date".into()))?
                    .try_into()
                    .unwrap();
                pos = end;
                Literal::Date(i32::from_le_bytes(raw))
            }
            12 => {
                let end = pos + 8;
                let raw: [u8; 8] = bytes
                    .get(pos..end)
                    .ok_or_else(|| DbError::SqlPlan("row decode error: truncated time".into()))?
                    .try_into()
                    .unwrap();
                pos = end;
                Literal::Time(i64::from_le_bytes(raw))
            }
            other => {
                return Err(DbError::SqlPlan(format!(
                    "row decode error: unknown tag {other}"
                )))
            }
        };
        out.push(lit);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{DEFAULT_PAGE_SIZE, INVALID_LSN};
    use crate::sql::parser::parse_sql;
    use crate::txn::IsolationLevel;
    use tempfile::tempdir;

    struct Harness {
        pool: BufferPool,
        wal: Wal,
        lock_mgr: LockManager,
        txn_mgr: TransactionManager,
        catalog: Catalog,
        control: ControlData,
        control_path: std::path::PathBuf,
        page_size: usize,
        next_event_seq: u64,
    }

    impl Harness {
        fn new(dir: &std::path::Path) -> Self {
            let control_path = dir.join("control");
            let control = crate::control::create(&control_path, DEFAULT_PAGE_SIZE).unwrap();
            let pool =
                BufferPool::open(&dir.join("data.db"), DEFAULT_PAGE_SIZE as usize, 64).unwrap();
            let wal = Wal::open(&dir.join("db.wal"), INVALID_LSN).unwrap();
            Self {
                pool,
                wal,
                lock_mgr: LockManager::new(),
                txn_mgr: TransactionManager::new(),
                catalog: Catalog::new(),
                control,
                control_path,
                page_size: DEFAULT_PAGE_SIZE as usize,
                next_event_seq: 1,
            }
        }

        fn exec_as(&mut self, xid: Xid, sql: &str) -> Result<ExecResult> {
            let mut plans = parse_sql(sql)?;
            assert_eq!(plans.len(), 1, "expected exactly one statement");
            let plan = plans.remove(0);
            let mut ctx = ExecCtx {
                catalog: &mut self.catalog,
                txn_mgr: &mut self.txn_mgr,
                pool: &mut self.pool,
                wal: &mut self.wal,
                lock_mgr: &mut self.lock_mgr,
                control_path: &self.control_path,
                control: &mut self.control,
                page_size: self.page_size,
                xid,
                next_event_seq: &mut self.next_event_seq,
            };
            execute(plan, &mut ctx)
        }

        fn begin(&mut self) -> Xid {
            self.txn_mgr
                .begin(IsolationLevel::ReadCommitted, &mut self.wal)
                .unwrap()
        }

        fn commit(&mut self, xid: Xid) {
            self.txn_mgr
                .commit(xid, &mut self.wal, &mut self.lock_mgr)
                .unwrap();
        }
    }

    #[test]
    fn create_insert_select_round_trip() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());

        let xid = h.begin();
        h.exec_as(
            xid,
            "CREATE TABLE accounts (id INT, name TEXT, active BOOLEAN)",
        )
        .unwrap();
        h.exec_as(
            xid,
            "INSERT INTO accounts (id, name, active) VALUES (1, 'alice', true)",
        )
        .unwrap();
        h.exec_as(
            xid,
            "INSERT INTO accounts (id, name, active) VALUES (2, 'bob', false)",
        )
        .unwrap();
        h.commit(xid);

        let xid2 = h.begin();
        let result = h
            .exec_as(xid2, "SELECT * FROM accounts WHERE id = 1")
            .unwrap();
        match result {
            ExecResult::Rows(rows) => {
                assert_eq!(rows.len(), 1);
                assert_eq!(
                    rows[0],
                    vec![
                        Literal::Int(1),
                        Literal::Text("alice".to_string()),
                        Literal::Bool(true)
                    ]
                );
            }
            other => panic!("expected Rows, got {other:?}"),
        }
        h.commit(xid2);
    }

    #[test]
    fn select_with_projection() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE t (id INT, name TEXT)")
            .unwrap();
        h.exec_as(xid, "INSERT INTO t (id, name) VALUES (1, 'a')")
            .unwrap();
        h.commit(xid);

        let xid2 = h.begin();
        let result = h.exec_as(xid2, "SELECT name FROM t").unwrap();
        match result {
            ExecResult::Rows(rows) => {
                assert_eq!(rows, vec![vec![Literal::Text("a".to_string())]]);
            }
            other => panic!("expected Rows, got {other:?}"),
        }
    }

    #[test]
    fn update_then_reselect() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE accounts (id INT, balance INT)")
            .unwrap();
        h.exec_as(xid, "INSERT INTO accounts (id, balance) VALUES (1, 100)")
            .unwrap();
        h.commit(xid);

        let xid2 = h.begin();
        let updated = h
            .exec_as(xid2, "UPDATE accounts SET balance = 50 WHERE id = 1")
            .unwrap();
        assert_eq!(updated, ExecResult::Updated { count: 1 });
        h.commit(xid2);

        let xid3 = h.begin();
        let result = h.exec_as(xid3, "SELECT balance FROM accounts").unwrap();
        assert_eq!(result, ExecResult::Rows(vec![vec![Literal::Int(50)]]));
    }

    #[test]
    fn delete_then_reselect_finds_nothing() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE t (id INT)").unwrap();
        h.exec_as(xid, "INSERT INTO t (id) VALUES (1)").unwrap();
        h.commit(xid);

        let xid2 = h.begin();
        let deleted = h.exec_as(xid2, "DELETE FROM t WHERE id = 1").unwrap();
        assert_eq!(deleted, ExecResult::Deleted { count: 1 });
        h.commit(xid2);

        let xid3 = h.begin();
        let result = h.exec_as(xid3, "SELECT * FROM t").unwrap();
        assert_eq!(result, ExecResult::Rows(vec![]));
    }

    #[test]
    fn json_column_round_trip_and_extract() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE t (id INT, data JSON)")
            .unwrap();
        h.exec_as(
            xid,
            r#"INSERT INTO t (id, data) VALUES (1, '{"status": "active"}')"#,
        )
        .unwrap();
        h.commit(xid);

        let xid2 = h.begin();
        let result = h
            .exec_as(xid2, "SELECT * FROM t WHERE (data ->> 'status') = 'active'")
            .unwrap();
        match result {
            ExecResult::Rows(rows) => assert_eq!(rows.len(), 1),
            other => panic!("expected Rows, got {other:?}"),
        }

        let none = h
            .exec_as(
                xid2,
                "SELECT * FROM t WHERE (data ->> 'status') = 'inactive'",
            )
            .unwrap();
        assert_eq!(none, ExecResult::Rows(vec![]));
    }

    #[test]
    fn insert_invalid_json_is_rejected() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE t (data JSON)").unwrap();
        let err = h.exec_as(xid, "INSERT INTO t (data) VALUES ('not json')");
        assert!(matches!(err, Err(DbError::SqlPlan(_))));
    }

    #[test]
    fn table_survives_reopen_via_catalog_pages() {
        let dir = tempdir().unwrap();
        let (root_page, rid_data);
        {
            let mut h = Harness::new(dir.path());
            let xid = h.begin();
            h.exec_as(xid, "CREATE TABLE t (id INT)").unwrap();
            h.exec_as(xid, "INSERT INTO t (id) VALUES (42)").unwrap();
            h.commit(xid);
            h.pool.flush_all(h.wal.durable_lsn).unwrap();
            root_page = h.control.catalog_root;
            rid_data = h.catalog.lookup("t").unwrap().pages.clone();
        }
        assert_ne!(root_page, crate::format::INVALID_PAGE_ID);
        assert!(
            !rid_data.is_empty(),
            "table must have recorded its page list"
        );

        // Reopen: reconstruct catalog + pool from what was persisted.
        let mut pool =
            BufferPool::open(&dir.path().join("data.db"), DEFAULT_PAGE_SIZE as usize, 64).unwrap();
        let control = crate::control::read(&dir.path().join("control")).unwrap();
        let catalog = Catalog::load(&control, &mut pool).unwrap();
        let table_def = catalog.lookup("t").unwrap();
        let heap = Heap::from_pages(DEFAULT_PAGE_SIZE as usize, table_def.pages.clone());
        let snap = crate::mvcc::Snapshot::new(1000, 1000, vec![]);
        let rows = heap.scan(&snap, 1000, &pool).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            decode_row(&rows[0].1, &table_def.columns).unwrap(),
            vec![Literal::Int(42)]
        );
    }

    #[test]
    fn row_encode_decode_round_trip() {
        let columns = vec![
            ColumnDef {
                name: "a".to_string(),
                index: None,
                index_root: None,
                dropped: false,
                constraints: Default::default(),
                ty: ColumnType::Int64,
            },
            ColumnDef {
                name: "b".to_string(),
                index: None,
                index_root: None,
                dropped: false,
                constraints: Default::default(),
                ty: ColumnType::Text,
            },
            ColumnDef {
                name: "c".to_string(),
                index: None,
                index_root: None,
                dropped: false,
                constraints: Default::default(),
                ty: ColumnType::Bool,
            },
            ColumnDef {
                name: "d".to_string(),
                index: None,
                index_root: None,
                dropped: false,
                constraints: Default::default(),
                ty: ColumnType::Json,
            },
        ];
        let values = vec![
            Literal::Int(-7),
            Literal::Text("hello".to_string()),
            Literal::Bool(true),
            Literal::Json("{\"x\":1}".to_string()),
        ];
        let encoded = encode_row(&values);
        let decoded = decode_row(&encoded, &columns).unwrap();
        assert_eq!(decoded, values);
    }

    #[test]
    fn row_encode_decode_handles_null() {
        let columns = vec![ColumnDef {
            name: "a".to_string(),
            index: None,
            index_root: None,
            dropped: false,
            constraints: Default::default(),
            ty: ColumnType::Int64,
        }];
        let encoded = encode_row(&[Literal::Null]);
        let decoded = decode_row(&encoded, &columns).unwrap();
        assert_eq!(decoded, vec![Literal::Null]);
    }

    #[test]
    fn row_encode_decode_vector_round_trip() {
        let columns = vec![ColumnDef {
            name: "embedding".to_string(),
            index: None,
            index_root: None,
            dropped: false,
            constraints: Default::default(),
            ty: ColumnType::Vector(4),
        }];
        let values = vec![Literal::Vector(vec![0.1, -0.2, 0.3, 0.4])];
        let encoded = encode_row(&values);
        let decoded = decode_row(&encoded, &columns).unwrap();
        assert_eq!(decoded, values);
    }

    #[test]
    fn row_decode_rejects_vector_dimension_mismatch() {
        let columns = vec![ColumnDef {
            name: "embedding".to_string(),
            index: None,
            index_root: None,
            dropped: false,
            constraints: Default::default(),
            ty: ColumnType::Vector(4),
        }];
        // Encode a 3-dimension vector but declare the column as 4.
        let encoded = encode_row(&[Literal::Vector(vec![0.1, 0.2, 0.3])]);
        let err = decode_row(&encoded, &columns);
        assert!(matches!(err, Err(DbError::SqlPlan(_))));
    }

    #[test]
    fn coerce_vector_rejects_dimension_mismatch() {
        let table = TableDef {
            name: "t".to_string(),
            columns: vec![ColumnDef {
                name: "embedding".to_string(),
                index: None,
                index_root: None,
                dropped: false,
                constraints: Default::default(),
                ty: ColumnType::Vector(4),
            }],
            pages: vec![],
            rls_policy: None,
            events_enabled: false,
            serial_next: Default::default(),
            constraints: Default::default(),
        };
        let err = coerce_and_validate_row(&table, vec![Literal::Vector(vec![0.1, 0.2, 0.3])]);
        assert!(matches!(err, Err(DbError::SqlPlan(_))));
    }

    // ── P2.a: DECIMAL + TIMESTAMP ────────────────────────────────────────────

    fn col(name: &str, ty: ColumnType) -> ColumnDef {
        ColumnDef {
            name: name.to_string(),
            index: None,
            index_root: None,
            dropped: false,
            constraints: Default::default(),
            ty,
        }
    }

    #[test]
    fn row_encode_decode_decimal_and_timestamp_round_trip() {
        let columns = vec![
            col("price", ColumnType::Decimal(10, 2)),
            col("created", ColumnType::Timestamp),
        ];
        let values = vec![
            Literal::Decimal(-12345, 2),
            Literal::Timestamp(1_700_000_000_000_000),
        ];
        let encoded = encode_row(&values);
        let decoded = decode_row(&encoded, &columns).unwrap();
        assert_eq!(decoded, values);
    }

    #[test]
    fn row_decode_rejects_decimal_scale_mismatch() {
        let columns = vec![col("price", ColumnType::Decimal(10, 2))];
        // Stored scale 3 but column declares scale 2.
        let encoded = encode_row(&[Literal::Decimal(1234, 3)]);
        let err = decode_row(&encoded, &columns);
        assert!(matches!(err, Err(DbError::SqlPlan(_))));
    }

    #[test]
    fn decimal_column_round_trips_exactly() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE t (id INT, price DECIMAL(10, 2))")
            .unwrap();
        // Literal 9.5 widens to scale 2 => 9.50; integer 100 => 100.00.
        h.exec_as(xid, "INSERT INTO t (id, price) VALUES (1, 9.5)")
            .unwrap();
        h.exec_as(xid, "INSERT INTO t (id, price) VALUES (2, 100)")
            .unwrap();
        h.commit(xid);

        let xid2 = h.begin();
        let rows = match h.exec_as(xid2, "SELECT price FROM t WHERE id = 1").unwrap() {
            ExecResult::Rows(r) => r,
            other => panic!("expected Rows, got {other:?}"),
        };
        assert_eq!(rows, vec![vec![Literal::Decimal(950, 2)]]);
        let rows2 = match h.exec_as(xid2, "SELECT price FROM t WHERE id = 2").unwrap() {
            ExecResult::Rows(r) => r,
            other => panic!("expected Rows, got {other:?}"),
        };
        assert_eq!(rows2, vec![vec![Literal::Decimal(10000, 2)]]);
    }

    #[test]
    fn decimal_rejects_excess_fractional_digits() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE t (price DECIMAL(10, 2))")
            .unwrap();
        // 9.999 has scale 3; column allows scale 2 and the extra digit is
        // nonzero -> exact rescale impossible, reject rather than round.
        let err = h.exec_as(xid, "INSERT INTO t (price) VALUES (9.999)");
        assert!(matches!(err, Err(DbError::SqlPlan(_))));
        // 9.990 narrows exactly to 9.99 -> accepted.
        h.exec_as(xid, "INSERT INTO t (price) VALUES (9.990)")
            .unwrap();
    }

    #[test]
    fn decimal_rejects_precision_overflow() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE t (price DECIMAL(4, 2))")
            .unwrap();
        // 999.99 needs 5 significant digits; DECIMAL(4,2) caps at 4.
        let err = h.exec_as(xid, "INSERT INTO t (price) VALUES (999.99)");
        assert!(matches!(err, Err(DbError::SqlPlan(_))));
    }

    #[test]
    fn decimal_range_and_equality_predicates() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE t (id INT, price DECIMAL(10, 2))")
            .unwrap();
        for (id, p) in [(1, "5.00"), (2, "9.99"), (3, "10.00"), (4, "10.50")] {
            h.exec_as(
                xid,
                &format!("INSERT INTO t (id, price) VALUES ({id}, {p})"),
            )
            .unwrap();
        }
        h.commit(xid);

        let xid2 = h.begin();
        // Range predicate against a decimal literal of a *different* scale.
        let rows = match h
            .exec_as(xid2, "SELECT id FROM t WHERE price > 9.9")
            .unwrap()
        {
            ExecResult::Rows(r) => r,
            other => panic!("expected Rows, got {other:?}"),
        };
        assert_eq!(rows.len(), 3); // 9.99, 10.00, 10.50
                                   // Equality against an integer literal (scale 0 vs stored scale 2).
        let eq = match h
            .exec_as(xid2, "SELECT id FROM t WHERE price = 10")
            .unwrap()
        {
            ExecResult::Rows(r) => r,
            other => panic!("expected Rows, got {other:?}"),
        };
        assert_eq!(eq, vec![vec![Literal::Int(3)]]);
    }

    #[test]
    fn decimal_default_check_and_unique_constraints() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        h.exec_as(
            xid,
            "CREATE TABLE t (id INT, price DECIMAL(10, 2) DEFAULT 1.00 CHECK (price > 0) UNIQUE)",
        )
        .unwrap();
        // DEFAULT fills 1.00 when omitted.
        h.exec_as(xid, "INSERT INTO t (id) VALUES (1)").unwrap();
        // CHECK rejects a non-positive price.
        let bad = h.exec_as(xid, "INSERT INTO t (id, price) VALUES (2, -3.00)");
        assert!(matches!(bad, Err(DbError::CheckViolation { .. })));
        // UNIQUE rejects a duplicate decimal (1.00 already present via default).
        let dup = h.exec_as(xid, "INSERT INTO t (id, price) VALUES (3, 1.00)");
        assert!(matches!(dup, Err(DbError::UniqueViolation { .. })));
        // A distinct decimal is accepted.
        h.exec_as(xid, "INSERT INTO t (id, price) VALUES (4, 2.50)")
            .unwrap();
    }

    #[test]
    fn timestamp_column_round_trips_and_orders() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE t (id INT, created TIMESTAMP)")
            .unwrap();
        h.exec_as(
            xid,
            "INSERT INTO t (id, created) VALUES (1, '2023-12-31 23:59:59')",
        )
        .unwrap();
        h.exec_as(
            xid,
            "INSERT INTO t (id, created) VALUES (2, '2024-06-01 12:00:00')",
        )
        .unwrap();
        h.commit(xid);

        let xid2 = h.begin();
        let created = crate::sql::datetime::parse_timestamp("2023-12-31 23:59:59").unwrap();
        let rows = match h
            .exec_as(xid2, "SELECT created FROM t WHERE id = 1")
            .unwrap()
        {
            ExecResult::Rows(r) => r,
            other => panic!("expected Rows, got {other:?}"),
        };
        assert_eq!(rows, vec![vec![Literal::Timestamp(created)]]);

        // Range predicate with a string literal on the right-hand side.
        let after = match h
            .exec_as(
                xid2,
                "SELECT id FROM t WHERE created > '2024-01-01 00:00:00'",
            )
            .unwrap()
        {
            ExecResult::Rows(r) => r,
            other => panic!("expected Rows, got {other:?}"),
        };
        assert_eq!(after, vec![vec![Literal::Int(2)]]);
    }

    #[test]
    fn timestamp_as_primary_key_enforces_uniqueness() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE t (created TIMESTAMP PRIMARY KEY)")
            .unwrap();
        h.exec_as(
            xid,
            "INSERT INTO t (created) VALUES ('2024-01-01 00:00:00')",
        )
        .unwrap();
        // Same instant expressed with a 'T' separator — must still collide.
        let dup = h.exec_as(
            xid,
            "INSERT INTO t (created) VALUES ('2024-01-01T00:00:00')",
        );
        assert!(matches!(dup, Err(DbError::UniqueViolation { .. })));
    }

    #[test]
    fn invalid_timestamp_literal_is_rejected() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE t (created TIMESTAMP)")
            .unwrap();
        let err = h.exec_as(
            xid,
            "INSERT INTO t (created) VALUES ('2024-13-40 99:99:99')",
        );
        assert!(matches!(err, Err(DbError::SqlPlan(_))));
    }

    #[test]
    fn decimal_and_timestamp_survive_reopen() {
        let dir = tempdir().unwrap();
        {
            let mut h = Harness::new(dir.path());
            let xid = h.begin();
            h.exec_as(
                xid,
                "CREATE TABLE t (price DECIMAL(10, 2), created TIMESTAMP)",
            )
            .unwrap();
            h.exec_as(
                xid,
                "INSERT INTO t (price, created) VALUES (19.95, '2024-03-14 09:26:53')",
            )
            .unwrap();
            h.commit(xid);
            h.pool.flush_all(h.wal.durable_lsn).unwrap();
        }

        let mut pool =
            BufferPool::open(&dir.path().join("data.db"), DEFAULT_PAGE_SIZE as usize, 64).unwrap();
        let control = crate::control::read(&dir.path().join("control")).unwrap();
        let catalog = Catalog::load(&control, &mut pool).unwrap();
        let table_def = catalog.lookup("t").unwrap();
        assert_eq!(table_def.columns[0].ty, ColumnType::Decimal(10, 2));
        assert_eq!(table_def.columns[1].ty, ColumnType::Timestamp);
        let heap = Heap::from_pages(DEFAULT_PAGE_SIZE as usize, table_def.pages.clone());
        let snap = crate::mvcc::Snapshot::new(1000, 1000, vec![]);
        let rows = heap.scan(&snap, 1000, &pool).unwrap();
        let decoded = decode_row(&rows[0].1, &table_def.columns).unwrap();
        assert_eq!(
            decoded,
            vec![
                Literal::Decimal(1995, 2),
                Literal::Timestamp(
                    crate::sql::datetime::parse_timestamp("2024-03-14 09:26:53").unwrap()
                ),
            ]
        );
    }

    // ── P2.b: FLOAT / UUID / BYTEA / DATE / TIME ─────────────────────────────

    #[test]
    fn row_encode_decode_p2b_round_trip() {
        let columns = vec![
            col("f", ColumnType::Float),
            col("u", ColumnType::Uuid),
            col("b", ColumnType::Bytea),
            col("d", ColumnType::Date),
            col("t", ColumnType::Time),
        ];
        let values = vec![
            Literal::Float(-3.5),
            Literal::Uuid([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]),
            Literal::Bytea(vec![0xde, 0xad, 0xbe, 0xef]),
            Literal::Date(19_000),
            Literal::Time(45_296_000_000),
        ];
        let encoded = encode_row(&values);
        assert_eq!(decode_row(&encoded, &columns).unwrap(), values);
    }

    #[test]
    fn float_column_round_trips_and_orders() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE t (id INT, x FLOAT)").unwrap();
        h.exec_as(xid, "INSERT INTO t (id, x) VALUES (1, 1.5)")
            .unwrap();
        h.exec_as(xid, "INSERT INTO t (id, x) VALUES (2, 10)")
            .unwrap();
        h.commit(xid);
        let xid2 = h.begin();
        let rows = match h.exec_as(xid2, "SELECT x FROM t WHERE id = 1").unwrap() {
            ExecResult::Rows(r) => r,
            o => panic!("{o:?}"),
        };
        assert_eq!(rows, vec![vec![Literal::Float(1.5)]]);
        let gt = match h.exec_as(xid2, "SELECT id FROM t WHERE x > 2").unwrap() {
            ExecResult::Rows(r) => r,
            o => panic!("{o:?}"),
        };
        assert_eq!(gt, vec![vec![Literal::Int(2)]]);
    }

    #[test]
    fn uuid_round_trip_equality_and_pk() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE t (id UUID PRIMARY KEY)")
            .unwrap();
        h.exec_as(
            xid,
            "INSERT INTO t (id) VALUES ('550e8400-e29b-41d4-a716-446655440000')",
        )
        .unwrap();
        // Same UUID without hyphens must collide under PRIMARY KEY.
        let dup = h.exec_as(
            xid,
            "INSERT INTO t (id) VALUES ('550e8400e29b41d4a716446655440000')",
        );
        assert!(matches!(dup, Err(DbError::UniqueViolation { .. })));
        // Round-trip: canonical lowercase hyphenated form comes back.
        let rows = match h
            .exec_as(
                xid,
                "SELECT id FROM t WHERE id = '550E8400-E29B-41D4-A716-446655440000'",
            )
            .unwrap()
        {
            ExecResult::Rows(r) => r,
            o => panic!("{o:?}"),
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0][0],
            Literal::Uuid(parse_uuid("550e8400-e29b-41d4-a716-446655440000").unwrap())
        );
        let bad = h.exec_as(xid, "INSERT INTO t (id) VALUES ('not-a-uuid')");
        assert!(matches!(bad, Err(DbError::SqlPlan(_))));
    }

    #[test]
    fn bytea_round_trip_hex_and_raw() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE t (id INT, b BYTEA)").unwrap();
        h.exec_as(xid, r"INSERT INTO t (id, b) VALUES (1, '\xDEADBEEF')")
            .unwrap();
        h.exec_as(xid, "INSERT INTO t (id, b) VALUES (2, 'hi')")
            .unwrap();
        h.commit(xid);
        let xid2 = h.begin();
        let r1 = match h.exec_as(xid2, "SELECT b FROM t WHERE id = 1").unwrap() {
            ExecResult::Rows(r) => r,
            o => panic!("{o:?}"),
        };
        assert_eq!(r1, vec![vec![Literal::Bytea(vec![0xde, 0xad, 0xbe, 0xef])]]);
        let r2 = match h.exec_as(xid2, "SELECT b FROM t WHERE id = 2").unwrap() {
            ExecResult::Rows(r) => r,
            o => panic!("{o:?}"),
        };
        assert_eq!(r2, vec![vec![Literal::Bytea(b"hi".to_vec())]]);
    }

    #[test]
    fn date_and_time_round_trip_and_order() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE t (id INT, d DATE, tm TIME)")
            .unwrap();
        h.exec_as(
            xid,
            "INSERT INTO t (id, d, tm) VALUES (1, '2024-01-15', '09:30:00')",
        )
        .unwrap();
        h.exec_as(
            xid,
            "INSERT INTO t (id, d, tm) VALUES (2, '2024-06-01', '18:00:00')",
        )
        .unwrap();
        h.commit(xid);
        let xid2 = h.begin();
        let rows = match h.exec_as(xid2, "SELECT d, tm FROM t WHERE id = 1").unwrap() {
            ExecResult::Rows(r) => r,
            o => panic!("{o:?}"),
        };
        assert_eq!(
            rows,
            vec![vec![
                Literal::Date(crate::sql::datetime::parse_date("2024-01-15").unwrap()),
                Literal::Time(crate::sql::datetime::parse_time("09:30:00").unwrap()),
            ]]
        );
        let after = match h
            .exec_as(
                xid2,
                "SELECT id FROM t WHERE d > '2024-03-01' AND tm > '12:00:00'",
            )
            .unwrap()
        {
            ExecResult::Rows(r) => r,
            o => panic!("{o:?}"),
        };
        assert_eq!(after, vec![vec![Literal::Int(2)]]);
    }

    // ── P2.c: ALTER / DROP / TRUNCATE ────────────────────────────────────────

    #[test]
    fn alter_add_column_shows_default_for_old_rows() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE t (id INT)").unwrap();
        h.exec_as(xid, "INSERT INTO t (id) VALUES (1)").unwrap();
        // Add a column with a DEFAULT: the pre-existing row must show it.
        let r = h
            .exec_as(xid, "ALTER TABLE t ADD COLUMN status TEXT DEFAULT 'new'")
            .unwrap();
        assert_eq!(r, ExecResult::AlteredTable);
        h.exec_as(xid, "INSERT INTO t (id, status) VALUES (2, 'live')")
            .unwrap();
        h.commit(xid);

        let xid2 = h.begin();
        let old = match h
            .exec_as(xid2, "SELECT status FROM t WHERE id = 1")
            .unwrap()
        {
            ExecResult::Rows(r) => r,
            o => panic!("{o:?}"),
        };
        assert_eq!(old, vec![vec![Literal::Text("new".to_string())]]);
        let new = match h
            .exec_as(xid2, "SELECT status FROM t WHERE id = 2")
            .unwrap()
        {
            ExecResult::Rows(r) => r,
            o => panic!("{o:?}"),
        };
        assert_eq!(new, vec![vec![Literal::Text("live".to_string())]]);
    }

    #[test]
    fn alter_add_not_null_column_without_default_rejected() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE t (id INT)").unwrap();
        let err = h.exec_as(xid, "ALTER TABLE t ADD COLUMN x INT NOT NULL");
        assert!(matches!(err, Err(DbError::SqlPlan(_))));
    }

    #[test]
    fn drop_middle_column_preserves_old_row_alignment() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE t (a INT, b INT, c INT)")
            .unwrap();
        h.exec_as(xid, "INSERT INTO t (a, b, c) VALUES (1, 2, 3)")
            .unwrap();
        h.exec_as(xid, "ALTER TABLE t DROP COLUMN b").unwrap();
        h.commit(xid);

        let xid2 = h.begin();
        // The tombstone hazard: the pre-drop row's `c` must still decode as 3,
        // not be misread from `b`'s old bytes.
        let rows = match h.exec_as(xid2, "SELECT a, c FROM t").unwrap() {
            ExecResult::Rows(r) => r,
            o => panic!("{o:?}"),
        };
        assert_eq!(rows, vec![vec![Literal::Int(1), Literal::Int(3)]]);
        // SELECT * returns only the two visible columns.
        let star = match h.exec_as(xid2, "SELECT * FROM t").unwrap() {
            ExecResult::Rows(r) => r,
            o => panic!("{o:?}"),
        };
        assert_eq!(star, vec![vec![Literal::Int(1), Literal::Int(3)]]);
        // The dropped column is no longer referenceable.
        assert!(matches!(
            h.exec_as(xid2, "SELECT b FROM t"),
            Err(DbError::ColumnNotFound { .. })
        ));
        // A new insert over the visible columns still aligns.
        let xid3 = h.begin();
        h.exec_as(xid3, "INSERT INTO t (a, c) VALUES (4, 6)")
            .unwrap();
        h.commit(xid3);
        let xid4 = h.begin();
        let after = match h.exec_as(xid4, "SELECT a, c FROM t WHERE a = 4").unwrap() {
            ExecResult::Rows(r) => r,
            o => panic!("{o:?}"),
        };
        assert_eq!(after, vec![vec![Literal::Int(4), Literal::Int(6)]]);
    }

    #[test]
    fn drop_column_if_exists_is_noop() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE t (a INT, b INT)").unwrap();
        assert_eq!(
            h.exec_as(xid, "ALTER TABLE t DROP COLUMN IF EXISTS nope")
                .unwrap(),
            ExecResult::AlteredTable
        );
        assert!(matches!(
            h.exec_as(xid, "ALTER TABLE t DROP COLUMN nope"),
            Err(DbError::ColumnNotFound { .. })
        ));
    }

    #[test]
    fn drop_table_then_recreate() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE t (id INT)").unwrap();
        h.exec_as(xid, "INSERT INTO t (id) VALUES (1)").unwrap();
        assert_eq!(
            h.exec_as(xid, "DROP TABLE t").unwrap(),
            ExecResult::DroppedTable
        );
        assert!(matches!(
            h.exec_as(xid, "SELECT * FROM t"),
            Err(DbError::TableNotFound(_))
        ));
        // Re-create with a fresh schema; the old rows are gone.
        h.exec_as(xid, "CREATE TABLE t (name TEXT)").unwrap();
        h.commit(xid);
        let xid2 = h.begin();
        assert_eq!(
            h.exec_as(xid2, "SELECT * FROM t").unwrap(),
            ExecResult::Rows(vec![])
        );
    }

    #[test]
    fn truncate_removes_all_rows_keeps_schema() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE t (id INT)").unwrap();
        h.exec_as(xid, "INSERT INTO t (id) VALUES (1)").unwrap();
        h.exec_as(xid, "INSERT INTO t (id) VALUES (2)").unwrap();
        h.commit(xid);
        let xid2 = h.begin();
        assert_eq!(
            h.exec_as(xid2, "TRUNCATE TABLE t").unwrap(),
            ExecResult::Truncated { count: 2 }
        );
        h.commit(xid2);
        let xid3 = h.begin();
        assert_eq!(
            h.exec_as(xid3, "SELECT * FROM t").unwrap(),
            ExecResult::Rows(vec![])
        );
        // Schema intact: can still insert.
        h.exec_as(xid3, "INSERT INTO t (id) VALUES (9)").unwrap();
    }

    #[test]
    fn ddl_on_system_table_rejected() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        assert!(matches!(
            h.exec_as(xid, "DROP TABLE __events__"),
            Err(DbError::SqlPlan(_))
        ));
    }

    // ── P2.d: SERIAL / sequences ─────────────────────────────────────────────

    #[test]
    fn serial_auto_increments_monotonically() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE t (id SERIAL, name TEXT)")
            .unwrap();
        h.exec_as(xid, "INSERT INTO t (name) VALUES ('a')").unwrap();
        h.exec_as(xid, "INSERT INTO t (name) VALUES ('b')").unwrap();
        h.exec_as(xid, "INSERT INTO t (name) VALUES ('c')").unwrap();
        h.commit(xid);
        let xid2 = h.begin();
        let rows = match h.exec_as(xid2, "SELECT id, name FROM t").unwrap() {
            ExecResult::Rows(r) => r,
            o => panic!("{o:?}"),
        };
        let mut ids: Vec<i64> = rows
            .iter()
            .map(|r| match r[0] {
                Literal::Int(n) => n,
                _ => panic!("id not int"),
            })
            .collect();
        ids.sort_unstable();
        assert_eq!(ids, vec![1, 2, 3]);
    }

    #[test]
    fn serial_respects_explicit_value_and_is_the_pk() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE t (id SERIAL PRIMARY KEY, name TEXT)")
            .unwrap();
        h.exec_as(xid, "INSERT INTO t (name) VALUES ('a')").unwrap(); // id=1
                                                                      // Explicit value is honored as-is.
        h.exec_as(xid, "INSERT INTO t (id, name) VALUES (100, 'b')")
            .unwrap();
        h.exec_as(xid, "INSERT INTO t (name) VALUES ('c')").unwrap(); // id=2
        let xid_dup = xid;
        // id=1 already used -> PRIMARY KEY conflict on an explicit dup.
        let dup = h.exec_as(xid_dup, "INSERT INTO t (id, name) VALUES (1, 'x')");
        assert!(matches!(dup, Err(DbError::UniqueViolation { .. })));
    }

    #[test]
    fn generated_as_identity_auto_fills() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        h.exec_as(
            xid,
            "CREATE TABLE t (id INT GENERATED ALWAYS AS IDENTITY, name TEXT)",
        )
        .unwrap();
        h.exec_as(xid, "INSERT INTO t (name) VALUES ('a')").unwrap();
        let rows = match h.exec_as(xid, "SELECT id FROM t").unwrap() {
            ExecResult::Rows(r) => r,
            o => panic!("{o:?}"),
        };
        assert_eq!(rows, vec![vec![Literal::Int(1)]]);
    }

    #[test]
    fn serial_on_non_integer_rejected() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        // GENERATED AS IDENTITY on a TEXT column must be rejected.
        let err = h.exec_as(xid, "CREATE TABLE t (id TEXT GENERATED ALWAYS AS IDENTITY)");
        assert!(matches!(err, Err(DbError::SqlPlan(_))));
    }

    // ── M2.c: CREATE INDEX ───────────────────────────────────────────────────

    #[test]
    fn create_index_persists_hnsw_on_vector_column() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE t (id INT, embedding VECTOR(2))")
            .unwrap();
        let result = h
            .exec_as(xid, "CREATE INDEX idx ON t USING HNSW (embedding)")
            .unwrap();
        assert_eq!(result, ExecResult::CreatedIndex);
        assert_eq!(
            h.catalog.lookup("t").unwrap().columns[1].index,
            Some(IndexKind::Hnsw)
        );
    }

    #[test]
    fn create_index_persists_fulltext_on_text_column() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE t (id INT, body TEXT)")
            .unwrap();
        let result = h
            .exec_as(xid, "CREATE INDEX idx ON t USING FULLTEXT (body)")
            .unwrap();
        assert_eq!(result, ExecResult::CreatedIndex);
        assert_eq!(
            h.catalog.lookup("t").unwrap().columns[1].index,
            Some(IndexKind::FullText)
        );
    }

    #[test]
    fn create_index_rejects_hnsw_on_text_column() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE t (id INT, body TEXT)")
            .unwrap();
        let err = h
            .exec_as(xid, "CREATE INDEX idx ON t USING HNSW (body)")
            .unwrap_err();
        assert!(matches!(err, DbError::SqlPlan(_)));
    }

    #[test]
    fn create_index_rejects_fulltext_on_vector_column() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE t (id INT, embedding VECTOR(2))")
            .unwrap();
        let err = h
            .exec_as(xid, "CREATE INDEX idx ON t USING FULLTEXT (embedding)")
            .unwrap_err();
        assert!(matches!(err, DbError::SqlPlan(_)));
    }

    #[test]
    fn create_index_accepts_btree_on_int64_column() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE t (id INT)").unwrap();
        let result = h
            .exec_as(xid, "CREATE INDEX idx ON t USING BTREE (id)")
            .unwrap();
        assert_eq!(result, ExecResult::CreatedIndex);
        assert_eq!(
            h.catalog.lookup("t").unwrap().columns[0].index,
            Some(IndexKind::BTree)
        );
    }

    #[test]
    fn create_index_accepts_btree_on_text_column() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE t (id INT, name TEXT)")
            .unwrap();
        let result = h
            .exec_as(xid, "CREATE INDEX idx ON t USING BTREE (name)")
            .unwrap();
        assert_eq!(result, ExecResult::CreatedIndex);
        assert_eq!(
            h.catalog.lookup("t").unwrap().columns[1].index,
            Some(IndexKind::BTree)
        );
    }

    #[test]
    fn create_index_accepts_btree_on_bool_column() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE t (id INT, active BOOL)")
            .unwrap();
        let result = h
            .exec_as(xid, "CREATE INDEX idx ON t USING BTREE (active)")
            .unwrap();
        assert_eq!(result, ExecResult::CreatedIndex);
        assert_eq!(
            h.catalog.lookup("t").unwrap().columns[1].index,
            Some(IndexKind::BTree)
        );
    }

    #[test]
    fn create_index_rejects_btree_on_vector_column() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE t (id INT, embedding VECTOR(2))")
            .unwrap();
        let err = h
            .exec_as(xid, "CREATE INDEX idx ON t USING BTREE (embedding)")
            .unwrap_err();
        assert!(matches!(err, DbError::SqlPlan(_)));
    }

    #[test]
    fn create_index_rejects_btree_on_json_column() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE t (id INT, data JSON)")
            .unwrap();
        let err = h
            .exec_as(xid, "CREATE INDEX idx ON t USING BTREE (data)")
            .unwrap_err();
        assert!(matches!(err, DbError::SqlPlan(_)));
    }

    #[test]
    fn create_index_rejects_unknown_column() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE t (id INT)").unwrap();
        let err = h
            .exec_as(xid, "CREATE INDEX idx ON t USING HNSW (nope)")
            .unwrap_err();
        assert!(matches!(err, DbError::ColumnNotFound { .. }));
    }

    // ── M2.d: NEAR ───────────────────────────────────────────────────────────

    #[test]
    fn near_on_unindexed_column_is_rejected() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE t (id INT, embedding VECTOR(2))")
            .unwrap();
        let err = h
            .exec_as(xid, "SELECT * FROM t WHERE NEAR(embedding, [0.0, 0.0], 3)")
            .unwrap_err();
        assert!(matches!(err, DbError::SqlPlan(_)));
    }

    #[test]
    fn near_on_durable_index_returns_nearest_no_worker() {
        // P3.c: NEAR is served by the durable on-disk IVF-Flat index — no async
        // worker anywhere. The Harness has none, yet NEAR works end-to-end.
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE t (id INT, embedding VECTOR(2))")
            .unwrap();
        h.exec_as(xid, "CREATE INDEX idx ON t USING HNSW (embedding)")
            .unwrap();
        h.exec_as(xid, "INSERT INTO t (id, embedding) VALUES (1, [0.0, 0.0])")
            .unwrap();
        h.exec_as(xid, "INSERT INTO t (id, embedding) VALUES (2, [9.0, 9.0])")
            .unwrap();
        h.exec_as(xid, "INSERT INTO t (id, embedding) VALUES (3, [0.2, 0.2])")
            .unwrap();
        let res = h
            .exec_as(xid, "SELECT id FROM t WHERE NEAR(embedding, [0.0, 0.0], 2)")
            .unwrap();
        match res {
            ExecResult::Rows(rows) => {
                assert_eq!(rows.len(), 2);
                assert_eq!(rows[0][0], Literal::Int(1));
                assert_eq!(rows[1][0], Literal::Int(3));
            }
            other => panic!("expected Rows, got {other:?}"),
        }
    }

    #[test]
    fn near_on_wrong_column_type_is_rejected() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE t (id INT, name TEXT)")
            .unwrap();
        let err = h
            .exec_as(xid, "SELECT * FROM t WHERE NEAR(name, [0.0, 0.0], 3)")
            .unwrap_err();
        assert!(matches!(err, DbError::SqlPlan(_)));
    }
}
