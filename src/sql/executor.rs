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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

use serde_json::Value as JsonValue;

use crate::{
    btree_index::{DiskBTree, OrderedValue, RangeOp},
    bufferpool::{BufferPool, PageReader},
    catalog::{Catalog, CatalogCtx, ColumnDef, ColumnType, IndexKind, TableConstraints, TableDef},
    control::ControlData,
    disk_vector::DiskIvfIndex,
    error::{DbError, Result},
    format::{PageId, Xid},
    heap::{get_visible, Heap, RowId},
    lockmgr::{LockManager, RecordId},
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

/// Compute the `RecordId` for a `UniqueKey` phantom lock: a stable hash of
/// `(table_name, col_name, key_value)`. Two distinct `OrderedValue` variants
/// are distinguished by a tag byte so `Int(1)` ≠ `Bool(true)`.
///
/// Used by `exec_insert` to acquire the lock BEFORE taking the uniqueness
/// snapshot, serializing concurrent inserters racing the same PK/UNIQUE value.
fn unique_key_record_id(table: &str, col: &str, key: &OrderedValue) -> RecordId {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    table.hash(&mut h);
    col.hash(&mut h);
    match key {
        OrderedValue::Int(n) => {
            1u8.hash(&mut h);
            n.hash(&mut h);
        }
        OrderedValue::Text(s) => {
            2u8.hash(&mut h);
            s.hash(&mut h);
        }
        OrderedValue::Bool(b) => {
            3u8.hash(&mut h);
            b.hash(&mut h);
        }
    }
    RecordId::unique_key(h.finish())
}

/// Compute the `RecordId` for an `FkKey` phantom lock: a stable hash of
/// `(parent_table, ref_col, key_value)`. Acquired Exclusive by both the child
/// inserter (before its snapshot) and the parent deleter (before its RESTRICT
/// scan), held through commit — closes the parent-delete / child-insert race.
fn fk_key_record_id(parent_table: &str, ref_col: &str, key: &OrderedValue) -> RecordId {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    parent_table.hash(&mut h);
    ref_col.hash(&mut h);
    match key {
        OrderedValue::Int(n) => {
            1u8.hash(&mut h);
            n.hash(&mut h);
        }
        OrderedValue::Text(s) => {
            2u8.hash(&mut h);
            s.hash(&mut h);
        }
        OrderedValue::Bool(b) => {
            3u8.hash(&mut h);
            b.hash(&mut h);
        }
    }
    RecordId::fk_key(h.finish())
}

/// Resolve the parent column that a FK reference targets — either the
/// explicitly named `ref_col`, or the single PK column inferred from
/// `parent_def` when no column name was stored (SQL allows `REFERENCES t`
/// with no column list, defaulting to `t`'s PK).
fn resolve_fk_ref_col<'a>(
    parent_def: &'a TableDef,
    ref_col: Option<&str>,
) -> Result<&'a ColumnDef> {
    if let Some(name) = ref_col {
        let idx = column_index(parent_def, name)?;
        return Ok(&parent_def.columns[idx]);
    }
    // No explicit column: infer from parent's single-column PK.
    let col_pks: Vec<_> = parent_def
        .columns
        .iter()
        .filter(|c| !c.dropped && c.constraints.primary_key)
        .collect();
    if col_pks.len() == 1 {
        return Ok(col_pks[0]);
    }
    if parent_def.constraints.primary_key.len() == 1 {
        let idx = column_index(parent_def, &parent_def.constraints.primary_key[0])?;
        return Ok(&parent_def.columns[idx]);
    }
    Err(DbError::SqlPlan(format!(
        "FOREIGN KEY to '{}' specifies no column and the table has no single-column PK",
        parent_def.name
    )))
}

/// Format a `Literal` as a human-readable value string for error messages.
fn literal_display(lit: &Literal) -> String {
    match lit {
        Literal::Int(n) => n.to_string(),
        Literal::Text(s) => format!("'{s}'"),
        Literal::Bool(b) => b.to_string(),
        Literal::Float(f) => f.to_string(),
        Literal::Json(s) => s.clone(),
        Literal::Decimal(v, s) => format!(
            "{}.{}",
            v / 10i128.pow(*s as u32),
            v.abs() % 10i128.pow(*s as u32)
        ),
        Literal::Timestamp(us) => format!("{us}µs"),
        Literal::Uuid(b) => format!("{:032x}", u128::from_be_bytes(*b)),
        Literal::Bytea(b) => format!("<{} bytes>", b.len()),
        Literal::Date(d) => format!("date({d})"),
        Literal::Time(t) => format!("time({t})"),
        Literal::Vector(_) => "<vector>".to_string(),
        Literal::Param(n) => format!("${n}"),
        Literal::Null => "NULL".to_string(),
    }
}

/// Returns true if any table in `catalog` carries a column-level or
/// table-level FK that references `parent_table`. Used to gate the O(n)
/// catalog scan in `exec_delete` / `exec_update` so tables that are never
/// referenced pay zero overhead.
fn table_has_fk_children(catalog: &Catalog, parent_table: &str) -> bool {
    catalog.tables().any(|t| {
        t.columns.iter().any(|c| {
            c.constraints
                .references
                .as_ref()
                .is_some_and(|r| r.table == parent_table)
        }) || t
            .constraints
            .foreign_keys
            .iter()
            .any(|fk| fk.ref_table == parent_table)
    })
}

/// Acquire exclusive `FkKey` phantom locks for every non-NULL FK column value
/// in `row`. Must be called BEFORE `snapshot_for_statement` so the lock is held
/// when the snapshot is taken, preventing the parent-delete / child-insert race.
fn acquire_fk_key_locks(
    table_def: &TableDef,
    row: &[Literal],
    xid: Xid,
    lock_mgr: &LockManager,
    catalog: &Catalog,
) -> Result<()> {
    for (col_idx, col) in table_def.columns.iter().enumerate() {
        if col.dropped {
            continue;
        }
        let Some(fk) = &col.constraints.references else {
            continue;
        };
        if matches!(row[col_idx], Literal::Null) {
            continue;
        }
        if let Ok(key) = OrderedValue::try_from(&row[col_idx]) {
            // Resolve the actual ref-column name (handles `REFERENCES t` with no column).
            let ref_col_name = if let Some(name) = fk.column.as_deref() {
                name.to_string()
            } else if let Ok(parent_def) = catalog.lookup(&fk.table) {
                resolve_fk_ref_col(parent_def, None)
                    .map(|c| c.name.clone())
                    .unwrap_or_default()
            } else {
                String::new()
            };
            lock_mgr.acquire_blocking(fk_key_record_id(&fk.table, &ref_col_name, &key), xid)?;
        }
    }
    for fk in &table_def.constraints.foreign_keys {
        if fk.columns.len() != 1 {
            continue; // composite FK: no phantom lock (heap scan fallback)
        }
        let col_idx = match column_index(table_def, &fk.columns[0]) {
            Ok(i) => i,
            Err(_) => continue,
        };
        if matches!(row[col_idx], Literal::Null) {
            continue;
        }
        if let Ok(key) = OrderedValue::try_from(&row[col_idx]) {
            let ref_col = fk.ref_columns.first().map(String::as_str).unwrap_or("");
            lock_mgr.acquire_blocking(fk_key_record_id(&fk.ref_table, ref_col, &key), xid)?;
        }
    }
    Ok(())
}

/// Acquire exclusive `FkKey` phantom locks for the parent table's PK column
/// values in `row`. Called BEFORE the RESTRICT scan in `exec_delete` /
/// `exec_update`, so a concurrent child inserter either sees the lock and
/// blocks (getting FK violation after the parent commits) or committed first
/// and will be caught by the RESTRICT scan with a fresh snapshot.
fn acquire_fk_key_locks_parent(
    parent_def: &TableDef,
    row: &[Literal],
    xid: Xid,
    lock_mgr: &LockManager,
) -> Result<()> {
    // Column-level PRIMARY KEY columns.
    for (col_idx, col) in parent_def.columns.iter().enumerate() {
        if col.dropped || !col.constraints.primary_key {
            continue;
        }
        if let Ok(key) = OrderedValue::try_from(&row[col_idx]) {
            lock_mgr.acquire_blocking(fk_key_record_id(&parent_def.name, &col.name, &key), xid)?;
        }
    }
    // Table-level PRIMARY KEY columns (if not already covered above).
    for pk_name in &parent_def.constraints.primary_key {
        let col_idx = match column_index(parent_def, pk_name) {
            Ok(i) => i,
            Err(_) => continue,
        };
        if parent_def.columns[col_idx].constraints.primary_key {
            continue; // already locked above
        }
        if let Ok(key) = OrderedValue::try_from(&row[col_idx]) {
            lock_mgr.acquire_blocking(fk_key_record_id(&parent_def.name, pk_name, &key), xid)?;
        }
    }
    Ok(())
}

/// Verify that every non-NULL FK value in `row` has a visible parent row.
/// Called AFTER acquiring the `FkKey` phantom locks and taking the statement
/// snapshot, so it sees committed parent rows including those inserted earlier
/// in the same transaction (own-xid visibility via `get_visible`'s `self_xid`).
fn enforce_fk_rows_exist(
    table_def: &TableDef,
    row: &[Literal],
    snapshot: &Snapshot,
    xid: Xid,
    pool: &BufferPool,
    catalog: &Catalog,
) -> Result<()> {
    // Column-level FKs.
    for (col_idx, col) in table_def.columns.iter().enumerate() {
        if col.dropped {
            continue;
        }
        let Some(fk) = &col.constraints.references else {
            continue;
        };
        let fk_val = &row[col_idx];
        if matches!(fk_val, Literal::Null) {
            continue;
        }
        check_fk_parent_exists(
            &table_def.name,
            &fk.table,
            fk.column.as_deref(),
            &col.name,
            fk_val,
            snapshot,
            xid,
            pool,
            catalog,
        )?;
    }
    // Table-level FKs.
    for fk in &table_def.constraints.foreign_keys {
        if fk.columns.len() == 1 {
            let col_idx = column_index(table_def, &fk.columns[0])?;
            let fk_val = &row[col_idx];
            if matches!(fk_val, Literal::Null) {
                continue;
            }
            let ref_col = fk.ref_columns.first().map(String::as_str);
            check_fk_parent_exists(
                &table_def.name,
                &fk.ref_table,
                ref_col,
                &fk.columns[0],
                fk_val,
                snapshot,
                xid,
                pool,
                catalog,
            )?;
        } else {
            // Composite FK: heap scan of parent (O(n) — no composite PK index yet).
            check_fk_parent_exists_composite(table_def, fk, row, snapshot, xid, pool, catalog)?;
        }
    }
    Ok(())
}

/// Point-lookup variant: single-column FK, uses parent's `unique_index_root`
/// (item 35) for O(log n) check. Falls back to a heap scan if the parent
/// column has no implicit unique index (pre-item-35 tables or non-BTree types).
#[allow(clippy::too_many_arguments)]
fn check_fk_parent_exists(
    child_table: &str,
    ref_table: &str,
    ref_col: Option<&str>,
    child_col_name: &str,
    fk_val: &Literal,
    snapshot: &Snapshot,
    xid: Xid,
    pool: &BufferPool,
    catalog: &Catalog,
) -> Result<()> {
    let parent_def = catalog.lookup(ref_table)?;
    let parent_col = resolve_fk_ref_col(parent_def, ref_col)?;

    // Fast path: parent PK/UNIQUE column has an implicit B-tree (item 35).
    if let Some(uiq_meta) = parent_col.unique_index_root {
        if let Ok(key) = OrderedValue::try_from(fk_val) {
            let candidates = DiskBTree::new(uiq_meta, pool.page_size()).search_eq(&key, pool)?;
            for rid in candidates {
                if get_visible(pool, rid, snapshot, xid)?.is_some() {
                    return Ok(());
                }
            }
            return Err(DbError::ForeignKeyViolation {
                table: child_table.to_string(),
                ref_table: ref_table.to_string(),
                column: Some(child_col_name.to_string()),
                value: Some(literal_display(fk_val)),
            });
        }
    }

    // Fallback: heap scan of parent (no implicit unique index — pre-item-35
    // table, or non-BTree-indexable type). O(n) in parent table size.
    let ref_col_idx = column_index(parent_def, &parent_col.name)?;
    let parent_heap = Heap::open(
        pool.page_size(),
        parent_def.fsm_meta,
        parent_def.pages.clone(),
    );
    for (_, bytes) in parent_heap.scan(snapshot, xid, pool)? {
        let parent_row = decode_row(&bytes, &parent_def.columns)?;
        if parent_row[ref_col_idx] == *fk_val {
            return Ok(());
        }
    }
    Err(DbError::ForeignKeyViolation {
        table: child_table.to_string(),
        ref_table: ref_table.to_string(),
        column: Some(child_col_name.to_string()),
        value: Some(literal_display(fk_val)),
    })
}

/// Composite-FK fallback: heap-scan parent for a row where every referenced
/// column matches the child's FK column values. O(n) in parent table size;
/// documented limitation — use a covering index on the parent's composite PK
/// for large tables.
fn check_fk_parent_exists_composite(
    child_def: &TableDef,
    fk: &crate::catalog::ForeignKey,
    child_row: &[Literal],
    snapshot: &Snapshot,
    xid: Xid,
    pool: &BufferPool,
    catalog: &Catalog,
) -> Result<()> {
    let parent_def = catalog.lookup(&fk.ref_table)?;
    // Map each child FK column → (parent_ref_col_idx, child_col_idx).
    let pairs: Vec<(usize, usize)> = fk
        .columns
        .iter()
        .zip(fk.ref_columns.iter())
        .map(|(child_col, ref_col)| {
            Ok((
                column_index(parent_def, ref_col)?,
                column_index(child_def, child_col)?,
            ))
        })
        .collect::<Result<_>>()?;

    let child_vals: Vec<&Literal> = pairs.iter().map(|(_, ci)| &child_row[*ci]).collect();
    if child_vals.iter().any(|v| matches!(v, Literal::Null)) {
        return Ok(()); // NULL in any part of a composite FK → unchecked
    }

    let parent_heap = Heap::open(
        pool.page_size(),
        parent_def.fsm_meta,
        parent_def.pages.clone(),
    );
    for (_, bytes) in parent_heap.scan(snapshot, xid, pool)? {
        let parent_row = decode_row(&bytes, &parent_def.columns)?;
        if pairs
            .iter()
            .zip(child_vals.iter())
            .all(|((pi, _), cv)| &parent_row[*pi] == *cv)
        {
            return Ok(());
        }
    }
    Err(DbError::ForeignKeyViolation {
        table: child_def.name.clone(),
        ref_table: fk.ref_table.clone(),
        column: Some(fk.columns.join(", ")),
        value: Some(
            child_vals
                .iter()
                .map(|v| literal_display(v))
                .collect::<Vec<_>>()
                .join(", "),
        ),
    })
}

/// RESTRICT: for a parent row about to be deleted/updated, verify that no
/// visible child row references its PK value. Iterates all catalog tables
/// for FK references to `parent_def.name`; uses the child FK column's
/// secondary DiskBTree index if present, otherwise falls back to a heap scan.
fn enforce_fk_restrict(
    parent_def: &TableDef,
    deleted_row: &[Literal],
    snapshot: &Snapshot,
    xid: Xid,
    pool: &BufferPool,
    catalog: &Catalog,
) -> Result<()> {
    for child_def in catalog.tables() {
        // Column-level references.
        for (child_col_idx, child_col) in child_def.columns.iter().enumerate() {
            if child_col.dropped {
                continue;
            }
            let Some(fk) = &child_col.constraints.references else {
                continue;
            };
            if fk.table != parent_def.name {
                continue;
            }
            let parent_col = match resolve_fk_ref_col(parent_def, fk.column.as_deref()) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let parent_col_idx = match column_index(parent_def, &parent_col.name) {
                Ok(i) => i,
                Err(_) => continue,
            };
            let pk_val = &deleted_row[parent_col_idx];
            if matches!(pk_val, Literal::Null) {
                continue;
            }
            check_restrict_child(
                parent_def,
                pk_val,
                child_def,
                child_col_idx,
                snapshot,
                xid,
                pool,
            )?;
        }
        // Table-level FK references (single-column fast path only).
        for fk in &child_def.constraints.foreign_keys {
            if fk.ref_table != parent_def.name {
                continue;
            }
            if fk.columns.len() != 1 || fk.ref_columns.len() != 1 {
                continue; // composite — skip (would need composite heap scan)
            }
            let child_col_idx = match column_index(child_def, &fk.columns[0]) {
                Ok(i) => i,
                Err(_) => continue,
            };
            let parent_col_idx = match column_index(parent_def, &fk.ref_columns[0]) {
                Ok(i) => i,
                Err(_) => continue,
            };
            let pk_val = &deleted_row[parent_col_idx];
            if matches!(pk_val, Literal::Null) {
                continue;
            }
            check_restrict_child(
                parent_def,
                pk_val,
                child_def,
                child_col_idx,
                snapshot,
                xid,
                pool,
            )?;
        }
    }
    Ok(())
}

/// Inner RESTRICT check: does any visible row in `child_def` have `child_col`
/// equal to `pk_val`? Uses the child column's secondary DiskBTree index for
/// O(log n) if one exists; falls back to a full heap scan otherwise (O(n) —
/// documented: add `CREATE INDEX ON child(fk_col)` to avoid).
fn check_restrict_child(
    parent_def: &TableDef,
    pk_val: &Literal,
    child_def: &TableDef,
    child_col_idx: usize,
    snapshot: &Snapshot,
    xid: Xid,
    pool: &BufferPool,
) -> Result<()> {
    let child_col = &child_def.columns[child_col_idx];

    // Fast path: child FK column has an explicit secondary DiskBTree index.
    if let Some(index_root) = child_col.index_root {
        if matches!(child_col.index, Some(IndexKind::BTree)) {
            if let Ok(key) = OrderedValue::try_from(pk_val) {
                let candidates =
                    DiskBTree::new(index_root, pool.page_size()).search_eq(&key, pool)?;
                for rid in candidates {
                    if get_visible(pool, rid, snapshot, xid)?.is_some() {
                        return Err(DbError::ForeignKeyViolation {
                            table: child_def.name.clone(),
                            ref_table: parent_def.name.clone(),
                            column: Some(child_col.name.clone()),
                            value: Some(literal_display(pk_val)),
                        });
                    }
                }
                return Ok(());
            }
        }
    }

    // Fallback: full heap scan of child (no index on FK column).
    let child_heap = Heap::open(
        pool.page_size(),
        child_def.fsm_meta,
        child_def.pages.clone(),
    );
    for (_, bytes) in child_heap.scan(snapshot, xid, pool)? {
        let child_row = decode_row(&bytes, &child_def.columns)?;
        if child_row[child_col_idx] == *pk_val {
            return Err(DbError::ForeignKeyViolation {
                table: child_def.name.clone(),
                ref_table: parent_def.name.clone(),
                column: Some(child_col.name.clone()),
                value: Some(literal_display(pk_val)),
            });
        }
    }
    Ok(())
}

use super::datetime;
use super::logical::{CmpOp, Expr, Literal, LogicalPlan};

/// Measurement-only (Phase A C1): total number of `decode_row` calls since
/// process start. Every full-row decode (a heap scan materializing a row into
/// `Vec<Literal>`) bumps this by one. A benchmark diffs it around an operation
/// to attribute "rows decoded per op" — the metric that exposes the write
/// path's full-scan-the-heap cost (RC1/RC3) and, later, decode pushdown wins
/// (Phase B). `Relaxed` because it is a pure statistic with no ordering
/// obligations; the few-ns cost is negligible next to a per-row decode.
pub static ROWS_DECODED: AtomicU64 = AtomicU64::new(0);

/// Measurement-only (Phase B C1′): total number of column *values* materialized
/// into a `Literal` since process start. A full-row `decode_row` bumps this once
/// per column; the projection-pushdown `deform_row` (B2) bumps it only for the
/// columns actually needed. Diffed around an op, `cols/row` (this ÷ records) is
/// the direct proof of the decode-pushdown win — it falls as unreferenced
/// columns (esp. TEXT) stop being materialized. `Relaxed`, like `ROWS_DECODED`.
pub static COLS_DECODED: AtomicU64 = AtomicU64::new(0);

/// How the executor holds the catalog for one statement (index-write-concurrency
/// Item 0a). Every SQL statement that changes no schema reads the catalog only
/// (it `lookup(table)?.clone()`s the `TableDef` and works off the owned clone),
/// so DML can run under a **shared** catalog lock and overlap concurrent
/// writers. DDL — and the two DML mutations that touch the catalog (a SERIAL
/// bump, or a legacy non-FSM table's page-list persist) — need **exclusive**
/// access. This handle lets one `ExecCtx` type carry either:
///
/// * `Shared(&Catalog)` — the concurrent DML path (`cat_read`). `exclusive()`
///   returns an error, which is a *tripwire*: the routing in `lib.rs` must never
///   send a catalog-mutating statement down this path (it escalates to
///   `cat_write` instead), so hitting it means a routing bug, caught as a clean
///   error rather than silent corruption.
/// * `Exclusive(&mut Catalog)` — DDL, and all DML when the toggle is off
///   (`cat_write`, today's behavior verbatim).
///
/// Read access goes through `Deref`, so every existing `ctx.catalog.lookup(..)`
/// call site is unchanged.
pub enum CatalogHandle<'a> {
    Shared(&'a Catalog),
    Exclusive(&'a mut Catalog),
}

impl std::ops::Deref for CatalogHandle<'_> {
    type Target = Catalog;
    fn deref(&self) -> &Catalog {
        match self {
            CatalogHandle::Shared(c) => c,
            CatalogHandle::Exclusive(c) => c,
        }
    }
}

impl CatalogHandle<'_> {
    /// Borrow the catalog for a *read* as an explicit `&Catalog` — for the few
    /// call sites that pass it to a function expecting `&Catalog` (where `Deref`
    /// coercion on a field place isn't automatic).
    pub fn get(&self) -> &Catalog {
        self
    }

    /// Borrow the catalog for a *mutation*. Succeeds only for `Exclusive`; a
    /// `Shared` handle means a catalog-mutating statement was mis-routed onto the
    /// concurrent (`cat_read`) path — a bug, surfaced as an error, never a
    /// corrupting write under a shared lock.
    pub fn exclusive(&mut self) -> Result<&mut Catalog> {
        match self {
            CatalogHandle::Exclusive(c) => Ok(c),
            CatalogHandle::Shared(_) => Err(DbError::SqlPlan(
                "catalog mutation attempted under a shared (concurrent-DML) lock; \
                 this statement should have been routed to the exclusive path"
                    .into(),
            )),
        }
    }
}

/// Everything the executor needs, bundled to avoid a long parameter list.
pub struct ExecCtx<'a> {
    pub catalog: CatalogHandle<'a>,
    pub txn_mgr: &'a TransactionManager,
    pub pool: &'a BufferPool,
    pub wal: &'a Wal,
    pub lock_mgr: &'a LockManager,
    pub control_path: &'a Path,
    pub control: &'a Mutex<ControlData>,
    pub page_size: usize,
    pub xid: Xid,
    /// Next `seq` to assign in `__events__` (M4). Lives here rather than as
    /// an extra function argument threaded through `execute()` — unlike
    /// M3.c's `edge_index` (needed by exactly one top-level entry point,
    /// `graph_executor::execute`), event capture must reach the deeply
    /// nested private `exec_insert`/`exec_update`/`exec_delete`. Bumped by
    /// `send_event_capture` on every captured event. Atomic (P5.e) so the
    /// shared `&self` engine can hand out `__events__` sequence numbers from
    /// concurrent writer threads without a lock.
    pub next_event_seq: &'a AtomicU64,
    /// Meta page of the durable `__events__.seq` B-tree index (item 26, Q1).
    /// `Some(meta)` on a fully-opened Engine (always the case from lib.rs);
    /// `None` in unit tests that build their own `ExecCtx` without a full
    /// engine (the tests don't exercise event capture, so the index is never
    /// needed there).
    pub event_seq_index_meta: Option<crate::format::PageId>,
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
/// Schema-generation tripwire (index-write-concurrency, Validation §5). A DML
/// statement clones its `TableDef` up front and works off that owned copy; this
/// asserts, at write time, that the catalog's live generation for the table is
/// still the one it cloned. Under the concurrent (`cat_read`) path the whole
/// statement holds a shared catalog lock, so no DDL (which needs `cat_write`)
/// can interleave — the generation MUST be stable. A firing assert means the
/// lock discipline regressed and a statement is about to write against a stale
/// schema; a `debug_assert!` so it costs nothing in release but turns the
/// dangerous window into a hard failure under test / stress / TSan. `gen0` is
/// the fallback so a table that is genuinely absent (never the case mid-DML
/// under a held lock) does not itself trip the assert.
#[inline]
fn assert_schema_stable(ctx: &ExecCtx, table: &str, gen0: u64) {
    debug_assert_eq!(
        ctx.catalog
            .lookup(table)
            .map(|t| t.generation)
            .unwrap_or(gen0),
        gen0,
        "table '{table}' schema generation changed during a running DML statement — \
         catalog lock discipline violated (index-write-concurrency tripwire)"
    );
}

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
        // Explicit secondary index (CREATE INDEX).
        if let Some(meta_page) = col.index_root {
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
        // Implicit unique-enforcement index (item 35): maintain on every INSERT
        // so enforce_unique can do a point lookup instead of a heap scan.
        if let Some(uiq_meta) = col.unique_index_root {
            if let Ok(value) = OrderedValue::try_from(&row[idx]) {
                DiskBTree::new(uiq_meta, ctx.page_size).insert(value, row_id, ctx.pool, ctx.wal)?;
            }
        }
    }
    Ok(())
}

/// A per-column accumulator for **coalesced** index maintenance (A1). Each
/// `DiskBTree`-backed indexed column (BTree or FullText) collects its
/// `(key, RowId)` pairs across every row an UPDATE touches, then flushes once
/// via [`DiskBTree::insert_many`] so each dirtied leaf is WAL-logged **once**
/// instead of once per row. This is the fix for RC2: a `body`-only bulk UPDATE
/// re-inserts each row's *unchanged* `k` as a new version — correctness demands
/// the entry (an index scan resolves to the live RowId; skipping it loses the
/// row), but per-row logging emitted one ~8 KiB `WAL_INDEX` image *per row*.
/// Coalescing keeps every entry and collapses the WAL to one image per leaf.
///
/// Hnsw (vector) is deliberately *not* batched here — it is not the bulk-update
/// hot path and uses different index machinery ([`DiskIvfIndex`]); it stays on
/// the per-row path, matching [`apply_durable_index_writes`].
struct IndexColBatch {
    col_idx: usize,
    meta_page: PageId,
    is_fulltext: bool,
    entries: Vec<(OrderedValue, RowId)>,
}

/// Staged in-place B-tree RowId patches for unchanged-key UPDATE (item 47).
/// One per secondary BTree column.  Flushed coalesced after the UPDATE row
/// loop so a leaf touched by many rows gets one WAL page-image, not one per row.
struct PatchColBatch {
    #[allow(dead_code)]
    col_idx: usize,
    meta_page: PageId,
    patches: Vec<(OrderedValue, RowId, RowId)>, // (key, old_rid, new_rid)
}

/// One empty [`IndexColBatch`] per BTree/FullText indexed column of `table_def`.
fn init_index_batches(table_def: &TableDef) -> Vec<IndexColBatch> {
    let mut batches = Vec::new();
    for (col_idx, col) in table_def.columns.iter().enumerate() {
        if col.dropped {
            continue;
        }
        let Some(meta_page) = col.index_root else {
            continue;
        };
        match col.index {
            Some(IndexKind::BTree) => batches.push(IndexColBatch {
                col_idx,
                meta_page,
                is_fulltext: false,
                entries: Vec::new(),
            }),
            Some(IndexKind::FullText) => batches.push(IndexColBatch {
                col_idx,
                meta_page,
                is_fulltext: true,
                entries: Vec::new(),
            }),
            _ => {}
        }
    }
    batches
}

/// Initialise one [`PatchColBatch`] per secondary BTree column.
/// Used by the item-47 UPDATE path alongside the existing [`init_index_batches`].
fn init_patch_batches(table_def: &TableDef) -> Vec<PatchColBatch> {
    let mut out = Vec::new();
    for (col_idx, col) in table_def.columns.iter().enumerate() {
        if col.dropped {
            continue;
        }
        // Secondary BTree index.
        if let Some(meta_page) = col.index_root {
            if matches!(col.index, Some(IndexKind::BTree)) {
                out.push(PatchColBatch {
                    col_idx,
                    meta_page,
                    patches: Vec::new(),
                });
            }
        }
        // Unique-enforcement index — must also be batched, otherwise each row
        // calls patch_many with a single entry (one FPI per row = no savings).
        if let Some(meta_page) = col.unique_index_root {
            out.push(PatchColBatch {
                col_idx,
                meta_page,
                patches: Vec::new(),
            });
        }
    }
    out
}

/// Item 47 Phase A — stage one row's index writes for UPDATE, routing
/// unchanged-key columns into the per-leaf coalesced patch path and
/// changed-key columns into the existing insert batch.
///
/// Unchanged-key secondary BTree: push `(key, old_rid, new_rid)` to
/// `patch_batches` — flushed after the row loop with one WAL page-image per
/// leaf instead of one per row.  Unique-enforcement indexes are handled
/// inline (per-row) but their patch batches are also accumulated here via
/// `unique_patches`.
///
/// Changed-key, FullText, and HNSW columns fall through to the standard
/// insert/coalesce path (existing behaviour).
#[allow(clippy::too_many_arguments)]
fn stage_row_index_writes_update(
    table_def: &TableDef,
    old_row_id: RowId,
    new_row_id: RowId,
    before_row: &[Literal],
    new_row: &[Literal],
    index_batches: &mut [IndexColBatch],
    patch_batches: &mut [PatchColBatch],
    ctx: &mut ExecCtx,
) -> Result<()> {
    // Secondary BTree / FullText indexes.
    for ib in index_batches.iter_mut() {
        let old_val = &before_row[ib.col_idx];
        let new_val = &new_row[ib.col_idx];
        if ib.is_fulltext {
            if let Literal::Text(text) = new_val {
                for token in crate::fulltext::tokenize(text) {
                    ib.entries.push((OrderedValue::Text(token), new_row_id));
                }
            }
        } else if old_val == new_val {
            if let Ok(key) = OrderedValue::try_from(new_val) {
                // Find the corresponding patch batch by meta_page.
                if let Some(pb) = patch_batches
                    .iter_mut()
                    .find(|p| p.meta_page == ib.meta_page)
                {
                    pb.patches.push((key, old_row_id, new_row_id));
                }
            }
        } else if let Ok(value) = OrderedValue::try_from(new_val) {
            ib.entries.push((value, new_row_id));
        }
    }

    // Per-column: HNSW and unique-enforcement indexes.
    for (idx, col) in table_def.columns.iter().enumerate() {
        if col.dropped {
            continue;
        }
        if let Some(meta_page) = col.index_root {
            if let Some(IndexKind::Hnsw) = col.index {
                if let Literal::Vector(v) = &new_row[idx] {
                    DiskIvfIndex::open(meta_page, ctx.page_size)
                        .insert(new_row_id, v, ctx.pool, ctx.wal)?;
                }
            }
        }
        if let Some(uiq_meta) = col.unique_index_root {
            if before_row[idx] == new_row[idx] {
                if let Ok(key) = OrderedValue::try_from(&new_row[idx]) {
                    // Unique index, unchanged key: accumulate into patch_batches
                    // (added by init_patch_batches). flush_patch_batches then calls
                    // patch_many once per leaf, not once per row.
                    if let Some(pb) = patch_batches.iter_mut().find(|p| p.meta_page == uiq_meta) {
                        pb.patches.push((key, old_row_id, new_row_id));
                    }
                }
            } else if let Ok(value) = OrderedValue::try_from(&new_row[idx]) {
                DiskBTree::new(uiq_meta, ctx.page_size)
                    .insert(value, new_row_id, ctx.pool, ctx.wal)?;
            }
        }
    }
    Ok(())
}

/// Flush the accumulated per-secondary-index patch batches (item 47).
/// Applies all `(key, old_rid→new_rid)` patches with one WAL page-image per
/// leaf (via [`DiskBTree::patch_many`]), then records a `BTreePatch` undo
/// action per patch so user-tx abort can restore `old_rid`.
fn flush_patch_batches(batches: &[PatchColBatch], ctx: &mut ExecCtx) -> Result<()> {
    use crate::txn::UndoAction;
    for b in batches {
        if b.patches.is_empty() {
            continue;
        }
        DiskBTree::new(b.meta_page, ctx.page_size).patch_many(&b.patches, ctx.pool, ctx.wal)?;
        for (key, old_rid, new_rid) in &b.patches {
            ctx.txn_mgr.record_undo(
                ctx.xid,
                UndoAction::BTreePatch {
                    meta_page: b.meta_page,
                    page_size: ctx.page_size,
                    key: key.clone(),
                    old_rid: *old_rid,
                    new_rid: *new_rid,
                },
            )?;
        }
    }
    Ok(())
}

/// Flush every staged [`IndexColBatch`] with one coalesced [`DiskBTree::insert_many`]
/// per column (A1). Called once after an UPDATE's row loop, on success.
fn flush_index_batches(batches: &[IndexColBatch], ctx: &mut ExecCtx) -> Result<()> {
    for b in batches {
        if b.entries.is_empty() {
            continue;
        }
        DiskBTree::new(b.meta_page, ctx.page_size).insert_many(&b.entries, ctx.pool, ctx.wal)?;
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
/// C1 (item 29): build and store the canonical CDC envelope for one DML event.
///
/// `before` = pre-mutation image (None for INSERT).
/// `after`  = post-mutation image (None for DELETE).
///
/// **Back-compat**: `payload` in the stored JSON equals `after` (INSERT/UPDATE)
/// or `before` (DELETE) — the same flat row object existing consumers read from
/// `event.payload["col"]`. The new `before`/`after` fields are additive.
/// Old events that predate item 29 store only the flat row; `resolve_event_candidates`
/// detects the absence of a `"payload"` key and falls back transparently.
fn send_event_capture(
    table_def: &TableDef,
    op: &str,
    before: Option<&[Literal]>,
    after: Option<&[Literal]>,
    ctx: &mut ExecCtx,
) -> Result<()> {
    if !table_def.events_enabled {
        return Ok(());
    }
    // Item 55 investigation: sub-step timing active under RUST_LOG=unidb=debug.
    let _outer_span = tracing::debug_span!("event_queue_capture").entered();
    let t0 = std::time::Instant::now();

    let before_json = before.map(|r| queue::payload::row_to_json(r, &table_def.columns));
    let after_json = after.map(|r| queue::payload::row_to_json(r, &table_def.columns));
    // Back-compat payload = after for INSERT/UPDATE, before for DELETE.
    let compat_payload = after_json
        .as_ref()
        .or(before_json.as_ref())
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let ts_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;

    let events_def = ctx.catalog.lookup(EVENTS_TABLE)?.clone();
    let heap = Heap::open(ctx.page_size, events_def.fsm_meta, events_def.pages.clone());

    let seq = ctx.next_event_seq.fetch_add(1, Ordering::SeqCst);

    // Canonical envelope stored in the payload JSON column (item 29, C1).
    // "payload" = back-compat flat row; "before"/"after"/"ts_ms"/"source" are new.
    // Consumers reading event.payload["col"] see the same flat object as before.
    let envelope = serde_json::json!({
        "payload": compat_payload,
        "before": before_json,
        "after": after_json,
        "ts_ms": ts_ms,
        "source": {
            "seq": seq,
            "txId": ctx.xid,
            "table": table_def.name,
            "schema": "public"
        }
    });
    let encoded = encode_row(&queue::event_row(
        seq as i64,
        ctx.xid as i64,
        &table_def.name,
        op,
        &envelope,
    ));
    let json_us = t0.elapsed().as_micros();

    let t1 = std::time::Instant::now();
    let row_id = heap.insert(&encoded, ctx.xid, ctx.pool, ctx.wal)?;
    let heap_us = t1.elapsed().as_micros();

    ctx.txn_mgr.record_undo(
        ctx.xid,
        UndoAction::Insert {
            page_id: row_id.page_id,
            slot: row_id.slot,
        },
    )?;

    // Q1 (item 26): maintain the durable seq index so poll_events is O(log n + returned).
    // Uses the same standalone-mini-txn insert as apply_durable_index_writes — the index
    // entry is WAL-logged before WAL_TXN_COMMIT, so it is always crash-consistent with
    // the heap row (both durable if the user txn commits; both absent if it aborts under
    // deferred sync). A stale entry from an aborted txn is harmless: MVCC re-check in
    // resolve_event_candidates filters it via NoVisibleVersion.
    let t2 = std::time::Instant::now();
    if let Some(meta_page) = ctx.event_seq_index_meta {
        DiskBTree::new(meta_page, ctx.page_size).insert(
            OrderedValue::Int(seq as i64),
            row_id,
            ctx.pool,
            ctx.wal,
        )?;
    }
    let btree_us = t2.elapsed().as_micros();

    let t3 = std::time::Instant::now();
    persist_pages_if_changed(EVENTS_TABLE, &heap, &events_def.pages, ctx)?;
    let persist_us = t3.elapsed().as_micros();

    tracing::debug!(
        seq,
        json_us,
        heap_us,
        btree_us,
        persist_us,
        payload_bytes = encoded.len(),
        "event_capture_substeps"
    );
    Ok(())
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExecResult {
    CreatedTable,
    CreatedIndex,
    Inserted {
        count: usize,
    },
    /// A result set: the output column names (in order) plus one value vector
    /// per row. `columns` lets the REST layer return `{columns, rows}` (a client
    /// can zip names to values) instead of anonymous positional arrays. For
    /// `SELECT *` the columns are the table's non-dropped columns in order; for
    /// an explicit projection they are exactly the projected names.
    Rows {
        columns: Vec<String>,
        rows: Vec<Vec<Literal>>,
    },
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
        LogicalPlan::Analyze { table } => exec_analyze(&table, ctx),
        LogicalPlan::Explain { analyze, spec } => {
            crate::sql::query_exec::exec_explain(&spec, analyze, ctx)
        }
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
    ctx.catalog
        .exclusive()?
        .add_column(table, column, &mut cctx)?;
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
    match ctx
        .catalog
        .exclusive()?
        .drop_column(table, column, &mut cctx)
    {
        Ok(()) => Ok(ExecResult::AlteredTable),
        // `IF EXISTS`: a missing column is not an error.
        Err(DbError::ColumnNotFound { .. }) if if_exists => Ok(ExecResult::AlteredTable),
        Err(e) => Err(e),
    }
}

fn exec_drop_table(table: &str, if_exists: bool, ctx: &mut ExecCtx) -> Result<ExecResult> {
    reject_system_table(table)?;
    let mut cctx = catalog_ctx!(ctx);
    match ctx.catalog.exclusive()?.drop_table(table, &mut cctx) {
        Ok(()) => Ok(ExecResult::DroppedTable),
        Err(DbError::TableNotFound(_)) if if_exists => Ok(ExecResult::DroppedTable),
        Err(e) => Err(e),
    }
}

/// `ANALYZE` (P4.d): scan the table's live rows under the statement snapshot,
/// compute statistics, and persist them on the catalog (durable — never
/// recomputed on open). Returns an empty result set.
fn exec_analyze(table: &str, ctx: &mut ExecCtx) -> Result<ExecResult> {
    let table_def = ctx.catalog.lookup(table)?.clone();
    let heap = Heap::open(ctx.page_size, table_def.fsm_meta, table_def.pages.clone());
    let snapshot = ctx.txn_mgr.snapshot_for_statement(ctx.xid)?;
    let mut rows = Vec::new();
    for (_, bytes) in heap.scan(&snapshot, ctx.xid, ctx.pool)? {
        rows.push(decode_row(&bytes, &table_def.columns)?);
    }
    // scan_pages() re-uses the FSM directory already loaded by scan() above —
    // ensure_directory() is idempotent and returns immediately on the second call.
    let page_count = heap.scan_pages(ctx.pool)?.len() as u64;
    let mut stats = crate::sql::statistics::compute(&rows, &table_def.columns);
    stats.page_count = page_count;
    let mut cctx = catalog_ctx!(ctx);
    ctx.catalog
        .exclusive()?
        .set_table_stats(table, stats, &mut cctx)?;
    Ok(ExecResult::Rows {
        columns: Vec::new(),
        rows: Vec::new(),
    })
}

fn exec_truncate(table: &str, ctx: &mut ExecCtx) -> Result<ExecResult> {
    reject_system_table(table)?;
    let table_def = ctx.catalog.lookup(table)?.clone();
    // Count the live rows removed (under this statement's snapshot) for the
    // result, before the page list is cleared.
    // Item 48: use count_visible (header-only, no decode) instead of scan().len().
    let heap = Heap::open(ctx.page_size, table_def.fsm_meta, table_def.pages.clone());
    let snapshot = ctx.txn_mgr.snapshot_for_statement(ctx.xid)?;
    let count = heap.count_visible(&snapshot, ctx.xid, ctx.pool)?;
    let mut cctx = catalog_ctx!(ctx);
    ctx.catalog.exclusive()?.truncate(table, &mut cctx)?;
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

    // Collect PK/UNIQUE columns whose types support a BTree index before
    // `name` and `columns` are moved into `def`.
    let pk_unique_indexable: Vec<String> = columns
        .iter()
        .filter(|c| {
            !c.dropped
                && (c.constraints.primary_key || c.constraints.unique)
                && matches!(
                    c.ty,
                    ColumnType::Int64 | ColumnType::Text | ColumnType::Bool
                )
        })
        .map(|c| c.name.clone())
        .collect();
    let table_name = name.clone();

    let def = TableDef {
        name,
        columns,
        pages: Vec::new(),
        fsm_meta: None,
        rls_policy: None,
        events_enabled: false,
        serial_next,
        constraints,
        generation: 0,
    };
    let mut cctx = catalog_ctx!(ctx);
    ctx.catalog.exclusive()?.create_table(def, &mut cctx)?;

    // Item 35: For every column with a PRIMARY KEY or UNIQUE constraint whose
    // type is BTree-indexable (Int64/Text/Bool), create an implicit durable
    // B-tree that `enforce_unique` can use for O(1) point-lookup enforcement
    // instead of an O(n) heap scan. The tree is empty at creation (no rows
    // yet); it is maintained by `apply_durable_index_writes` and
    // `apply_durable_index_writes` / `stage_row_index_writes_update` on every subsequent INSERT/UPDATE.
    //
    // Stored in `ColumnDef.unique_index_root` (separate from `index_root` for
    // the explicit secondary index so both can coexist without conflict).
    // `#[serde(default)]` on the field means pre-item-35 catalogs open with
    // `None` and fall back to the heap-scan path — no FORMAT_VERSION bump.
    for col_name in &pk_unique_indexable {
        let tree = DiskBTree::create(ctx.pool, ctx.wal)?;
        let mut cctx2 = catalog_ctx!(ctx);
        ctx.catalog.exclusive()?.set_column_unique_index_root(
            &table_name,
            col_name,
            Some(tree.meta_page()),
            &mut cctx2,
        )?;
    }

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
        .exclusive()?
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
        let heap = Heap::open(ctx.page_size, table_def.fsm_meta, table_def.pages.clone());
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
        // P3.a/P3.b: a durable BTree/FullText index — sort-then-bulk-load
        // (item 40). Phase 1: collect (key, row_id) pairs from the heap.
        // Phase 2: sort by key. Phase 3: bulk-insert via insert_many, which
        // wraps the entire build in one WAL mini-txn (one fsync vs. one per
        // row) and coalesces per-leaf WAL writes. Sorted input drives keys
        // rightward, filling leaf pages to ~90-95% and eliminating random
        // splits — the dominant cost in the unsorted path.
        let tree = DiskBTree::create(ctx.pool, ctx.wal)?;
        let heap = Heap::open(ctx.page_size, table_def.fsm_meta, table_def.pages.clone());
        // Phase 1: collect
        let mut pairs: Vec<(OrderedValue, RowId)> = Vec::new();
        for (row_id, bytes) in heap.scan(&snapshot, ctx.xid, ctx.pool)? {
            let row = decode_row(&bytes, &table_def.columns)?;
            match kind {
                IndexKind::BTree => {
                    if let Ok(value) = OrderedValue::try_from(&row[col_idx]) {
                        pairs.push((value, row_id));
                    }
                }
                IndexKind::FullText => {
                    if let Literal::Text(text) = &row[col_idx] {
                        for token in crate::fulltext::tokenize(text) {
                            pairs.push((OrderedValue::Text(token), row_id));
                        }
                    }
                }
                _ => unreachable!(),
            }
        }
        // Phase 2: sort by key so rightmost-leaf inserts dominate and page
        // fill approaches capacity (insert_many also sorts, but pre-sorting
        // makes the internal sort O(N) on already-sorted input).
        pairs.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
        // Phase 3: bulk insert — one WAL mini-txn, one fsync for all pairs.
        tree.insert_many(&pairs, ctx.pool, ctx.wal)?;
        tree.meta_page()
    };

    let mut cctx = catalog_ctx!(ctx);
    ctx.catalog
        .exclusive()?
        .set_column_index_root(table, column, Some(meta_page), &mut cctx)?;
    Ok(ExecResult::CreatedIndex)
}

/// Persist a **legacy** table's page list back to the catalog if the heap grew
/// during this statement's execution. For an **FSM-backed** table (the common
/// case since the durable-FSM milestone) this is a no-op: the page directory
/// lives in the durable FSM tree and self-persists at page-alloc time, so there
/// is no catalog `pages` blob to rewrite — which is exactly what removed the
/// O(heap-pages) blob-overflow (`HeapFull`) ceiling. Guarding on `fsm_meta`
/// keeps pre-FSM catalogs (no `fsm_meta`) working via their old in-catalog list.
fn persist_pages_if_changed(
    table: &str,
    heap: &Heap,
    original: &[PageId],
    ctx: &mut ExecCtx,
) -> Result<()> {
    if heap.is_fsm_backed() {
        return Ok(());
    }
    if heap.page_ids() != original {
        let new_pages = heap.page_ids().to_vec();
        let mut cctx = catalog_ctx!(ctx);
        ctx.catalog
            .exclusive()?
            .set_pages(table, new_pages, &mut cctx)?;
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
    enforce_referenced_tables_exist(&table_def, ctx.catalog.get())?;
    let heap = Heap::open(ctx.page_size, table_def.fsm_meta, table_def.pages.clone());

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
        // UNIQUE + FK — two-step approach for concurrent-writer safety
        // (item 35 inv. 3 / item 36 inv. 3): both phantom locks must be
        // acquired BEFORE taking the snapshot so that any concurrent winner
        // (another inserter racing the same unique key, or a parent deleter
        // racing this child insert) is already committed and visible when the
        // snapshot is taken.
        //
        // Step 1a: UniqueKey phantom locks (item 35).
        for (col_idx, col) in table_def.columns.iter().enumerate() {
            if col.dropped || col.unique_index_root.is_none() {
                continue;
            }
            if let Ok(key) = OrderedValue::try_from(&coerced[col_idx]) {
                let lock_id = unique_key_record_id(&table_def.name, &col.name, &key);
                ctx.lock_mgr.acquire_blocking(lock_id, ctx.xid)?;
            }
        }
        // Step 1b: FkKey phantom locks (item 36) — before snapshot so a
        // concurrent parent deleter either already committed (FK violation
        // follows) or blocks here and sees the committed child after we commit.
        acquire_fk_key_locks(
            &table_def,
            &coerced,
            ctx.xid,
            ctx.lock_mgr,
            ctx.catalog.get(),
        )?;
        // Step 2: take snapshot AFTER all phantom locks — every concurrent
        // winner is now visible.
        let snapshot = ctx.txn_mgr.snapshot_for_statement(ctx.xid)?;
        enforce_unique(
            &table_def, &coerced, &heap, &snapshot, ctx.xid, ctx.pool, None,
        )?;
        // Step 3: FK row-existence check (item 36). NULL columns are
        // skipped; own-xid rows (same-txn parent) are visible via get_visible.
        enforce_fk_rows_exist(
            &table_def,
            &coerced,
            &snapshot,
            ctx.xid,
            ctx.pool,
            ctx.catalog.get(),
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
        // C1 (item 29): INSERT has after-only; no pre-image.
        send_event_capture(&table_def, "insert", None, Some(&coerced), ctx)?;
        count += 1;
    }

    persist_pages_if_changed(table, &heap, &table_def.pages, ctx)?;
    assert_schema_stable(ctx, table, table_def.generation);
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

    if let Some((column, query_expr)) = predicate.as_ref().and_then(find_match) {
        let query_str = match query_expr {
            Expr::Literal(Literal::Text(q)) => q.clone(),
            other => {
                return Err(DbError::SqlUnsupported(format!(
                    "MATCH: query argument must be a TEXT literal or a bound $n parameter; \
                     got {other:?}"
                )))
            }
        };
        return exec_select_match(&table_def, projection, predicate, column, &query_str, ctx);
    }

    if let Some(hit) = predicate.as_ref().and_then(|e| {
        find_best_indexable_btree_predicate(e, &table_def, ctx.catalog.table_stats(&table_def.name))
    }) {
        if index_lookup_is_selective(&table_def, hit, ctx, true) {
            if let Some(result) =
                try_exec_select_btree(&table_def, projection, predicate, hit, ctx)?
            {
                return Ok(result);
            }
        }
    }

    let heap = Heap::open(ctx.page_size, table_def.fsm_meta, table_def.pages.clone());
    let snapshot = ctx.txn_mgr.snapshot_for_statement(ctx.xid)?;

    // B2 projection/qual decode pushdown: materialize only the predicate columns
    // to test the filter, and only materialize projection columns for surviving
    // rows — so a large `TEXT`/`Bytea` value nobody projects, or a row the
    // predicate rejects, never pays its `String` allocation.
    let cols = &table_def.columns;
    let ncols = cols.len();
    let proj_cols = projection_columns(&table_def, projection);
    let mut pred_cols = Vec::new();
    if let Some(pred) = predicate {
        expr_columns(pred, &table_def, &mut pred_cols);
    }
    let (pred_needed, pred_upto) = needed_mask(&pred_cols, ncols);
    let mut full_cols = proj_cols;
    full_cols.extend_from_slice(&pred_cols);
    let (full_needed, full_upto) = needed_mask(&full_cols, ncols);
    let has_pred = predicate.is_some();

    // The two-phase B2 decode as a closure, shared by the serial and parallel
    // paths: predicate columns → test → projection columns only on a match.
    let per_row = |bytes: &[u8]| -> Result<Option<Vec<Literal>>> {
        if has_pred {
            let prow = deform_row(bytes, cols, pred_upto, &pred_needed)?;
            if !predicate_matches(predicate, cols, &prow)? {
                return Ok(None);
            }
        }
        let row = deform_row(bytes, cols, full_upto, &full_needed)?;
        Ok(Some(project_row(projection, cols, &row)?))
    };

    // P-b: parallelize the full scan across worker threads when the table is
    // large enough (Milestone P). A plain `SELECT` has no `ORDER BY` (that routes
    // through the Query engine), so the concat of per-worker results is correct;
    // SSI read-set tracking is preserved by noting the gathered `read_ids`.
    let pages = heap.scan_pages(ctx.pool)?;
    if let Some(lease) = crate::sql::parallel_scan::acquire(pages.len()) {
        let (rows, read_ids) = crate::sql::parallel_scan::parallel_filter_project(
            &pages,
            &ctx.pool.shared_reader(),
            &snapshot,
            ctx.xid,
            lease.degree(),
            &|_rid, bytes| per_row(bytes),
        )?;
        ctx.txn_mgr.ssi_note_reads(ctx.xid, &read_ids);
        return Ok(ExecResult::Rows {
            columns: output_columns(projection, &table_def.columns),
            rows,
        });
    }

    let mut out = Vec::new();
    let mut read_ids = Vec::new();
    for (i, (row_id, bytes)) in heap
        .scan(&snapshot, ctx.xid, ctx.pool)?
        .into_iter()
        .enumerate()
    {
        if i % 1024 == 0 {
            crate::query_limits::check()?; // P5.f: timeout / cancellation
        }
        if let Some(row) = per_row(&bytes)? {
            // P1.d: this row is part of the statement's read set (an SSI
            // rw-antidependency source). No-op unless `xid` is serializable.
            read_ids.push(row_id);
            out.push(row);
        }
    }
    ctx.txn_mgr.ssi_note_reads(ctx.xid, &read_ids);
    Ok(ExecResult::Rows {
        columns: output_columns(projection, &table_def.columns),
        rows: out,
    })
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
    let heap = Heap::open(
        reader.page_size(),
        table_def.fsm_meta,
        table_def.pages.clone(),
    );
    // B2 decode pushdown (same as `exec_select`, for the concurrent-read path).
    let cols = &table_def.columns;
    let ncols = cols.len();
    let proj_cols = projection_columns(&table_def, projection);
    let mut pred_cols = Vec::new();
    if let Some(pred) = predicate {
        expr_columns(pred, &table_def, &mut pred_cols);
    }
    let (pred_needed, pred_upto) = needed_mask(&pred_cols, ncols);
    let mut full_cols = proj_cols;
    full_cols.extend_from_slice(&pred_cols);
    let (full_needed, full_upto) = needed_mask(&full_cols, ncols);
    let has_pred = predicate.is_some();

    let mut out = Vec::new();
    for (i, (_, bytes)) in heap
        .scan(snapshot, self_xid, reader)?
        .into_iter()
        .enumerate()
    {
        if i % 1024 == 0 {
            crate::query_limits::check()?; // P5.f: timeout / cancellation
        }
        if has_pred {
            let prow = deform_row(&bytes, cols, pred_upto, &pred_needed)?;
            if !predicate_matches(predicate, cols, &prow)? {
                continue;
            }
        }
        let row = deform_row(&bytes, cols, full_upto, &full_needed)?;
        out.push(project_row(projection, cols, &row)?);
    }
    Ok(ExecResult::Rows {
        columns: output_columns(projection, &table_def.columns),
        rows: out,
    })
}

/// Whether `plan` may run on the concurrent read path (6b): a plain `SELECT`
/// with no NEAR or MATCH term. NEAR needs the HNSW index; MATCH needs the
/// FULLTEXT index — both require `ExecCtx` and must stay on the writer path.
pub(crate) fn plan_is_concurrent_read(plan: &LogicalPlan) -> bool {
    match plan {
        LogicalPlan::Select { predicate, .. } => {
            let pred = predicate.as_ref();
            pred.and_then(find_near).is_none() && pred.and_then(find_match).is_none()
        }
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

/// Find a top-level (or top-level-AND'd) `Expr::Match` in a predicate (G11,
/// item 30). Same walk as `find_near` — AND-only grammar, no `OR`/nesting.
pub(crate) fn find_match(expr: &Expr) -> Option<(&str, &Expr)> {
    match expr {
        Expr::Match { column, query } => Some((column.as_str(), query.as_ref())),
        Expr::And(lhs, rhs) => find_match(lhs).or_else(|| find_match(rhs)),
        _ => None,
    }
}

/// SQL `[I]LIKE` pattern matching (G9, item 30).
///
/// `%` = any run of characters (including empty), `_` = exactly one character,
/// every other character matches itself literally. `case_insensitive = true`
/// folds both sides to lowercase before matching (ILIKE semantics).
///
/// The algorithm is a straightforward recursive backtrack over char slices —
/// O(n·m) worst-case, sufficient for V1. Pure-prefix optimisation (`'abc%'` →
/// B-tree range) is tracked as a follow-up in the item-30 spec.
pub(crate) fn like_match(text: &str, pattern: &str, case_insensitive: bool) -> bool {
    if case_insensitive {
        let t: Vec<char> = text.chars().flat_map(|c| c.to_lowercase()).collect();
        let p: Vec<char> = pattern.chars().flat_map(|c| c.to_lowercase()).collect();
        like_match_chars(&t, &p)
    } else {
        let t: Vec<char> = text.chars().collect();
        let p: Vec<char> = pattern.chars().collect();
        like_match_chars(&t, &p)
    }
}

fn like_match_chars(text: &[char], pattern: &[char]) -> bool {
    match pattern.first() {
        None => text.is_empty(),
        Some('%') => {
            // `%` matches any sequence — try consuming zero or more text chars.
            like_match_chars(text, &pattern[1..])
                || (!text.is_empty() && like_match_chars(&text[1..], pattern))
        }
        Some('_') => !text.is_empty() && like_match_chars(&text[1..], &pattern[1..]),
        Some(&c) => !text.is_empty() && text[0] == c && like_match_chars(&text[1..], &pattern[1..]),
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

/// Like `find_indexable_btree_predicate` but for `AND` expressions it evaluates
/// both arms with `ANALYZE` statistics and returns the **most selective** one.
///
/// This matters for queries like `WHERE k >= 0 AND k < N` where both arms are
/// sargable on the same BTree column.  The naive left-first pick returns
/// `k >= 0` (selectivity ≈ 1.0 when 0 is the column minimum), causing the
/// B-tree scan to fetch every row before the upper-bound filter is applied.
/// Picking `k < N` (selectivity ≈ 0.5) halves the candidate set and allows
/// the size-aware A3 gate to correctly prefer the index at large table sizes
/// and the sequential scan at small ones — matching Postgres's cost-based
/// range-bound selection.
///
/// When statistics are absent both arms are tried in left-to-right order
/// (same as `find_indexable_btree_predicate`), so the function is always safe
/// to call even on tables that have not yet been `ANALYZE`d.
fn find_best_indexable_btree_predicate<'a>(
    expr: &'a Expr,
    table_def: &TableDef,
    stats: Option<&crate::sql::statistics::TableStats>,
) -> Option<(&'a str, CmpOp, &'a Literal)> {
    match expr {
        Expr::And(lhs, rhs) => {
            let l = find_best_indexable_btree_predicate(lhs, table_def, stats);
            let r = find_best_indexable_btree_predicate(rhs, table_def, stats);
            match (l, r) {
                (None, r) => r,
                (Some(lhit), None) => Some(lhit),
                (Some(lhit), Some(rhit)) => {
                    // Both arms are sargable; pick whichever has lower estimated
                    // selectivity so the B-tree returns the smallest candidate set.
                    let sel_of = |(col, op, lit): (&str, CmpOp, &Literal)| {
                        stats
                            .and_then(|s| {
                                s.columns
                                    .get(col)
                                    .and_then(|cs| cs.selectivity(op, lit, s.row_count))
                            })
                            .unwrap_or(1.0)
                    };
                    if sel_of(rhit) < sel_of(lhit) {
                        Some(rhit)
                    } else {
                        Some(lhit)
                    }
                }
            }
        }
        _ => find_indexable_btree_predicate(expr, table_def),
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
    let heap = Heap::open(ctx.page_size, table_def.fsm_meta, table_def.pages.clone());
    let snapshot = ctx.txn_mgr.snapshot_for_statement(ctx.xid)?;

    // B2 decode pushdown on the index-resolved candidates (the SELECT-filtered
    // hot path, since a range predicate is served here, not by the full scan):
    // materialize the predicate columns to re-check the row, and the projection
    // columns only for survivors.
    let cols = &table_def.columns;
    let ncols = cols.len();
    let proj_cols = projection_columns(table_def, projection);
    let mut pred_cols = Vec::new();
    if let Some(pred) = predicate {
        expr_columns(pred, table_def, &mut pred_cols);
    }
    let (pred_needed, pred_upto) = needed_mask(&pred_cols, ncols);
    let mut full_cols = proj_cols;
    full_cols.extend_from_slice(&pred_cols);
    let (full_needed, full_upto) = needed_mask(&full_cols, ncols);
    let has_pred = predicate.is_some();

    // The B2 two-phase decode as a closure, shared by all resolution paths.
    let per_candidate = |bytes: &[u8]| -> Result<Option<Vec<Literal>>> {
        if has_pred {
            let prow = deform_row(bytes, cols, pred_upto, &pred_needed)?;
            if !predicate_matches(predicate, cols, &prow)? {
                return Ok(None);
            }
        }
        let row = deform_row(bytes, cols, full_upto, &full_needed)?;
        Ok(Some(project_row(projection, cols, &row)?))
    };

    // Lever 1 (item 45): for range predicates, acquire workers optimistically
    // before the B-tree scan and dispatch each pre-partitioned slice to exactly
    // one worker (static assignment, no work-stealing cursor). Each partition
    // covers a contiguous key range, so each worker's heap accesses are
    // clustered by key-ordered insertion locality — lower page-cache pressure
    // than the interleaved access pattern of work-stealing over a flat list.
    // `usize::MAX` bypasses the MIN_PAGES floor in `acquire`; we enforce
    // PARALLEL_CANDIDATE_MIN ourselves after counting the collected total.
    let range_op = match op {
        CmpOp::Lt => Some(RangeOp::Lt),
        CmpOp::Le => Some(RangeOp::Le),
        CmpOp::Gt => Some(RangeOp::Gt),
        CmpOp::Ge => Some(RangeOp::Ge),
        _ => None,
    };
    if let Some(rop) = range_op {
        let maybe_lease = crate::sql::parallel_scan::acquire(usize::MAX);
        if let Some(lease) = maybe_lease {
            let degree = lease.degree();
            let partitions = tree.search_range_partition(rop, &value, degree, ctx.pool)?;
            let total: usize = partitions.iter().map(|p| p.len()).sum();

            if total >= crate::sql::parallel_scan::PARALLEL_CANDIDATE_MIN {
                let reader = ctx.pool.shared_reader();
                let deadline = crate::query_limits::snapshot_deadline();
                let stop = AtomicBool::new(false);
                let err: Mutex<Option<DbError>> = Mutex::new(None);
                let parts: Mutex<Vec<Vec<Vec<Literal>>>> = Mutex::new(Vec::new());
                std::thread::scope(|s| {
                    for part in &partitions {
                        let reader = reader.clone();
                        // Rebind shared state as references so the `move`
                        // closure captures them by pointer, not by value.
                        let (stop, err, deadline, parts) = (&stop, &err, &deadline, &parts);
                        let (snapshot, per_candidate) = (&snapshot, &per_candidate);
                        let xid = ctx.xid;
                        s.spawn(move || {
                            let mut rows: Vec<Vec<Literal>> = Vec::new();
                            for (i, &rid) in part.iter().enumerate() {
                                if stop.load(Ordering::Relaxed) {
                                    break;
                                }
                                if i % 64 == 0 {
                                    if let Err(e) = deadline.check() {
                                        *err.lock().unwrap_or_else(|p| p.into_inner()) = Some(e);
                                        stop.store(true, Ordering::Relaxed);
                                        break;
                                    }
                                }
                                let bytes = match get_visible(&reader, rid, snapshot, xid) {
                                    Ok(Some(b)) => b,
                                    Ok(None) => continue,
                                    Err(e) => {
                                        *err.lock().unwrap_or_else(|p| p.into_inner()) = Some(e);
                                        stop.store(true, Ordering::Relaxed);
                                        break;
                                    }
                                };
                                match per_candidate(&bytes) {
                                    Ok(Some(row)) => rows.push(row),
                                    Ok(None) => {}
                                    Err(e) => {
                                        *err.lock().unwrap_or_else(|p| p.into_inner()) = Some(e);
                                        stop.store(true, Ordering::Relaxed);
                                        break;
                                    }
                                }
                            }
                            parts.lock().unwrap_or_else(|p| p.into_inner()).push(rows);
                        });
                    }
                });
                if let Some(e) = err.into_inner().unwrap_or_else(|p| p.into_inner()) {
                    return Err(e);
                }
                let mut rows = Vec::new();
                for part_rows in parts.into_inner().unwrap_or_else(|p| p.into_inner()) {
                    rows.extend(part_rows);
                }
                return Ok(Some(ExecResult::Rows {
                    columns: output_columns(projection, &table_def.columns),
                    rows,
                }));
            }

            // Total below PARALLEL_CANDIDATE_MIN: reuse already-collected RowIds
            // for the serial path — avoids a second B-tree scan.
            // lease drops here, returning workers to the global pool.
            let candidate_ids: Vec<RowId> = partitions.into_iter().flatten().collect();
            drop(lease);
            let mut out = Vec::new();
            for row_id in candidate_ids {
                let bytes = match heap.get(row_id, &snapshot, ctx.xid, ctx.pool) {
                    Ok(b) => b,
                    Err(DbError::NoVisibleVersion { .. }) => continue,
                    Err(e) => return Err(e),
                };
                if let Some(row) = per_candidate(&bytes)? {
                    out.push(row);
                }
            }
            return Ok(Some(ExecResult::Rows {
                columns: output_columns(projection, &table_def.columns),
                rows: out,
            }));
        }
        // Workers unavailable — fall through to the serial/Eq path below.
    }

    // Non-range predicates (Eq) or workers unavailable: serial B-tree scan.
    // The existing parallel_resolve_candidates work-stealing path handles Eq
    // with many duplicate keys; range fallback just goes fully serial.
    let candidate_ids: Vec<RowId> = match tree.search(op, &value, ctx.pool)? {
        Some(ids) => ids,
        None => return Ok(None),
    };
    let maybe_lease = if candidate_ids.len() >= crate::sql::parallel_scan::PARALLEL_CANDIDATE_MIN {
        crate::sql::parallel_scan::acquire(candidate_ids.len())
    } else {
        None
    };
    if let Some(lease) = maybe_lease {
        let rows = crate::sql::parallel_scan::parallel_resolve_candidates(
            &candidate_ids,
            &ctx.pool.shared_reader(),
            &snapshot,
            ctx.xid,
            lease.degree(),
            &|_rid, bytes| per_candidate(bytes),
        )?;
        return Ok(Some(ExecResult::Rows {
            columns: output_columns(projection, &table_def.columns),
            rows,
        }));
    }

    let mut out = Vec::new();
    for row_id in candidate_ids {
        let bytes = match heap.get(row_id, &snapshot, ctx.xid, ctx.pool) {
            Ok(b) => b,
            Err(DbError::NoVisibleVersion { .. }) => continue,
            Err(e) => return Err(e),
        };
        if let Some(row) = per_candidate(&bytes)? {
            out.push(row);
        }
    }
    Ok(Some(ExecResult::Rows {
        columns: output_columns(projection, &table_def.columns),
        rows: out,
    }))
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
        return Ok(ExecResult::Rows {
            columns: output_columns(projection, &table_def.columns),
            rows: Vec::new(),
        });
    };

    // Probe the nearest cells' posting lists for candidate RowIds. Candidates are
    // then re-checked against the full predicate below (MVCC visibility, RLS, any
    // AND'd WHERE terms) and exact-re-ranked from the heap's stored vectors, so
    // the over-fetch-then-filter contract is identical to a full scan's per-row
    // check (see `eval_expr`'s `Expr::Near` arm for the other half).
    let ivf = DiskIvfIndex::open(meta_page, ctx.page_size);
    let (metric, candidate_ids) = ivf.candidates(query, None, ctx.pool)?;

    let heap = Heap::open(ctx.page_size, table_def.fsm_meta, table_def.pages.clone());
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
        scored.push((
            dist,
            project_row_near(projection, &table_def.columns, &row, dist)?,
        ));
    }
    scored.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(k);
    Ok(ExecResult::Rows {
        columns: output_columns(projection, &table_def.columns),
        rows: scored.into_iter().map(|(_, r)| r).collect(),
    })
}

/// `MATCH`'s over-fetch-then-filter execution (G11, item 30): probe the
/// FULLTEXT index for each query token's posting list, intersect them
/// (AND-all-tokens semantics, same as `Engine::search_fulltext`), then run
/// each surviving candidate through the full predicate for MVCC / RLS / other
/// AND'd WHERE terms. `eval_expr`'s `Expr::Match` arm returns `true` so the
/// predicate check passes for matched candidates.
fn exec_select_match(
    table_def: &TableDef,
    projection: &[String],
    predicate: &Option<Expr>,
    column: &str,
    query: &str,
    ctx: &mut ExecCtx,
) -> Result<ExecResult> {
    let col = table_def
        .columns
        .iter()
        .find(|c| c.name == column && !c.dropped)
        .ok_or_else(|| DbError::ColumnNotFound {
            table: table_def.name.clone(),
            column: column.to_string(),
        })?;
    let meta = match (col.index, col.index_root) {
        (Some(IndexKind::FullText), Some(m)) => m,
        _ => {
            return Err(DbError::SqlPlan(format!(
                "MATCH: column '{column}' has no FULLTEXT index; \
                 create one with CREATE INDEX … USING FULLTEXT ({column})"
            )))
        }
    };
    let tokens = crate::fulltext::tokenize(query);
    if tokens.is_empty() {
        return Ok(ExecResult::Rows {
            columns: output_columns(projection, &table_def.columns),
            rows: Vec::new(),
        });
    }
    let tree = DiskBTree::new(meta, ctx.page_size);
    let mut posting_lists: Vec<Vec<RowId>> = Vec::with_capacity(tokens.len());
    for token in &tokens {
        posting_lists.push(tree.search_eq(&OrderedValue::Text(token.clone()), ctx.pool)?);
    }
    // AND-intersect: start from the shortest posting list so the result shrinks fastest.
    posting_lists.sort_by_key(|l| l.len());
    let mut candidates: std::collections::HashSet<RowId> =
        posting_lists[0].iter().copied().collect();
    for list in &posting_lists[1..] {
        let set: std::collections::HashSet<RowId> = list.iter().copied().collect();
        candidates.retain(|r| set.contains(r));
        if candidates.is_empty() {
            break;
        }
    }
    let heap = Heap::open(ctx.page_size, table_def.fsm_meta, table_def.pages.clone());
    let snapshot = ctx.txn_mgr.snapshot_for_statement(ctx.xid)?;
    let mut out = Vec::new();
    for rid in candidates {
        let bytes = match heap.get(rid, &snapshot, ctx.xid, ctx.pool) {
            Ok(b) => b,
            Err(DbError::NoVisibleVersion { .. }) => continue,
            Err(e) => return Err(e),
        };
        let row = decode_row(&bytes, &table_def.columns)?;
        if !predicate_matches(predicate, &table_def.columns, &row)? {
            continue;
        }
        out.push(project_row(projection, &table_def.columns, &row)?);
    }
    Ok(ExecResult::Rows {
        columns: output_columns(projection, &table_def.columns),
        rows: out,
    })
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
    enforce_referenced_tables_exist(&table_def, ctx.catalog.get())?;
    let heap = Heap::open(ctx.page_size, table_def.fsm_meta, table_def.pages.clone());
    let snapshot = ctx.txn_mgr.snapshot_for_statement(ctx.xid)?;

    let matching = matching_rows(&heap, &snapshot, ctx, &table_def, predicate)?;
    // P1.d: the rows an UPDATE selects are part of its read set (SSI).
    let read_ids: Vec<RowId> = matching.iter().map(|(rid, _)| *rid).collect();
    ctx.txn_mgr.ssi_note_reads(ctx.xid, &read_ids);

    // A1: accumulate BTree/FullText index entries across every updated row, then
    // flush them coalesced after the loop — one WAL_INDEX page image per dirtied
    // leaf instead of one per row (RC2). Correctness is unchanged: every entry is
    // still inserted (see `stage_row_index_writes_update`).
    let mut index_batches = init_index_batches(&table_def);
    // Item 47: per-secondary-BTree patch batches (unchanged-key in-place RowId patch).
    let mut patch_batches = init_patch_batches(&table_def);
    // A4: whether any UNIQUE/PRIMARY KEY set exists at all — computed once, not
    // per row. When there are none, the loop skips both the per-row
    // `snapshot_for_statement` allocation *and* `enforce_unique`'s full-heap
    // scan (which would otherwise fire per row → RC5's O(N²)); the check is a
    // no-op in that case (`enforce_unique` early-returns on empty active sets).
    let has_unique = !unique_column_sets(&table_def)?.is_empty();
    // Item 36: gate FK child-side check (new values reference valid parents).
    let has_fk_refs = table_def
        .columns
        .iter()
        .any(|c| !c.dropped && c.constraints.references.is_some())
        || !table_def.constraints.foreign_keys.is_empty();
    // Item 53: skip FK child-side enforcement when no FK column appears in the
    // SET clause. The new row version copies the unchanged FK column value from
    // the old version; the old version already satisfied the constraint, so the
    // new one does identically. Conservative rule: any column named on the LHS
    // of any assignment is "written" — `SET customer_id = other_col` is
    // detected because `customer_id` is the explicit assignment target.
    let has_fk_refs_in_set = has_fk_refs && {
        let set_col_names: std::collections::HashSet<&str> =
            assignments.iter().map(|(col, _)| col.as_str()).collect();
        table_def.columns.iter().any(|c| {
            !c.dropped
                && c.constraints.references.is_some()
                && set_col_names.contains(c.name.as_str())
        }) || table_def.constraints.foreign_keys.iter().any(|fk| {
            fk.columns
                .iter()
                .any(|col| set_col_names.contains(col.as_str()))
        })
    };
    // Item 36: gate FK parent-side RESTRICT (does any child table reference us?).
    let has_fk_children = table_has_fk_children(ctx.catalog.get(), table);
    let mut count = 0;
    for (row_id, bytes) in matching {
        let mut row = decode_row(&bytes, &table_def.columns)?;
        // C1 (item 29): snapshot the pre-mutation image before set_column overwrites it.
        let before_row = row.clone();
        for (col, expr) in assignments {
            let new_val = eval_expr(expr, &table_def.columns, &row)?;
            set_column(&table_def.columns, &mut row, col, new_val)?;
        }
        let coerced = coerce_and_validate_row(&table_def, row)?;
        enforce_not_null(&table_def, &coerced)?;
        enforce_checks(&table_def, &coerced)?;
        // UNIQUE + FK — acquire all phantom locks BEFORE taking a fresh
        // snapshot, then run uniqueness + FK checks with it (items 35/36/53).
        // RESTRICT on old PK also uses a fresh snapshot (after its lock).
        if has_unique || has_fk_refs_in_set || has_fk_children {
            // Step 1: acquire UniqueKey + FkKey (child-side) phantom locks.
            if has_unique {
                for (col_idx, col) in table_def.columns.iter().enumerate() {
                    if col.dropped || col.unique_index_root.is_none() {
                        continue;
                    }
                    if let Ok(key) = OrderedValue::try_from(&coerced[col_idx]) {
                        let lock_id = unique_key_record_id(&table_def.name, &col.name, &key);
                        ctx.lock_mgr.acquire_blocking(lock_id, ctx.xid)?;
                    }
                }
            }
            // Item 53: skip FkKey phantom lock + enforce when FK col not in SET.
            if has_fk_refs_in_set {
                acquire_fk_key_locks(
                    &table_def,
                    &coerced,
                    ctx.xid,
                    ctx.lock_mgr,
                    ctx.catalog.get(),
                )?;
            }
            // Step 1b: FkKey parent lock for RESTRICT (old PK value).
            if has_fk_children {
                acquire_fk_key_locks_parent(&table_def, &before_row, ctx.xid, ctx.lock_mgr)?;
            }
            // Step 2: fresh snapshot AFTER all phantom locks.
            let usnap = ctx.txn_mgr.snapshot_for_statement(ctx.xid)?;
            // UNIQUE: exclude the row's current version (old tuple still visible
            // to this snapshot until heap.update supersedes it).
            if has_unique {
                enforce_unique(
                    &table_def,
                    &coerced,
                    &heap,
                    &usnap,
                    ctx.xid,
                    ctx.pool,
                    Some(row_id),
                )?;
            }
            // FK child-side: new values must reference a visible parent row.
            // Item 53: skipped when FK column not in SET (value unchanged).
            if has_fk_refs_in_set {
                enforce_fk_rows_exist(
                    &table_def,
                    &coerced,
                    &usnap,
                    ctx.xid,
                    ctx.pool,
                    ctx.catalog.get(),
                )?;
            }
            // FK parent-side RESTRICT: old PK value must not be referenced.
            if has_fk_children {
                enforce_fk_restrict(
                    &table_def,
                    &before_row,
                    &usnap,
                    ctx.xid,
                    ctx.pool,
                    ctx.catalog.get(),
                )?;
            }
        }
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
        // Item 47: unchanged-key columns use in-place RowId patch (no splits, 1
        // WAL page-image); changed-key columns fall through to the batch insert.
        stage_row_index_writes_update(
            &table_def,
            row_id,
            new_row_id,
            &before_row,
            &coerced,
            &mut index_batches,
            &mut patch_batches,
            ctx,
        )?;
        // C1 (item 29): UPDATE carries both before (pre-mutation) and after (post-mutation).
        send_event_capture(&table_def, "update", Some(&before_row), Some(&coerced), ctx)?;
        count += 1;
    }
    // Coalesced index maintenance for the whole statement (A1).
    // Item 47: flush unchanged-key patches first (one WAL page-image per leaf),
    // then insert changed-key entries from the standard batch.
    flush_patch_batches(&patch_batches, ctx)?;
    flush_index_batches(&index_batches, ctx)?;

    persist_pages_if_changed(table, &heap, &table_def.pages, ctx)?;
    assert_schema_stable(ctx, table, table_def.generation);
    Ok(ExecResult::Updated { count })
}

fn exec_delete(table: &str, predicate: &Option<Expr>, ctx: &mut ExecCtx) -> Result<ExecResult> {
    let table_def = ctx.catalog.lookup(table)?.clone();
    let heap = Heap::open(ctx.page_size, table_def.fsm_meta, table_def.pages.clone());
    let snapshot = ctx.txn_mgr.snapshot_for_statement(ctx.xid)?;

    // Item 48: fast path for unconditional DELETE with no FK children and no CDC.
    // Routes through the O(pages) truncate instead of xmax-stamping every row.
    // CDC is skipped intentionally — TRUNCATE has never emitted per-row events.
    if predicate.is_none()
        && !table_has_fk_children(ctx.catalog.get(), table)
        && !table_def.events_enabled
    {
        let count = heap.count_visible(&snapshot, ctx.xid, ctx.pool)?;
        let mut cctx = catalog_ctx!(ctx);
        ctx.catalog.exclusive()?.truncate(table, &mut cctx)?;
        return Ok(ExecResult::Deleted { count });
    }

    let matching = matching_rows(&heap, &snapshot, ctx, &table_def, predicate)?;
    // P1.d: the rows a DELETE selects are part of its read set (SSI).
    let read_ids: Vec<RowId> = matching.iter().map(|(rid, _)| *rid).collect();
    ctx.txn_mgr.ssi_note_reads(ctx.xid, &read_ids);

    // Item 36: gate the RESTRICT scan — zero overhead when no table
    // references this one (the common case).
    let has_fk_children = table_has_fk_children(ctx.catalog.get(), table);
    let needs_per_row_checks = has_fk_children || table_def.events_enabled;

    // Item 44: two-pass DELETE — pre-check pass (per-row, unchanged semantics)
    // then batched heap mutations (one WAL mini-txn per page instead of per row).
    //
    // Pre-check pass: FK RESTRICT and CDC event capture must still run per-row
    // *before* any heap mutation (FK needs a fresh snapshot per row; CDC needs
    // the row data, which is gone after deletion).  All write-locks are acquired
    // inside `delete_many`, which runs after the pre-checks.
    //
    // When there are no per-row side-effects (no FK children, no CDC) we skip
    // the pre-check pass entirely and go straight to the batch delete.
    let row_ids: Vec<RowId> = if needs_per_row_checks {
        let mut ids = Vec::with_capacity(matching.len());
        for (row_id, bytes) in &matching {
            let row = decode_row(bytes, &table_def.columns)?;
            if has_fk_children {
                // Protocol (closes the parent-delete / child-insert race):
                //   1. Acquire FkKey phantom lock on parent PK value BEFORE snapshot.
                //   2. Take fresh snapshot — sees any child that committed before us.
                //   3. Scan referencing child tables; reject if any visible child found.
                acquire_fk_key_locks_parent(&table_def, &row, ctx.xid, ctx.lock_mgr)?;
                let restrict_snap = ctx.txn_mgr.snapshot_for_statement(ctx.xid)?;
                enforce_fk_restrict(
                    &table_def,
                    &row,
                    &restrict_snap,
                    ctx.xid,
                    ctx.pool,
                    ctx.catalog.get(),
                )?;
            }
            // C1 (item 29): before-only; captured before heap mutation.
            send_event_capture(&table_def, "delete", Some(&row), None, ctx)?;
            ids.push(*row_id);
        }
        ids
    } else {
        matching.iter().map(|(rid, _)| *rid).collect()
    };

    let deleted = match heap.delete_many(&row_ids, ctx.xid, ctx.pool, ctx.wal, ctx.lock_mgr) {
        Err(e @ DbError::WriteConflict { .. }) => return Err(classify_conflict(e, ctx)),
        other => other?,
    };

    // P1.d: SSI write tracking + undo log — one entry per deleted row.
    for rid in &deleted {
        ctx.txn_mgr.ssi_note_write(ctx.xid, *rid);
        ctx.txn_mgr.record_undo(
            ctx.xid,
            UndoAction::XmaxStamp {
                page_id: rid.page_id,
                slot: rid.slot,
            },
        )?;
    }
    let count = deleted.len();

    persist_pages_if_changed(table, &heap, &table_def.pages, ctx)?;
    assert_schema_stable(ctx, table, table_def.generation);
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

/// Rows selected for an UPDATE/DELETE: raw heap bytes paired with each RowId.
/// Callers decode lazily — DELETE's common path (no FK children, no CDC) never
/// needs column values at all (item 52 Phase B).
type MatchedRows = Vec<(RowId, Vec<u8>)>;

/// Legacy selectivity cap used as a fallback when `ANALYZE` predates the
/// page-count field (i.e. `TableStats::page_count == 0`).  Preserves the
/// original 50%-selective-DELETE behaviour for un-re-analyzed tables.
const INDEX_RANGE_SELECTIVITY_MAX: f64 = 0.3;

/// Per-row heap fetch cost (seq-page equivalents) on the **parallel** SELECT
/// path (`try_exec_select_btree` → `parallel_resolve_candidates`, 18 workers).
/// unidb pages live in the OS page cache via mmap so random vs. sequential
/// cost is much closer than in Postgres's buffer-pool model.  With 18 workers
/// amortising the per-call overhead, one `heap.get()` ≈ 0.012 seq-page reads.
///
/// Calibration (50% selectivity, 8 KiB pages, ~133 rows/page):
///   At 2 000 rows (15 pages, 1 000 matched):
///     15 > 4 + 1 000 × 0.012 = 16 → false → scan wins ✓
///   At 40 000 rows (296 pages, 20 000 matched):
///     296 > 4 + 20 000 × 0.012 = 244 → true → index wins ✓
const HEAP_FETCH_SEQ_EQUIV: f64 = 0.012;

/// Per-row heap fetch cost on the **serial** UPDATE/DELETE path (`matching_rows`
/// → `index_matching_rows`, one `heap.get()` per candidate, no workers).
/// Serial random access is measurably more expensive than parallel: no
/// amortised thread-spawn benefit, and the hardware prefetcher cannot pipeline
/// across independently scheduled random accesses.  Empirically ≈ 0.05
/// seq-page-read equivalents.
///
/// Calibration (50% selectivity — the DELETE regression case):
///   At 2 000 rows (15 pages, 1 000 matched):
///     15 > 4 + 1 000 × 0.05 = 54 → false → scan wins ✓
///   At 40 000 rows (296 pages, 20 000 matched):
///     296 > 4 + 20 000 × 0.05 = 1 004 → false → scan wins ✓
///   At 40 000 rows, 5% selectivity (2 000 matched):
///     296 > 4 + 2 000 × 0.05 = 104 → true → index wins ✓
const HEAP_FETCH_SEQ_EQUIV_SERIAL: f64 = 0.05;

/// Estimated overhead of the B-tree traversal in units of sequential page
/// reads.  For table sizes in the 1 k–1 M range the tree is 3–5 levels deep;
/// 4 is a safe midpoint.
const BTREE_STARTUP_PAGES: f64 = 4.0;

/// Whether the index-driven path is worth its overhead over a sequential scan
/// (A3 gate).  Used by both the SELECT path (`try_exec_select_btree`) and the
/// UPDATE/DELETE path (`matching_rows`).
///
/// `parallel`: `true` when the caller will resolve candidates via
/// `parallel_resolve_candidates` (SELECT fast path); `false` when each
/// candidate is resolved serially (UPDATE/DELETE via `index_matching_rows`).
/// The two paths have different per-row costs — parallel amortises thread
/// overhead across many workers, so its effective `heap.get()` cost is much
/// lower than the serial path's.  Using the parallel constant for serial
/// UPDATE/DELETE over-estimated the index benefit and regressed 50%-selective
/// DELETE at large table sizes (296 pages, 20 k matched: parallel said
/// "296 > 244 → index", but serial actually costs "296 > 1004 → scan").
///
/// For range predicates with `ANALYZE` page_count:
///   scan_cost  ≈ page_count
///   index_cost ≈ BTREE_STARTUP_PAGES + matched_rows × fetch_cost
///   where fetch_cost = HEAP_FETCH_SEQ_EQUIV      (parallel, 0.012)
///                    | HEAP_FETCH_SEQ_EQUIV_SERIAL (serial,   0.05)
///
/// Equality (`=`) is always taken — a point lookup is inherently selective.
/// When `page_count` is absent the gate falls back to the legacy fixed
/// selectivity threshold, preserving behaviour for un-re-ANALYZEd tables.
fn index_lookup_is_selective(
    table_def: &TableDef,
    hit: (&str, CmpOp, &Literal),
    ctx: &ExecCtx,
    parallel: bool,
) -> bool {
    let (column, op, literal) = hit;
    if matches!(op, CmpOp::Eq) {
        return true;
    }
    let Some(stats) = ctx.catalog.table_stats(&table_def.name) else {
        return false; // no ANALYZE evidence → prefer the sequential scan
    };
    let Some(col_stats) = stats.columns.get(column) else {
        return false;
    };
    let Some(selectivity) = col_stats.selectivity(op, literal, stats.row_count) else {
        return false;
    };
    // Size-aware cost model (requires ANALYZE with page_count, item 43).
    if stats.page_count > 0 {
        let fetch_cost = if parallel {
            HEAP_FETCH_SEQ_EQUIV
        } else {
            HEAP_FETCH_SEQ_EQUIV_SERIAL
        };
        let matched_rows = selectivity * stats.row_count as f64;
        let index_cost = BTREE_STARTUP_PAGES + matched_rows * fetch_cost;
        return (stats.page_count as f64) > index_cost;
    }
    // Fallback: page_count unavailable (pre-item-43 ANALYZE) → legacy gate.
    selectivity <= INDEX_RANGE_SELECTIVITY_MAX
}

fn matching_rows(
    heap: &Heap,
    snapshot: &crate::mvcc::Snapshot,
    ctx: &mut ExecCtx,
    table_def: &TableDef,
    predicate: &Option<Expr>,
) -> Result<MatchedRows> {
    // A3: when the predicate is a *selective* sargable range/equality on a
    // BTree-indexed column, drive row lookup from the durable B+tree instead of
    // a whole-heap scan — the same fast path SELECT already uses
    // (`try_exec_select_btree`), now shared by UPDATE/DELETE (fixes RC1: a
    // selective `DELETE … WHERE k = x` no longer decodes every row to find its
    // matches). Every live row carries a B-tree entry for its key (insert + the
    // A1 coalesced update both maintain it), so the index scan is complete;
    // candidates are re-checked against the full predicate + MVCC visibility, so
    // the result is identical to a scan.
    //
    // The `index_lookup_is_selective` gate matters: for a *non*-selective range
    // (e.g. `k >= N/2`, matching half the table) the index-driven path does one
    // random `heap.get` per match, which is slower than a single sequential full
    // scan — so only equality (inherently selective) or an ANALYZE-proven
    // selective range takes the index; everything else falls through to the scan
    // (measured: forcing the index on a 50%-selective DELETE regressed it).
    if let Some(hit) = predicate.as_ref().and_then(|e| {
        find_best_indexable_btree_predicate(e, table_def, ctx.catalog.table_stats(&table_def.name))
    }) {
        if index_lookup_is_selective(table_def, hit, ctx, false) {
            if let Some(rows) = index_matching_rows(heap, snapshot, ctx, table_def, predicate, hit)?
            {
                return Ok(rows);
            }
        }
    }

    // Full-scan fallback — always correct (non-sargable predicate, no index, or
    // a non-orderable literal). B2: evaluate the predicate on just its columns,
    // and only fully decode (UPDATE/DELETE need the whole row) rows that match —
    // so a `DELETE … WHERE k >= lo` no longer materializes every rejected row's
    // `body` `String`.
    let cols = &table_def.columns;
    let mut pred_cols = Vec::new();
    if let Some(pred) = predicate {
        expr_columns(pred, table_def, &mut pred_cols);
    }
    let (pred_needed, pred_upto) = needed_mask(&pred_cols, cols.len());
    let has_pred = predicate.is_some();

    let mut out = Vec::new();
    for (row_id, bytes) in heap.scan(snapshot, ctx.xid, ctx.pool)? {
        if has_pred {
            let prow = deform_row(&bytes, cols, pred_upto, &pred_needed)?;
            if !predicate_matches(predicate, cols, &prow)? {
                continue;
            }
        }
        out.push((row_id, bytes));
    }
    Ok(out)
}

/// Index-driven half of [`matching_rows`] (A3): resolve a sargable B-tree
/// predicate to `(RowId, row)` pairs via the durable B+tree, re-checking every
/// candidate against MVCC visibility and the *full* predicate (so an AND'd
/// non-indexed term still filters). Returns `Ok(None)` when the index cannot
/// serve this predicate (non-orderable literal, `Ne`, or the column has no
/// built index) so the caller falls back to a full scan — never an error, never
/// a wrong result. Candidate RowIds are de-duplicated so a row can never be
/// handed to the caller (and thus updated/deleted) twice.
fn index_matching_rows(
    heap: &Heap,
    snapshot: &crate::mvcc::Snapshot,
    ctx: &mut ExecCtx,
    table_def: &TableDef,
    predicate: &Option<Expr>,
    hit: (&str, CmpOp, &Literal),
) -> Result<Option<MatchedRows>> {
    let (column, op, literal) = hit;
    let Ok(value) = OrderedValue::try_from(literal) else {
        return Ok(None);
    };
    let Some(meta_page) = table_def
        .columns
        .iter()
        .find(|c| c.name == column)
        .and_then(|c| c.index_root)
    else {
        return Ok(None);
    };
    let tree = DiskBTree::new(meta_page, ctx.page_size);
    let mut candidate_ids = match tree.search(op, &value, ctx.pool)? {
        Some(ids) => ids,
        None => return Ok(None),
    };
    // B5: resolve candidates in physical (page, slot) order so `heap.get` walks
    // the heap sequentially-ish instead of randomly — the bitmap-heap-scan idea
    // that softens the A3 random-access cost on a fragmented table (index/key
    // order can scatter across pages after updates). Order doesn't matter for
    // UPDATE/DELETE, which touch every matched row.
    candidate_ids.sort_unstable_by_key(|r| (r.page_id, r.slot));

    // Predicate-column mask for the re-check (mirrors matching_rows's B2 path):
    // non-indexed AND terms (e.g. `k = 5 AND body LIKE 'x'`) only need their own
    // columns materialised; the caller decodes the full row only if it needs it.
    let cols = &table_def.columns;
    let mut pred_cols_list = Vec::new();
    if let Some(pred) = predicate {
        expr_columns(pred, table_def, &mut pred_cols_list);
    }
    let (pred_needed, pred_upto) = needed_mask(&pred_cols_list, cols.len());

    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for row_id in candidate_ids {
        if !seen.insert((row_id.page_id, row_id.slot)) {
            continue; // a rowid resolved once is enough (dedup superseded/dup entries)
        }
        let bytes = match heap.get(row_id, snapshot, ctx.xid, ctx.pool) {
            Ok(b) => b,
            Err(DbError::NoVisibleVersion { .. }) => continue, // superseded / aborted hint
            Err(e) => return Err(e),
        };
        let prow = deform_row(&bytes, cols, pred_upto, &pred_needed)?;
        if predicate_matches(predicate, cols, &prow)? {
            out.push((row_id, bytes));
        }
    }
    Ok(Some(out))
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
            let value = ctx
                .catalog
                .exclusive()?
                .alloc_serial(table, &col.name, &mut cctx)?;
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
                column: None,
                value: None,
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

/// Enforce every UNIQUE/PRIMARY KEY set under `snapshot`.
///
/// For single-column sets whose column has an implicit unique-enforcement
/// B-tree (`ColumnDef.unique_index_root`), a point lookup replaces the
/// former O(n) heap scan — the fix for item 35. MVCC visibility is
/// re-verified for every candidate RowId returned by the index (`get_visible`
/// with the same snapshot + xid), so stale dead-version entries left by an
/// UPDATE until vacuum are correctly filtered out and do not produce false
/// conflicts. Own-xid rows (earlier insertions in the same multi-row INSERT
/// or bulk batch) ARE visible via the `xmin == self_xid` branch of
/// `is_visible`, which is intentional — same-batch duplicates are caught.
///
/// Sets with any NULL component in the new row are skipped (SQL treats NULLs
/// as distinct, so such a row never conflicts). `exclude` is the RowId of the
/// old version being replaced by an UPDATE — it must not count as a conflict
/// with the new version that has the same key value.
///
/// For multi-column sets (composite keys, out of scope for item 35), or for
/// single-column sets whose column type is not BTree-indexable (Decimal,
/// Timestamp, etc.), this function falls back to the O(n) heap scan. That
/// path is expected to be rare in practice.
#[allow(clippy::too_many_arguments)]
fn enforce_unique(
    table_def: &TableDef,
    new_row: &[Literal],
    heap: &Heap,
    snapshot: &Snapshot,
    xid: Xid,
    pool: &BufferPool,
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

    // Split active sets: those we can check via a fast index point lookup vs.
    // those that still need a heap scan (composite sets or non-indexable types).
    let mut heap_scan_sets: Vec<&Vec<usize>> = Vec::new();

    for set in &active {
        if set.len() == 1 {
            let col_idx = set[0];
            let col = &table_def.columns[col_idx];
            if let Some(uiq_meta) = col.unique_index_root {
                // Fast path: point lookup into the implicit unique B-tree.
                // The index may contain stale entries for dead MVCC versions
                // (UPDATE leaves the old key until vacuum), so we re-check
                // visibility for every candidate RowId.
                if let Ok(key) = OrderedValue::try_from(&new_row[col_idx]) {
                    let page_size = pool.page_size();
                    let candidates = DiskBTree::new(uiq_meta, page_size).search_eq(&key, pool)?;
                    for rid in candidates {
                        if Some(rid) == exclude {
                            continue;
                        }
                        if get_visible(pool, rid, snapshot, xid)?.is_some() {
                            return Err(DbError::UniqueViolation {
                                table: table_def.name.clone(),
                                columns: col.name.clone(),
                            });
                        }
                    }
                    continue; // this set is handled — do not add to heap_scan_sets
                }
            }
        }
        // Composite set, or column type not BTree-indexable: fall back to heap.
        heap_scan_sets.push(set);
    }

    // Heap scan for the remaining sets (composite keys, or no implicit index).
    if !heap_scan_sets.is_empty() {
        for (row_id, bytes) in heap.scan(snapshot, xid, pool)? {
            if Some(row_id) == exclude {
                continue;
            }
            let existing = decode_row(&bytes, &table_def.columns)?;
            for set in &heap_scan_sets {
                // `existing[i] == new_row[i]` is false when `existing[i]` is
                // NULL (different `Literal` variant) — existing NULLs never
                // conflict, no special-casing needed.
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
    }

    Ok(())
}

/// The output column names for a projection over `columns`, matching exactly
/// what [`project_row`] emits: `SELECT *` (empty projection) → every non-dropped
/// column in order; an explicit projection → the projected names as written.
pub(crate) fn output_columns(projection: &[String], columns: &[ColumnDef]) -> Vec<String> {
    if projection.is_empty() {
        columns
            .iter()
            .filter(|c| !c.dropped)
            .map(|c| c.name.clone())
            .collect()
    } else {
        projection.to_vec()
    }
}

/// Virtual computed-column name exposed only inside a `NEAR(...)` query's
/// projection (item 41): the Euclidean distance the HNSW/IVF-Flat scan
/// already computed for re-ranking (see [`exec_select_near`]). It is not a
/// real catalog column, so [`project_row`]'s normal column lookup would
/// reject it with `COLUMN_NOT_FOUND` — which is exactly the desired behavior
/// outside a `NEAR` context.
pub(crate) const VEC_DISTANCE_COL: &str = "vec_distance";

/// [`project_row`] plus the `vec_distance` virtual column (item 41): every
/// projected name is resolved as a real column, *except* `vec_distance`,
/// which substitutes `distance` directly. `SELECT *` (empty projection) never
/// includes it — a virtual column only surfaces when explicitly named, same
/// as every other SQL engine's computed-expression convention.
fn project_row_near(
    projection: &[String],
    columns: &[ColumnDef],
    row: &[Literal],
    distance: f32,
) -> Result<Vec<Literal>> {
    if projection.is_empty() {
        return project_row(projection, columns, row);
    }
    projection
        .iter()
        .map(|name| {
            if name == VEC_DISTANCE_COL {
                Ok(Literal::Float(distance as f64))
            } else {
                let idx = columns
                    .iter()
                    .position(|c| &c.name == name && !c.dropped)
                    .ok_or_else(|| DbError::ColumnNotFound {
                        table: String::new(),
                        column: name.clone(),
                    })?;
                Ok(row[idx].clone())
            }
        })
        .collect()
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
        // `Expr::Like` — SQL pattern matching (G9, item 30). NULL on either
        // operand produces NULL (the predicate evaluator treats NULL as false).
        Expr::Like {
            expr,
            pattern,
            negated,
            case_insensitive,
        } => {
            let val = eval_expr(expr, columns, row)?;
            let pat = eval_expr(pattern, columns, row)?;
            match (&val, &pat) {
                (Literal::Null, _) | (_, Literal::Null) => Ok(Literal::Null),
                (Literal::Text(t), Literal::Text(p)) => Ok(Literal::Bool(
                    like_match(t, p, *case_insensitive) != *negated,
                )),
                _ => Err(DbError::SqlUnsupported(format!(
                    "LIKE requires TEXT operands, got {val:?} LIKE {pat:?}"
                ))),
            }
        }
        // `Expr::Match` — same convention as `Expr::Near`: candidates were
        // pre-filtered by `exec_select_match` via the FULLTEXT index, so the
        // per-row re-check returns true and lets other AND'd WHERE terms apply
        // through `predicate_matches` unchanged.
        Expr::Match { .. } => Ok(Literal::Bool(true)),
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
    ROWS_DECODED.fetch_add(1, Ordering::Relaxed); // C1 measurement
                                                  // C1′: a full decode materializes every column (the baseline `cols/row` that
                                                  // B2's `deform_row` drives down by materializing only referenced columns).
    COLS_DECODED.fetch_add(columns.len() as u64, Ordering::Relaxed);
    let mut out = Vec::with_capacity(columns.len());
    let mut pos = 0usize;
    for col in columns {
        // A row written before an `ALTER TABLE ADD COLUMN` (P2.c) has no bytes
        // for the trailing new column(s). Fill each such column with its
        // coerced DEFAULT (so `ADD COLUMN ... DEFAULT x` shows `x` for old
        // rows) or NULL — no heap rewrite needed.
        if pos >= bytes.len() {
            out.push(missing_column_default(col));
            continue;
        }
        out.push(decode_value_at(bytes, &mut pos, col)?);
    }
    Ok(out)
}

/// The value a column takes when its bytes are absent (a row written before an
/// `ALTER TABLE ADD COLUMN`): the coerced DEFAULT, or NULL (P2.c).
fn missing_column_default(col: &ColumnDef) -> Literal {
    match &col.constraints.default {
        Some(default) => coerce_value("", col, default.clone()).unwrap_or(Literal::Null),
        None => Literal::Null,
    }
}

/// **Projection/qual decode pushdown (B2).** Like [`decode_row`], but
/// materializes a `Literal` only for columns where `needed[i]`, and **stops
/// after `upto`** (the highest needed index — mirrors Postgres
/// `heap_deform_tuple`'s `natts` limit: columns after `upto` are never touched,
/// not even their length prefix). Skipped columns (unneeded, or beyond `upto`)
/// hold `Literal::Null`, so the result stays full-width and positional
/// `predicate_matches`/`project_row` are unchanged. The win: the `TEXT`/`Bytea`
/// `String`/`Vec` allocations for unreferenced columns never happen — decode the
/// predicate columns, test, and decode projection columns only on a match.
pub fn deform_row(
    bytes: &[u8],
    columns: &[ColumnDef],
    upto: usize,
    needed: &[bool],
) -> Result<Vec<Literal>> {
    let mut out = vec![Literal::Null; columns.len()];
    let mut pos = 0usize;
    for (i, col) in columns.iter().enumerate() {
        if i > upto {
            break; // nothing at or past here is needed — stop entirely
        }
        if pos >= bytes.len() {
            if needed[i] {
                out[i] = missing_column_default(col);
                COLS_DECODED.fetch_add(1, Ordering::Relaxed);
            }
            continue;
        }
        if needed[i] {
            out[i] = decode_value_at(bytes, &mut pos, col)?;
            COLS_DECODED.fetch_add(1, Ordering::Relaxed);
        } else {
            skip_value_at(bytes, &mut pos)?; // advance past it, no allocation
        }
    }
    Ok(out)
}

/// Collect the column indices a predicate `Expr` references (B2) into `out` —
/// which columns `deform_row` must materialize to evaluate it. Exhaustive over
/// `Expr` (a new variant forces a compile error here, so the set can't silently
/// under-report). An unresolvable column name is skipped — the eventual
/// `predicate_matches` reports it, and decoding a *superset* is always safe.
fn expr_columns(expr: &Expr, table_def: &TableDef, out: &mut Vec<usize>) {
    match expr {
        Expr::Column(name) => {
            if let Ok(idx) = column_index(table_def, name) {
                out.push(idx);
            }
        }
        Expr::Literal(_) => {}
        Expr::BinOp { lhs, rhs, .. } => {
            expr_columns(lhs, table_def, out);
            expr_columns(rhs, table_def, out);
        }
        Expr::And(l, r) => {
            expr_columns(l, table_def, out);
            expr_columns(r, table_def, out);
        }
        Expr::JsonExtract { expr, .. } | Expr::JsonExtractText { expr, .. } => {
            expr_columns(expr, table_def, out);
        }
        Expr::Near { column, .. } | Expr::Match { column, .. } => {
            if let Ok(idx) = column_index(table_def, column) {
                out.push(idx);
            }
        }
        Expr::Like { expr, pattern, .. } => {
            expr_columns(expr, table_def, out);
            expr_columns(pattern, table_def, out);
        }
    }
}

/// Build a `deform_row` `(needed, upto)` pair from a set of column indices.
/// `needed[i]` marks a column to materialize; `upto` is the highest such index
/// (decode stops after it). An empty set → `upto = 0` with an all-false mask
/// (caller should skip the deform entirely).
fn needed_mask(cols: &[usize], ncols: usize) -> (Vec<bool>, usize) {
    let mut needed = vec![false; ncols];
    let mut upto = 0usize;
    for &c in cols {
        if c < ncols {
            needed[c] = true;
            upto = upto.max(c);
        }
    }
    (needed, upto)
}

/// The column indices a projection references. Empty projection = `SELECT *` =
/// every non-dropped column. A name that doesn't resolve widens to *all*
/// columns (safe: decode a superset rather than risk under-decoding).
fn projection_columns(table_def: &TableDef, projection: &[String]) -> Vec<usize> {
    let all: Vec<usize> = (0..table_def.columns.len())
        .filter(|&i| !table_def.columns[i].dropped)
        .collect();
    if projection.is_empty() {
        return all;
    }
    let mut out = Vec::with_capacity(projection.len());
    for name in projection {
        match column_index(table_def, name) {
            Ok(idx) => out.push(idx),
            Err(_) => return all, // unresolved (e.g. an expression) → decode everything
        }
    }
    out
}

/// Advance `*pos` past one encoded column value (the tag byte is at `*pos`)
/// without materializing it — the skip path for [`deform_row`]. Reads the tag +
/// any length prefix only; never allocates. Must stay in lockstep with the tag
/// lengths in `encode_row`/[`decode_value_at`].
fn skip_value_at(bytes: &[u8], pos: &mut usize) -> Result<()> {
    let tag = *bytes
        .get(*pos)
        .ok_or_else(|| DbError::SqlPlan("row decode error: truncated tag".into()))?;
    *pos += 1;
    let read_len = |pos: &mut usize| -> Result<usize> {
        let raw: [u8; 4] = bytes
            .get(*pos..*pos + 4)
            .ok_or_else(|| DbError::SqlPlan("row decode error: truncated length".into()))?
            .try_into()
            .unwrap();
        *pos += 4;
        Ok(u32::from_le_bytes(raw) as usize)
    };
    match tag {
        0 => {}                      // Null — no bytes
        3 => *pos += 1,              // Bool
        11 => *pos += 4,             // Date (i32)
        1 | 7 | 8 | 12 => *pos += 8, // Int / Timestamp / Float / Time
        9 => *pos += 16,             // Uuid
        6 => *pos += 17,             // Decimal (i128 + 1-byte scale)
        2 | 4 | 10 => {
            let len = read_len(pos)?;
            *pos += len;
        } // Text / Json / Bytea
        5 => {
            let dim = read_len(pos)?;
            *pos += dim * 4;
        } // Vector (dim * f32)
        other => {
            return Err(DbError::SqlPlan(format!(
                "row decode error: unknown tag {other}"
            )))
        }
    }
    Ok(())
}

/// Decode one column value (tag byte at `*pos`), advancing `*pos` past it.
/// `col` supplies type context for `Vector`/`Decimal` validation. Shared by
/// [`decode_row`] (full) and [`deform_row`] (selective).
fn decode_value_at(bytes: &[u8], pos_ref: &mut usize, col: &ColumnDef) -> Result<Literal> {
    let mut pos = *pos_ref;
    let tag = *bytes
        .get(pos)
        .ok_or_else(|| DbError::SqlPlan("row decode error: truncated tag".into()))?;
    pos += 1;
    let lit = {
        match tag {
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
        }
    };
    *pos_ref = pos;
    Ok(lit)
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
        control: Mutex<ControlData>,
        control_path: std::path::PathBuf,
        page_size: usize,
        next_event_seq: AtomicU64,
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
                control: Mutex::new(control),
                control_path,
                page_size: DEFAULT_PAGE_SIZE as usize,
                next_event_seq: AtomicU64::new(1),
            }
        }

        fn exec_as(&mut self, xid: Xid, sql: &str) -> Result<ExecResult> {
            let mut plans = parse_sql(sql)?;
            assert_eq!(plans.len(), 1, "expected exactly one statement");
            let plan = plans.remove(0);
            let mut ctx = ExecCtx {
                catalog: CatalogHandle::Exclusive(&mut self.catalog),
                txn_mgr: &self.txn_mgr,
                pool: &self.pool,
                wal: &self.wal,
                lock_mgr: &self.lock_mgr,
                control_path: &self.control_path,
                control: &self.control,
                page_size: self.page_size,
                xid,
                next_event_seq: &self.next_event_seq,
                event_seq_index_meta: None,
            };
            execute(plan, &mut ctx)
        }

        fn begin(&mut self) -> Xid {
            self.txn_mgr
                .begin(IsolationLevel::ReadCommitted, &self.wal)
                .unwrap()
        }

        fn commit(&mut self, xid: Xid) {
            self.txn_mgr.commit(xid, &self.wal, &self.lock_mgr).unwrap();
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
            ExecResult::Rows { rows, .. } => {
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
            ExecResult::Rows { rows, .. } => {
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
        assert_eq!(
            result,
            ExecResult::Rows {
                columns: vec!["balance".to_string()],
                rows: vec![vec![Literal::Int(50)]]
            }
        );
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
        assert!(matches!(result, ExecResult::Rows { rows, .. } if rows.is_empty()));
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
            ExecResult::Rows { rows, .. } => assert_eq!(rows.len(), 1),
            other => panic!("expected Rows, got {other:?}"),
        }

        let none = h
            .exec_as(
                xid2,
                "SELECT * FROM t WHERE (data ->> 'status') = 'inactive'",
            )
            .unwrap();
        assert!(matches!(none, ExecResult::Rows { rows, .. } if rows.is_empty()));
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
    fn table_survives_reopen_via_durable_fsm() {
        let dir = tempdir().unwrap();
        let (root_page, fsm_meta, legacy_pages);
        {
            let mut h = Harness::new(dir.path());
            let xid = h.begin();
            h.exec_as(xid, "CREATE TABLE t (id INT)").unwrap();
            h.exec_as(xid, "INSERT INTO t (id) VALUES (42)").unwrap();
            h.commit(xid);
            h.pool.flush_all(h.wal.durable_lsn()).unwrap();
            root_page = h.control.lock().unwrap().catalog_root;
            let def = h.catalog.lookup("t").unwrap();
            fsm_meta = def.fsm_meta;
            legacy_pages = def.pages.clone();
        }
        assert_ne!(root_page, crate::format::INVALID_PAGE_ID);
        // The durable FSM (not the old catalog `pages` blob) now holds the page
        // directory — that is what carries the table's pages across a reopen.
        assert!(
            fsm_meta.is_some(),
            "table must have minted a durable free-space map"
        );
        assert!(
            legacy_pages.is_empty(),
            "FSM-backed table must not grow the catalog page-list blob \
             (that O(pages) blob rewrite was the HeapFull ceiling)"
        );

        // Reopen: reconstruct catalog + pool from what was persisted.
        let pool =
            BufferPool::open(&dir.path().join("data.db"), DEFAULT_PAGE_SIZE as usize, 64).unwrap();
        let control = crate::control::read(&dir.path().join("control")).unwrap();
        let catalog = Catalog::load(&control, &pool).unwrap();
        let table_def = catalog.lookup("t").unwrap();
        let heap = Heap::open(
            DEFAULT_PAGE_SIZE as usize,
            table_def.fsm_meta,
            table_def.pages.clone(),
        );
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
                unique_index_root: None,
                dropped: false,
                constraints: Default::default(),
                ty: ColumnType::Int64,
            },
            ColumnDef {
                name: "b".to_string(),
                index: None,
                index_root: None,
                unique_index_root: None,
                dropped: false,
                constraints: Default::default(),
                ty: ColumnType::Text,
            },
            ColumnDef {
                name: "c".to_string(),
                index: None,
                index_root: None,
                unique_index_root: None,
                dropped: false,
                constraints: Default::default(),
                ty: ColumnType::Bool,
            },
            ColumnDef {
                name: "d".to_string(),
                index: None,
                index_root: None,
                unique_index_root: None,
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
            unique_index_root: None,
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
            unique_index_root: None,
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
            unique_index_root: None,
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
                unique_index_root: None,
                dropped: false,
                constraints: Default::default(),
                ty: ColumnType::Vector(4),
            }],
            pages: vec![],
            fsm_meta: None,
            rls_policy: None,
            events_enabled: false,
            serial_next: Default::default(),
            constraints: Default::default(),
            generation: 0,
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
            unique_index_root: None,
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
            ExecResult::Rows { rows: r, .. } => r,
            other => panic!("expected Rows, got {other:?}"),
        };
        assert_eq!(rows, vec![vec![Literal::Decimal(950, 2)]]);
        let rows2 = match h.exec_as(xid2, "SELECT price FROM t WHERE id = 2").unwrap() {
            ExecResult::Rows { rows: r, .. } => r,
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
            ExecResult::Rows { rows: r, .. } => r,
            other => panic!("expected Rows, got {other:?}"),
        };
        assert_eq!(rows.len(), 3); // 9.99, 10.00, 10.50
                                   // Equality against an integer literal (scale 0 vs stored scale 2).
        let eq = match h
            .exec_as(xid2, "SELECT id FROM t WHERE price = 10")
            .unwrap()
        {
            ExecResult::Rows { rows: r, .. } => r,
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
            ExecResult::Rows { rows: r, .. } => r,
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
            ExecResult::Rows { rows: r, .. } => r,
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
            h.pool.flush_all(h.wal.durable_lsn()).unwrap();
        }

        let pool =
            BufferPool::open(&dir.path().join("data.db"), DEFAULT_PAGE_SIZE as usize, 64).unwrap();
        let control = crate::control::read(&dir.path().join("control")).unwrap();
        let catalog = Catalog::load(&control, &pool).unwrap();
        let table_def = catalog.lookup("t").unwrap();
        assert_eq!(table_def.columns[0].ty, ColumnType::Decimal(10, 2));
        assert_eq!(table_def.columns[1].ty, ColumnType::Timestamp);
        let heap = Heap::open(
            DEFAULT_PAGE_SIZE as usize,
            table_def.fsm_meta,
            table_def.pages.clone(),
        );
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
            ExecResult::Rows { rows: r, .. } => r,
            o => panic!("{o:?}"),
        };
        assert_eq!(rows, vec![vec![Literal::Float(1.5)]]);
        let gt = match h.exec_as(xid2, "SELECT id FROM t WHERE x > 2").unwrap() {
            ExecResult::Rows { rows: r, .. } => r,
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
            ExecResult::Rows { rows: r, .. } => r,
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
            ExecResult::Rows { rows: r, .. } => r,
            o => panic!("{o:?}"),
        };
        assert_eq!(r1, vec![vec![Literal::Bytea(vec![0xde, 0xad, 0xbe, 0xef])]]);
        let r2 = match h.exec_as(xid2, "SELECT b FROM t WHERE id = 2").unwrap() {
            ExecResult::Rows { rows: r, .. } => r,
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
            ExecResult::Rows { rows: r, .. } => r,
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
            ExecResult::Rows { rows: r, .. } => r,
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
            ExecResult::Rows { rows: r, .. } => r,
            o => panic!("{o:?}"),
        };
        assert_eq!(old, vec![vec![Literal::Text("new".to_string())]]);
        let new = match h
            .exec_as(xid2, "SELECT status FROM t WHERE id = 2")
            .unwrap()
        {
            ExecResult::Rows { rows: r, .. } => r,
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
            ExecResult::Rows { rows: r, .. } => r,
            o => panic!("{o:?}"),
        };
        assert_eq!(rows, vec![vec![Literal::Int(1), Literal::Int(3)]]);
        // SELECT * returns only the two visible columns.
        let star = match h.exec_as(xid2, "SELECT * FROM t").unwrap() {
            ExecResult::Rows { rows: r, .. } => r,
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
            ExecResult::Rows { rows: r, .. } => r,
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
        assert!(matches!(
            h.exec_as(xid2, "SELECT * FROM t").unwrap(),
            ExecResult::Rows { rows, .. } if rows.is_empty()
        ));
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
        assert!(matches!(
            h.exec_as(xid3, "SELECT * FROM t").unwrap(),
            ExecResult::Rows { rows, .. } if rows.is_empty()
        ));
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

    // ── A3: index-driven UPDATE/DELETE (crud-perf Phase A) ───────────────────

    /// An equality UPDATE on a BTree-indexed column goes through the A3
    /// index-driven `matching_rows` path (`index_lookup_is_selective` takes Eq)
    /// and must update exactly the matching rows, leaving the row resolvable via
    /// the index at its new key and the untouched rows unchanged.
    #[test]
    fn a3_equality_update_via_index_is_correct() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE t (id INT, k INT)").unwrap();
        h.exec_as(xid, "CREATE INDEX t_k ON t USING BTREE (k)")
            .unwrap();
        for i in 0..50 {
            h.exec_as(xid, &format!("INSERT INTO t (id, k) VALUES ({i}, {i})"))
                .unwrap();
        }
        h.commit(xid);

        // Equality UPDATE of the indexed column itself (also exercises A1's
        // coalesced index write for the changed key).
        let xid = h.begin();
        let n = match h.exec_as(xid, "UPDATE t SET k = 999 WHERE k = 25").unwrap() {
            ExecResult::Updated { count } => count,
            o => panic!("{o:?}"),
        };
        h.commit(xid);
        assert_eq!(n, 1, "exactly one row matches k = 25");

        let ids_at = |h: &mut Harness, k: i64| -> Vec<i64> {
            let xid = h.begin();
            let r = h
                .exec_as(xid, &format!("SELECT id FROM t WHERE k = {k}"))
                .unwrap();
            h.commit(xid);
            match r {
                ExecResult::Rows { rows, .. } => rows
                    .iter()
                    .filter_map(|row| match row[0] {
                        Literal::Int(v) => Some(v),
                        _ => None,
                    })
                    .collect(),
                o => panic!("{o:?}"),
            }
        };
        assert!(ids_at(&mut h, 25).is_empty(), "old key must be gone");
        assert_eq!(ids_at(&mut h, 999), vec![25], "row resolves at its new key");
        assert_eq!(ids_at(&mut h, 24), vec![24], "neighbor row untouched");
    }

    /// An equality DELETE on a BTree-indexed column goes through the A3 index
    /// path and removes exactly the matching row; the rest survive and stay
    /// index-resolvable.
    #[test]
    fn a3_equality_delete_via_index_is_correct() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE t (id INT, k INT)").unwrap();
        h.exec_as(xid, "CREATE INDEX t_k ON t USING BTREE (k)")
            .unwrap();
        for i in 0..50 {
            h.exec_as(xid, &format!("INSERT INTO t (id, k) VALUES ({i}, {i})"))
                .unwrap();
        }
        h.commit(xid);

        let xid = h.begin();
        let n = match h.exec_as(xid, "DELETE FROM t WHERE k = 30").unwrap() {
            ExecResult::Deleted { count } => count,
            o => panic!("{o:?}"),
        };
        h.commit(xid);
        assert_eq!(n, 1);

        let xid = h.begin();
        let gone = h.exec_as(xid, "SELECT id FROM t WHERE k = 30").unwrap();
        let survivor = h.exec_as(xid, "SELECT id FROM t WHERE k = 31").unwrap();
        h.commit(xid);
        assert!(
            matches!(gone, ExecResult::Rows { ref rows, .. } if rows.is_empty()),
            "deleted row must not resolve via the index"
        );
        assert!(
            matches!(survivor, ExecResult::Rows { ref rows, .. } if rows.len() == 1),
            "neighbor survives and stays index-resolvable"
        );
    }

    // ── B2: projection / qual decode pushdown (crud-perf Phase B) ────────────

    /// `deform_row` (selective decode) must produce results byte-identical to a
    /// full decode across projection subsets, predicate-only columns, and
    /// `SELECT *` — even when the projected/predicate columns are not the ones a
    /// row's large `TEXT` value sits in.
    #[test]
    fn b2_projection_pushdown_matches_full_decode() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE t (id INT, k INT, g INT, body TEXT)")
            .unwrap();
        for i in 0..20 {
            h.exec_as(
                xid,
                &format!(
                    "INSERT INTO t (id, k, g, body) VALUES ({i}, {i}, {}, 'body-value-{i}')",
                    i % 3
                ),
            )
            .unwrap();
        }
        h.commit(xid);

        let rows = |h: &mut Harness, sql: &str| -> Vec<Vec<Literal>> {
            let xid = h.begin();
            let r = h.exec_as(xid, sql).unwrap();
            h.commit(xid);
            match r {
                ExecResult::Rows { rows, .. } => rows,
                o => panic!("{o:?}"),
            }
        };

        // Projection subset, predicate on a *different* column (body never
        // projected → the deform must skip it, but the filter on k is exact).
        let got = rows(&mut h, "SELECT id FROM t WHERE k >= 5 AND k < 10");
        let want: Vec<Vec<Literal>> = (5..10).map(|i| vec![Literal::Int(i)]).collect();
        assert_eq!(got, want, "projection subset + qual on other column");

        // Projection that *does* include the TEXT column, predicate on k.
        let got = rows(&mut h, "SELECT id, body FROM t WHERE k = 7");
        assert_eq!(
            got,
            vec![vec![Literal::Int(7), Literal::Text("body-value-7".into())]],
            "projected TEXT materialized for the matching row"
        );

        // SELECT * (empty projection → all columns) must be unchanged.
        let got = rows(&mut h, "SELECT * FROM t WHERE k = 3");
        assert_eq!(
            got,
            vec![vec![
                Literal::Int(3),
                Literal::Int(3),
                Literal::Int(0),
                Literal::Text("body-value-3".into())
            ]],
            "SELECT * still returns every column"
        );

        // Predicate that references only the TEXT column.
        let got = rows(&mut h, "SELECT id FROM t WHERE body = 'body-value-12'");
        assert_eq!(got, vec![vec![Literal::Int(12)]], "qual on TEXT column");
    }

    /// B1: `SELECT COUNT(*)` (the `count_visible` fast path, no decode) must
    /// agree with a decode-based count across inserts, an UPDATE (new version +
    /// superseded old must count once), and a DELETE (removed row uncounted);
    /// and a filtered/`GROUP BY` count must still be correct (falls back).
    #[test]
    fn b1_count_star_matches_mvcc_visibility() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE t (id INT, k INT)").unwrap();
        for i in 0..10 {
            h.exec_as(xid, &format!("INSERT INTO t (id, k) VALUES ({i}, {i})"))
                .unwrap();
        }
        h.commit(xid);

        let count = |h: &mut Harness, sql: &str| -> i64 {
            let xid = h.begin();
            let r = h.exec_as(xid, sql).unwrap();
            h.commit(xid);
            match r {
                ExecResult::Rows { rows, .. } => match rows[0][0] {
                    Literal::Int(n) => n,
                    ref o => panic!("{o:?}"),
                },
                o => panic!("{o:?}"),
            }
        };

        assert_eq!(count(&mut h, "SELECT COUNT(*) FROM t"), 10);

        // UPDATE creates a new version + supersedes the old — still one logical
        // row, so the count is unchanged.
        let xid = h.begin();
        h.exec_as(xid, "UPDATE t SET k = 100 WHERE id = 3").unwrap();
        h.commit(xid);
        assert_eq!(count(&mut h, "SELECT COUNT(*) FROM t"), 10);

        // DELETE removes one logical row.
        let xid = h.begin();
        h.exec_as(xid, "DELETE FROM t WHERE id = 7").unwrap();
        h.commit(xid);
        assert_eq!(count(&mut h, "SELECT COUNT(*) FROM t"), 9);

        // Filtered / grouped counts (normal path) still correct.
        assert_eq!(count(&mut h, "SELECT COUNT(*) FROM t WHERE k < 5"), 4);
        let xid = h.begin();
        let r = h
            .exec_as(xid, "SELECT COUNT(*) FROM t GROUP BY id")
            .unwrap();
        h.commit(xid);
        match r {
            ExecResult::Rows { rows, .. } => assert_eq!(rows.len(), 9),
            o => panic!("{o:?}"),
        }
    }

    // ── Item 46: GROUP BY decode pushdown ────────────────────────────────────

    /// Item 46: COUNT(*) GROUP BY single column should decode only that column,
    /// not all columns. On a 4-column table scanning N rows, cols/row must be
    /// 1.00 (only `g` decoded), not 4.00 (all columns decoded via decode_row).
    #[test]
    fn item46_group_by_decodes_only_group_key_columns() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        // 4 columns: id, k, g (group key), body (TEXT — the expensive column)
        h.exec_as(xid, "CREATE TABLE t (id INT, k INT, g INT, body TEXT)")
            .unwrap();
        let n = 100u64;
        for i in 0..n {
            h.exec_as(
                xid,
                &format!(
                    "INSERT INTO t (id, k, g, body) VALUES ({i}, {i}, {}, 'body-{i}')",
                    i % 5
                ),
            )
            .unwrap();
        }
        h.commit(xid);

        // Snapshot the global counter before the GROUP BY query.
        let cols_before = COLS_DECODED.load(std::sync::atomic::Ordering::Relaxed);

        let xid2 = h.begin();
        let r = h
            .exec_as(xid2, "SELECT g, COUNT(*) FROM t GROUP BY g")
            .unwrap();
        h.commit(xid2);

        let cols_after = COLS_DECODED.load(std::sync::atomic::Ordering::Relaxed);
        let cols_used = (cols_after - cols_before) as f64;
        let cols_per_row = cols_used / n as f64;

        // Verify correctness: 5 groups (g=0..4), 20 rows each.
        match r {
            ExecResult::Rows { rows, .. } => assert_eq!(rows.len(), 5, "5 distinct g values"),
            o => panic!("unexpected result: {o:?}"),
        }

        // Verify decode efficiency: item 46 fast path decodes only g (1 col/row).
        // Without the fast path this would be 4.00 (all columns via decode_row).
        assert!(
            cols_per_row < 1.5,
            "cols/row = {cols_per_row:.2} — expected ≈1.00 (only g decoded); \
             got {cols_used} total cols for {n} rows. \
             The item 46 GROUP BY decode-pushdown fast path may not have fired."
        );
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
            ExecResult::Rows { rows: r, .. } => r,
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
            ExecResult::Rows { rows: r, .. } => r,
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
            ExecResult::Rows { rows, .. } => {
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
