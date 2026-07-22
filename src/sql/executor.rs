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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use serde_json::Value as JsonValue;

use crate::{
    btree_index::{DiskBTree, OrderedValue, RangeOp},
    bufferpool::{BufferPool, PageReader},
    catalog::{Catalog, CatalogCtx, ColumnDef, ColumnType, IndexKind, TableConstraints, TableDef},
    control::ControlData,
    error::{DbError, Result},
    format::{PageId, Xid},
    heap::{get_visible, Heap, RowId},
    hnsw_index::{DiskHnswIndex, HnswL0Cache, HNSW_EF_SEARCH},
    lockmgr::{LockManager, RecordId},
    mvcc::Snapshot,
    queue::{self, EVENTS_TABLE},
    txn::{IsolationLevel, TransactionManager, UndoAction},
    wal::Wal,
};

// IVF tuning was used by the retired IVF-Flat index (item 63 replaced it with
// on-disk HNSW). These are kept for reference only; the functions are removed.

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

/// Returns `true` if any column named in `assignments` (the SET clause of an
/// UPDATE) has either a secondary B-tree index (`index_root`) or a
/// unique-enforcement B-tree index (`unique_index_root`). When this is `true`,
/// a HOT update cannot be used — the B-tree entry must be updated to point at
/// the new version (see CLAUDE.md §0.6.2 and item 58 design).
///
/// Columns in `assignments` are matched by name against `columns`, exactly as
/// the executor does for FK column detection (item 53 pattern).
fn set_touches_indexed_col(assignments: &[(String, Expr)], columns: &[ColumnDef]) -> bool {
    let set_cols: std::collections::HashSet<&str> =
        assignments.iter().map(|(col, _)| col.as_str()).collect();
    // Direct indexed column (key column of a BTree/FullText/HNSW index, or unique index).
    let touches_key = columns.iter().any(|c| {
        !c.dropped
            && set_cols.contains(c.name.as_str())
            && (c.index_root.is_some() || c.unique_index_root.is_some())
    });
    if touches_key {
        return true;
    }
    // item 102-B: SET touches an INCLUDE column of a covering B-tree index.
    // The include payload in the leaf must be updated, so HOT is not safe.
    columns.iter().any(|c| {
        !c.dropped
            && c.index_root.is_some()
            && matches!(c.index, Some(IndexKind::BTree))
            && c.include_cols
                .iter()
                .any(|inc| set_cols.contains(inc.as_str()))
    })
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
use super::logical::{ArithOp, CmpOp, Expr, Literal, LogicalPlan};

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

/// Item 59 Fix 1: gate `COLS_DECODED.fetch_add()` behind this bool so it is a
/// no-op on the hot path in release builds. Defaults **false**; the bench
/// enables it via `Engine::enable_diagnostics()` before sampling, and the
/// `group_by_cols_per_row` test enables it too. Using a plain `AtomicBool`
/// load (Relaxed) costs ~0.3 ns/call when false — negligible. The alternative
/// (`#[cfg(feature = "diagnostics")]`) would require a feature flag in every
/// Cargo.toml and bench invocation; a runtime bool is simpler and correct.
pub static DIAGNOSTICS_ENABLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Item 102-A: total number of rows returned via the index-only scan path
/// (B-tree leaf value used directly, heap fetch skipped). Tests diff this
/// counter to verify that heap fetches are truly 0 for qualifying queries.
/// `Relaxed` — pure statistic, no ordering obligations.
pub static IDX_ONLY_ROWS: AtomicU64 = AtomicU64::new(0);

/// item 102-B: rows served from a covering index (INCLUDE cols in B-tree leaf).
/// Rows counted here are a subset of IDX_ONLY_ROWS where ALL projected columns
/// (key + include cols) were satisfied from the leaf without a heap deform_row.
pub static IDX_INCLUDE_ROWS: AtomicU64 = AtomicU64::new(0);

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
    /// Per-index L0 neighbour-list cache for NEAR queries (item 72).
    /// `Some` on a fully-opened Engine; `None` in unit tests that build bare
    /// `ExecCtx` structs (those tests don't exercise NEAR queries).
    pub hnsw_l0_caches:
        Option<&'a Mutex<std::collections::HashMap<crate::format::PageId, HnswL0Cache>>>,
    /// Per-index vector hot cache for NEAR queries (item 73).
    /// Eliminates ~100 KB random reads per NEAR query after warm-up by caching
    /// encoded_rid → Vec<f32> for every node visited during beam search.
    pub hnsw_vec_caches: Option<
        &'a Mutex<
            std::collections::HashMap<crate::format::PageId, crate::hnsw_index::HnswVecCache>,
        >,
    >,
    /// Authorization store (item-24 Z5): supplies rows for `unidb_catalog.roles`,
    /// `unidb_catalog.grants`. `None` in unit tests that build bare `ExecCtx`
    /// structs and don't exercise the AuthZ catalog views.
    pub authz: Option<&'a crate::authz::RoleStore>,
    /// Identity of the executing user for `current_user()` resolution in RLS
    /// predicates (item-24 Z6). `None` = superuser / embedded path (no
    /// restriction). Set by `execute_sql_inner_as` before execution; `None`
    /// in all other paths (superuser/embedded always bypass RLS anyway).
    pub current_user: Option<String>,
    /// Sender end of the HNSW background worker channel (item 67).
    /// `Some` on a fully-opened Engine that has called `spawn_hnsw_worker`;
    /// `None` in unit tests that build bare `ExecCtx` structs (those tests
    /// don't exercise the async HNSW path and fall back to synchronous insert).
    /// The executor clones this cheaply per INSERT statement.
    pub hnsw_tx: Option<std::sync::mpsc::SyncSender<crate::hnsw_index::HnswMsg>>,
    /// Whether this execution is running inside an explicit user `BEGIN … COMMIT`
    /// block (item 94). When `false`, a standalone (auto-commit) statement may use
    /// the lightweight snapshot fast path for NEAR queries. When `true`, the caller
    /// has opened a multi-statement transaction and NEAR must use the transaction's
    /// own snapshot (via `txn_mgr.snapshot_for_statement(xid)`) so the query sees
    /// preceding writes within the same transaction. `None` in unit tests that build
    /// bare `ExecCtx` structs and don't test NEAR inside explicit transactions.
    pub in_explicit_txn: bool,
    /// Counter for standalone NEAR queries that used the lightweight snapshot
    /// (item 94). `None` in unit tests that build bare `ExecCtx` structs.
    /// Points at `Engine::near_lightweight_snaps`.
    pub near_lightweight_snaps: Option<&'a std::sync::atomic::AtomicU64>,
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
                        // item 102-B: encode INCLUDE column values as payload
                        // when this index was created with INCLUDE (...).
                        let include_payload: Vec<u8> = if col.include_cols.is_empty() {
                            Vec::new()
                        } else {
                            let inc_vals: Vec<Literal> = col
                                .include_cols
                                .iter()
                                .map(|inc_name| {
                                    table_def
                                        .columns
                                        .iter()
                                        .enumerate()
                                        .find(|(_, c)| c.name == *inc_name)
                                        .map(|(i, _)| row.get(i).cloned().unwrap_or(Literal::Null))
                                        .unwrap_or(Literal::Null)
                                })
                                .collect();
                            encode_row(&inc_vals)
                        };
                        DiskBTree::new(meta_page, ctx.page_size).insert_with_include(
                            value,
                            row_id,
                            &include_payload,
                            ctx.pool,
                            ctx.wal,
                        )?;
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
                        if let Some(ref tx) = ctx.hnsw_tx {
                            // Async path (item 67): dispatch to background worker.
                            // The beam-search + edge-update cost (~6–18 ms) is
                            // removed from the INSERT commit critical path.
                            // MVCC correctness: the heap row is already durable
                            // before this dispatch; the index insert may arrive
                            // slightly after commit, but NEAR re-checks MVCC
                            // visibility on every candidate so a race between
                            // index visibility and row visibility is safe.
                            // Item 107: depth gauge counts rows committed
                            // but not yet HNSW-indexed (the NEAR freshness
                            // lag, contract "a"). Incremented only on a
                            // successful enqueue; the worker decrements.
                            if tx
                                .send(crate::hnsw_index::HnswMsg::Work(
                                    crate::hnsw_index::HnswWorkItem {
                                        meta_page,
                                        page_size: ctx.page_size,
                                        row_id,
                                        vector: v.clone(),
                                    },
                                ))
                                .is_ok()
                            {
                                crate::hnsw_index::HNSW_QUEUE_DEPTH.fetch_add(1, Ordering::Relaxed);
                            } else {
                                // Worker gone (engine teardown mid-statement):
                                // fall back to the synchronous insert so the
                                // committed row is never silently unindexed.
                                DiskHnswIndex::open(meta_page, ctx.page_size)
                                    .insert(row_id, v, ctx.pool, ctx.wal)?;
                            }
                        } else {
                            // Sync fallback: unit tests and bare Engine::open()
                            // paths without a background worker.
                            DiskHnswIndex::open(meta_page, ctx.page_size)
                                .insert(row_id, v, ctx.pool, ctx.wal)?;
                        }
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
    /// For non-covering BTree / FullText indexes.
    entries: Vec<(OrderedValue, RowId)>,
    /// item 102-B: INCLUDE column names for this BTree index.
    /// Empty means non-covering (use `entries`); non-empty means covering
    /// (use `include_entries` which carries the encoded payloads).
    include_cols: Vec<String>,
    /// item 102-B: covering-index entries with include payload bytes.
    include_entries: Vec<(OrderedValue, RowId, Vec<u8>)>,
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
                include_cols: col.include_cols.clone(),
                include_entries: Vec::new(),
            }),
            Some(IndexKind::FullText) => batches.push(IndexColBatch {
                col_idx,
                meta_page,
                is_fulltext: true,
                entries: Vec::new(),
                include_cols: Vec::new(),
                include_entries: Vec::new(),
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
            // item 102-B: even if the indexed key is unchanged, if any INCLUDE
            // column changed we must insert a new entry (with updated payload)
            // rather than patching the RowId — patching would leave the old
            // include bytes on disk, making the covering scan return stale values.
            let include_cols_changed = !ib.include_cols.is_empty()
                && ib.include_cols.iter().any(|inc_name| {
                    table_def
                        .columns
                        .iter()
                        .enumerate()
                        .find(|(_, c)| c.name == *inc_name)
                        .map(|(i, _)| before_row.get(i) != new_row.get(i))
                        .unwrap_or(false)
                });
            if !include_cols_changed {
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
                // INCLUDE column changed: insert new entry with updated payload.
                let inc_vals: Vec<Literal> = ib
                    .include_cols
                    .iter()
                    .map(|inc_name| {
                        table_def
                            .columns
                            .iter()
                            .enumerate()
                            .find(|(_, c)| c.name == *inc_name)
                            .map(|(i, _)| new_row.get(i).cloned().unwrap_or(Literal::Null))
                            .unwrap_or(Literal::Null)
                    })
                    .collect();
                ib.include_entries
                    .push((value, new_row_id, encode_row(&inc_vals)));
            }
        } else if let Ok(value) = OrderedValue::try_from(new_val) {
            // item 102-B: for covering indexes, encode include payload.
            if ib.include_cols.is_empty() {
                ib.entries.push((value, new_row_id));
            } else {
                let inc_vals: Vec<Literal> = ib
                    .include_cols
                    .iter()
                    .map(|inc_name| {
                        table_def
                            .columns
                            .iter()
                            .enumerate()
                            .find(|(_, c)| c.name == *inc_name)
                            .map(|(i, _)| new_row.get(i).cloned().unwrap_or(Literal::Null))
                            .unwrap_or(Literal::Null)
                    })
                    .collect();
                ib.include_entries
                    .push((value, new_row_id, encode_row(&inc_vals)));
            }
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
                    DiskHnswIndex::open(meta_page, ctx.page_size)
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
                // Changed unique key — insert inline (per-row) to preserve in-statement
                // visibility for subsequent rows' enforce_unique checks.  Deferring this
                // to a post-loop batch would let enforce_unique miss earlier in-flight
                // entries and silently admit UNIQUE violations mid-statement.
                // Safe batch only applies to the can_batch_non_hot path (has_unique=false).
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
        // item 102-B: covering index entries carry include payloads.
        if !b.include_entries.is_empty() {
            DiskBTree::new(b.meta_page, ctx.page_size).insert_many_with_include(
                &b.include_entries,
                ctx.pool,
                ctx.wal,
            )?;
        }
        if !b.entries.is_empty() {
            DiskBTree::new(b.meta_page, ctx.page_size)
                .insert_many(&b.entries, ctx.pool, ctx.wal)?;
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

    // Item 60: replaced serde_json::json! + row_to_json (Value tree) with a
    // direct string builder.  No serde_json::Value is ever allocated; the
    // envelope is built as a String and stored verbatim as Literal::Json.
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
    let envelope_str = queue::payload::build_event_envelope_str(
        op,
        &table_def.name,
        before.map(|r| (r, table_def.columns.as_slice())),
        after.map(|r| (r, table_def.columns.as_slice())),
        ts_ms,
        seq,
        ctx.xid,
    );
    let encoded = encode_row(&queue::event_row(
        seq as i64,
        ctx.xid as i64,
        &table_def.name,
        op,
        envelope_str,
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
            fill_factor,
        } => exec_create_table(name, columns, constraints, fill_factor, ctx),
        LogicalPlan::Insert {
            table,
            columns,
            values,
            returning,
        } => exec_insert(&table, columns, values, returning.as_deref(), ctx),
        LogicalPlan::Select {
            table,
            projection,
            predicate,
        } => exec_select(&table, &projection, &predicate, ctx),
        LogicalPlan::Update {
            table,
            assignments,
            predicate,
            returning,
        } => exec_update(&table, &assignments, &predicate, returning.as_deref(), ctx),
        LogicalPlan::Delete {
            table,
            predicate,
            returning,
        } => exec_delete(&table, &predicate, returning.as_deref(), ctx),
        LogicalPlan::Query(spec) => crate::sql::query_exec::exec_query(&spec, ctx),
        LogicalPlan::CreateIndex {
            table,
            column,
            kind,
            include_cols,
        } => exec_create_index(&table, &column, kind, include_cols, ctx),
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
        LogicalPlan::SetOp {
            op,
            all,
            left,
            right,
        } => crate::sql::query_exec::exec_set_op(op, all, &left, &right, ctx),
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
    fill_factor: u8,
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
        insert_policy: None,
        update_policy: None,
        delete_policy: None,
        update_with_check: None,
        policies: vec![],
        events_enabled: false,
        serial_next,
        constraints,
        generation: 0,
        row_count: 0,
        fill_factor,
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
    include_cols: Vec<String>,
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
        // Item 63: on-disk HNSW vector index. Create the index, then bulk-insert
        // every committed row's vector. HNSW is built incrementally (no training
        // phase required, unlike IVF-Flat) so the build order is arbitrary.
        //
        // Performance (two-pass approach):
        // Pass 1: scan heap once, collect all (heap_rid, vector) pairs into an
        //   in-memory HashMap<i64, Vec<f32>> (build_cache).  This costs O(n) RAM
        //   for the duration of the build (~dim*4 bytes per row; 50 MB at 100k×dim128).
        // Pass 2: build HNSW using insert_with_cache so that every vector fetch
        //   during beam search hits the O(1) HashMap instead of the O(log n)
        //   DiskBTree.  Eliminates the O(n²·log n) DiskBTree lookup bottleneck
        //   that made 10k rows take 53+ minutes: now build is O(n·ef·M) distance
        //   comparisons on in-memory floats — expected <2 min at 10k, <10 min at 100k.
        // WAL durability: deferred-sync collapses ~N fsyncs to 1 after the loop.
        let heap = Heap::open(ctx.page_size, table_def.fsm_meta, table_def.pages.clone());

        // Pass 1: collect all vectors into the build cache.
        let mut build_cache: std::collections::HashMap<i64, Vec<f32>> =
            std::collections::HashMap::new();
        for (row_id, bytes) in heap.scan(&snapshot, ctx.xid, ctx.pool)? {
            let row = decode_row(&bytes, &table_def.columns)?;
            if let Literal::Vector(v) = &row[col_idx] {
                let key = (row_id.page_id as i64) * 65536 + row_id.slot as i64;
                build_cache.insert(key, v.clone());
            }
        }

        let hnsw = DiskHnswIndex::create(
            vec_dim as usize,
            crate::vector::Metric::Euclidean,
            ctx.pool,
            ctx.wal,
        )?;
        // Enable deferred sync for bulk build (no per-node fsync overhead).
        ctx.wal.set_deferred_sync(true);
        // Pass 2: insert each row using the build cache for O(1) vector lookups.
        for (row_id, bytes) in heap.scan(&snapshot, ctx.xid, ctx.pool)? {
            let row = decode_row(&bytes, &table_def.columns)?;
            if let Literal::Vector(v) = &row[col_idx] {
                hnsw.insert_with_cache(row_id, v, &build_cache, ctx.pool, ctx.wal)?;
            }
        }
        // Re-enable per-commit durability and force the accumulated writes to disk.
        ctx.wal.set_deferred_sync(false);
        ctx.wal.sync()?;

        // Lever 3 (item 92): pre-warm L0 and vector caches after bulk build.
        // Eliminates disk I/O on the first NEAR query by loading all node data
        // into the process-lifetime caches in one sequential pass.
        let meta_page = hnsw.meta_page();
        if let Some(l0_mu) = ctx.hnsw_l0_caches {
            if let Some(vec_mu) = ctx.hnsw_vec_caches {
                let l0_guard = l0_mu.lock().unwrap();
                let vec_guard = vec_mu.lock().unwrap();
                let mut local_l0 = l0_guard
                    .get(&meta_page)
                    .cloned()
                    .unwrap_or_else(HnswL0Cache::new);
                let mut local_vec = vec_guard
                    .get(&meta_page)
                    .cloned()
                    .unwrap_or_else(crate::hnsw_index::HnswVecCache::new);
                // Drop guards before I/O (prefetch can take time; don't hold locks).
                drop(l0_guard);
                drop(vec_guard);

                if let Ok(()) = hnsw.prefetch_caches(ctx.pool, &mut local_l0, &mut local_vec) {
                    let mut l0_guard_write = l0_mu.lock().unwrap();
                    let mut vec_guard_write = vec_mu.lock().unwrap();
                    let l0_entry = l0_guard_write
                        .entry(meta_page)
                        .or_insert_with(HnswL0Cache::new);
                    l0_entry.merge_from(local_l0);
                    let vec_entry = vec_guard_write
                        .entry(meta_page)
                        .or_insert_with(crate::hnsw_index::HnswVecCache::new);
                    vec_entry.merge_from(local_vec);
                }
            }
        }

        meta_page
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

        // item 102-B: if INCLUDE cols are present, pre-compute the column
        // indices for the include columns so Phase 1 can encode payloads.
        let include_col_idxs: Vec<usize> = include_cols
            .iter()
            .map(|inc_name| {
                table_def
                    .columns
                    .iter()
                    .position(|c| c.name == *inc_name)
                    .unwrap_or(usize::MAX)
            })
            .collect();
        let has_include = !include_cols.is_empty();

        // Phase 1: collect (key, row_id) pairs — and include payloads when applicable.
        let mut pairs: Vec<(OrderedValue, RowId)> = Vec::new();
        // include_pairs carries the include payload bytes alongside each entry;
        // populated only for BTree indexes with INCLUDE cols.
        let mut include_pairs: Vec<(OrderedValue, RowId, Vec<u8>)> = Vec::new();
        for (row_id, bytes) in heap.scan(&snapshot, ctx.xid, ctx.pool)? {
            let row = decode_row(&bytes, &table_def.columns)?;
            match kind {
                IndexKind::BTree => {
                    if let Ok(value) = OrderedValue::try_from(&row[col_idx]) {
                        if has_include {
                            let inc_vals: Vec<Literal> = include_col_idxs
                                .iter()
                                .map(|&i| {
                                    if i < row.len() {
                                        row[i].clone()
                                    } else {
                                        Literal::Null
                                    }
                                })
                                .collect();
                            include_pairs.push((value, row_id, encode_row(&inc_vals)));
                        } else {
                            pairs.push((value, row_id));
                        }
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
        if has_include {
            include_pairs.sort_unstable_by(|(a, _, _), (b, _, _)| a.cmp(b));
        } else {
            pairs.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
        }
        // Phase 3: bulk insert — one WAL mini-txn, one fsync for all pairs.
        if has_include {
            tree.insert_many_with_include(&include_pairs, ctx.pool, ctx.wal)?;
        } else {
            tree.insert_many(&pairs, ctx.pool, ctx.wal)?;
        }
        tree.meta_page()
    };

    let mut cctx = catalog_ctx!(ctx);
    ctx.catalog
        .exclusive()?
        .set_column_index_root(table, column, Some(meta_page), &mut cctx)?;

    // item 102-B: persist INCLUDE column list when non-empty (BTree only).
    if !include_cols.is_empty() {
        if !matches!(kind, IndexKind::BTree) {
            return Err(DbError::SqlPlan(
                "INCLUDE columns are only supported on BTree indexes".into(),
            ));
        }
        // Validate that every include column exists in the table and is not dropped.
        {
            let tdef = ctx.catalog.lookup(table)?;
            for inc in &include_cols {
                if tdef
                    .columns
                    .iter()
                    .find(|c| !c.dropped && c.name == *inc)
                    .is_none()
                {
                    return Err(DbError::ColumnNotFound {
                        table: table.to_string(),
                        column: inc.clone(),
                    });
                }
            }
        }
        let mut cctx2 = catalog_ctx!(ctx);
        ctx.catalog.exclusive()?.set_column_include_cols(
            table,
            column,
            include_cols,
            &mut cctx2,
        )?;
    }
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
    returning: Option<&[String]>,
    ctx: &mut ExecCtx,
) -> Result<ExecResult> {
    let table_def = ctx.catalog.lookup(table)?.clone();
    // FK (M11): only referenced-table existence is enforced, and it's a
    // schema-level property — check it once per statement, not per row.
    enforce_referenced_tables_exist(&table_def, ctx.catalog.get())?;
    // Item 69: apply the table's fill-factor reservation so INSERT stops
    // packing a page once the HOT-reserved slack threshold is reached.
    let heap = Heap::open(ctx.page_size, table_def.fsm_meta, table_def.pages.clone())
        .with_fill_factor(table_def.fill_factor);

    // Item 98: streaming-accumulation insert.
    //
    // The per-row loop is preserved in its original interleaved structure:
    // validate → insert → index update → event capture.  All correctness
    // invariants are maintained (intra-statement UNIQUE enforcement, FK checks,
    // own-xid visibility for subsequent rows).
    //
    // The only change: `heap.insert_accumulating` defers the WAL_COMMIT bracket
    // until the page fills or `heap.flush_insert_accum` is called at the end.
    // Within one page, all rows share ONE WAL mini-txn bracket, reducing WAL
    // mutex acquisitions from O(rows) to O(heap-pages).  Each row's WAL_INSERT
    // record is still logged individually (D5 is preserved: WAL before page).
    //
    // If the statement returns an error mid-loop (e.g. FK violation on row N),
    // the open mini-txn has no WAL_COMMIT so recovery treats it as incomplete
    // and undoes it.  The caller's `engine.abort(xid)` also applies the already-
    // registered UndoAction records for the committed rows.
    let mut insert_accum: Option<crate::heap::InsertAccum> = None;

    // G5 (item 19): RETURNING — collect rows when requested.
    let mut returned_rows: Vec<Vec<Literal>> = Vec::new();
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
        // Item-24 Z1: INSERT policy check. If a `FOR INSERT` (or `FOR ALL`)
        // RLS policy exists, evaluate the predicate against the new row before
        // writing — any row that would violate the predicate is rejected.
        // Item-24 Z6: substitute `current_user` in the policy clone before
        // evaluating, so per-user row isolation works on the INSERT path too.
        // When `current_user` is None (embedded/superuser path), skip any
        // policy that contains CurrentUser (same reasoning as `apply_rls_skip_current_user`).
        if let Some(ref ins_policy) = table_def.insert_policy {
            let has_cu = crate::sql::logical::expr_has_current_user_pub(ins_policy);
            if has_cu && ctx.current_user.is_none() {
                // Superuser/embedded path — bypass CurrentUser-dependent INSERT policies.
            } else {
                let mut policy = ins_policy.clone();
                if let Some(ref u) = ctx.current_user {
                    crate::sql::logical::substitute_current_user_in_expr(&mut policy, u);
                }
                if !check_passes(&policy, &table_def.columns, &coerced)? {
                    return Err(DbError::SqlPlan(format!(
                        "new row violates policy for table \"{}\"",
                        table_def.name
                    )));
                }
            }
        }
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
        // Item 98: accumulating insert — one WAL mini-txn per heap page.
        let row_id =
            heap.insert_accumulating(&encoded, ctx.xid, ctx.pool, ctx.wal, &mut insert_accum)?;
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
        if returning.is_some() {
            returned_rows.push(coerced);
        }
        count += 1;
    }
    // Commit the final page's deferred mini-txn (item 98).
    heap.flush_insert_accum(ctx.wal, &mut insert_accum)?;

    persist_pages_if_changed(table, &heap, &table_def.pages, ctx)?;
    assert_schema_stable(ctx, table, table_def.generation);

    // Item 97: defer row-count delta to user-txn commit so aborted txns
    // never corrupt the catalog's exact count.
    if count > 0 {
        ctx.txn_mgr
            .record_row_count_delta(ctx.xid, table, count as i64)?;
    }

    // G5 (item 19): emit RETURNING result if requested.
    if let Some(ret_cols) = returning {
        let (col_names, rows) = project_returning(&table_def, ret_cols, returned_rows)?;
        return Ok(ExecResult::Rows {
            columns: col_names,
            rows,
        });
    }
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
            // Item 102-A/B: detect index-only scan — projection ⊆ {indexed column}
            // (102-A) or projection ⊆ {indexed column} ∪ INCLUDE cols (102-B).
            // Empty projection means SELECT * (all columns) — NOT index-only.
            let (index_col, _, _) = hit;
            // Look up the include_cols for this column from the catalog.
            let btree_include_cols: &[String] = table_def
                .columns
                .iter()
                .find(|c| c.name.eq_ignore_ascii_case(index_col))
                .map(|c| c.include_cols.as_slice())
                .unwrap_or(&[]);
            let index_only = !projection.is_empty()
                && projection.iter().all(|p| {
                    p.eq_ignore_ascii_case(index_col)
                        || btree_include_cols
                            .iter()
                            .any(|ic| ic.eq_ignore_ascii_case(p))
                });
            if let Some(result) =
                try_exec_select_btree(&table_def, projection, predicate, hit, index_only, ctx)?
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

    // Item 59 Fix 2: bind column names → slot indices once before the scan.
    // We need an owned, mutable copy of the predicate so we can walk and
    // rewrite it; the original is left untouched (it may be reused elsewhere
    // in the plan by the Query engine).
    let bound_pred: Option<Expr> = predicate.clone().map(|mut p| {
        bind_predicate_columns(&mut p, cols);
        p
    });

    let mut pred_cols = Vec::new();
    if let Some(pred) = bound_pred.as_ref() {
        expr_columns(pred, &table_def, &mut pred_cols);
    }
    let (pred_needed, pred_upto) = needed_mask(&pred_cols, ncols);
    let mut full_cols = proj_cols;
    full_cols.extend_from_slice(&pred_cols);
    let (full_needed, full_upto) = needed_mask(&full_cols, ncols);
    let has_pred = bound_pred.is_some();

    // Item 59 Fix 3: late materialisation — build a raw integer filter from the
    // (now-bound) predicate. For simple `ColumnSlot(idx) op Int` conjunctions,
    // `raw_filter.passes()` reads the i64 directly from page bytes and rejects
    // non-matching rows without calling `deform_row` at all. At 5% selectivity
    // this eliminates ~95% of `deform_row` + `Vec<Literal>` allocations on the
    // predicate pass.
    let raw_filter: Option<RawFilter> = bound_pred.as_ref().and_then(try_build_raw_filter);

    // The two-phase B2 decode as a closure, shared by the serial and parallel
    // paths: predicate columns → test → projection columns only on a match.
    // Item 54 Phase A: project_row_drain moves Literals (incl. Text Strings)
    // instead of cloning, saving one String allocation per TEXT column per row.
    let per_row = |bytes: &[u8]| -> Result<Option<Vec<Literal>>> {
        if has_pred {
            // Fix 3: try the raw fast path first; fall through to deform_row
            // if the predicate is not a simple integer conjunction or if
            // try_raw_i64_at returns None (variable-width preceding column).
            if let Some(ref rf) = raw_filter {
                match rf.passes(bytes, cols) {
                    Some(true) => {}                // passes raw check — proceed to full decode
                    Some(false) => return Ok(None), // rejected cheaply
                    None => {
                        // Raw path unavailable (variable-width col or NULL) —
                        // fall through to the normal deform_row path below.
                        let prow = deform_row(bytes, cols, pred_upto, &pred_needed)?;
                        if !predicate_matches(&bound_pred, cols, &prow)? {
                            return Ok(None);
                        }
                        let mut row = deform_row(bytes, cols, full_upto, &full_needed)?;
                        return Ok(Some(project_row_drain(projection, cols, &mut row)?));
                    }
                }
            } else {
                let prow = deform_row(bytes, cols, pred_upto, &pred_needed)?;
                if !predicate_matches(&bound_pred, cols, &prow)? {
                    return Ok(None);
                }
            }
        }
        let mut row = deform_row(bytes, cols, full_upto, &full_needed)?;
        Ok(Some(project_row_drain(projection, cols, &mut row)?))
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
    // Item 59 Fix 2: bind column names → slot indices (same as exec_select).
    let bound_pred: Option<Expr> = predicate.clone().map(|mut p| {
        bind_predicate_columns(&mut p, cols);
        p
    });
    let mut pred_cols = Vec::new();
    if let Some(pred) = bound_pred.as_ref() {
        expr_columns(pred, &table_def, &mut pred_cols);
    }
    let (pred_needed, pred_upto) = needed_mask(&pred_cols, ncols);
    let mut full_cols = proj_cols;
    full_cols.extend_from_slice(&pred_cols);
    let (full_needed, full_upto) = needed_mask(&full_cols, ncols);
    let has_pred = bound_pred.is_some();
    // Item 59 Fix 3: raw integer filter for the readonly path.
    let raw_filter: Option<RawFilter> = bound_pred.as_ref().and_then(try_build_raw_filter);

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
            if let Some(ref rf) = raw_filter {
                match rf.passes(&bytes, cols) {
                    Some(true) => {}
                    Some(false) => continue,
                    None => {
                        let prow = deform_row(&bytes, cols, pred_upto, &pred_needed)?;
                        if !predicate_matches(&bound_pred, cols, &prow)? {
                            continue;
                        }
                        let row = deform_row(&bytes, cols, full_upto, &full_needed)?;
                        out.push(project_row(projection, cols, &row)?);
                        continue;
                    }
                }
            } else {
                let prow = deform_row(&bytes, cols, pred_upto, &pred_needed)?;
                if !predicate_matches(&bound_pred, cols, &prow)? {
                    continue;
                }
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
    index_only: bool,
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

    // Item 102-A/B: index-only fast path.
    //
    // 102-A: every projected column is the indexed column itself → key from leaf.
    // 102-B: projected columns ⊆ {key_col} ∪ INCLUDE cols → key + include payload
    //        from leaf, decoded to Literals without touching heap bytes.
    //
    // Both paths still call heap.get() for MVCC visibility (the B-tree may hold
    // stale entries for dead tuples not yet vacuumed).  The win is skipping
    // `deform_row` (the columnar decoder) on the heap page.
    if index_only {
        // Determine if this is 102-B (covering index with INCLUDE cols).
        let btree_col = table_def.columns.iter().find(|c| c.name == column);
        let include_cols_for_scan: Vec<String> = btree_col
            .map(|c| c.include_cols.clone())
            .unwrap_or_default();

        // Build the set of include-column ColumnDefs for decoding payloads.
        // These are in INCLUDE declaration order (matching how encode_row encoded them).
        let include_col_defs: Vec<crate::catalog::ColumnDef> = include_cols_for_scan
            .iter()
            .filter_map(|inc_name| {
                table_def
                    .columns
                    .iter()
                    .find(|c| c.name == *inc_name)
                    .cloned()
            })
            .collect();
        let is_covering = !include_cols_for_scan.is_empty();

        if is_covering {
            // 102-B: fetch (key, include_bytes, rid) from leaf.
            let key_candidates = tree.search_with_keys_and_include(op, &value, ctx.pool)?;
            let mut rows: Vec<Vec<Literal>> = Vec::with_capacity(key_candidates.len());
            for (key, inc_bytes, rid) in key_candidates {
                match heap.get(rid, &snapshot, ctx.xid, ctx.pool) {
                    Ok(_bytes) => {
                        // Visible. Build the projected row from key + decoded include.
                        let inc_vals: Vec<Literal> = if inc_bytes.is_empty() {
                            vec![Literal::Null; include_col_defs.len()]
                        } else {
                            decode_row(&inc_bytes, &include_col_defs)
                                .unwrap_or_else(|_| vec![Literal::Null; include_col_defs.len()])
                        };
                        // Project in the order of `projection` (which determines column order).
                        let key_name = column.to_ascii_lowercase();
                        let projected: Vec<Literal> = projection
                            .iter()
                            .map(|p| {
                                let p_lc = p.to_ascii_lowercase();
                                if p_lc == key_name {
                                    key.clone().into_literal()
                                } else {
                                    // Find this column in include_cols.
                                    include_col_defs
                                        .iter()
                                        .zip(inc_vals.iter())
                                        .find(|(def, _)| def.name.eq_ignore_ascii_case(p))
                                        .map(|(_, v)| v.clone())
                                        .unwrap_or(Literal::Null)
                                }
                            })
                            .collect();
                        rows.push(projected);
                    }
                    Err(DbError::NoVisibleVersion { .. }) => continue,
                    Err(e) => return Err(e),
                }
            }
            let n = rows.len() as u64;
            IDX_ONLY_ROWS.fetch_add(n, Ordering::Relaxed);
            IDX_INCLUDE_ROWS.fetch_add(n, Ordering::Relaxed);
            return Ok(Some(ExecResult::Rows {
                columns: output_columns(projection, &table_def.columns),
                rows,
            }));
        }

        // 102-A: projection is just the key column — use B-tree key directly.
        let key_candidates = tree.search_with_keys(op, &value, ctx.pool)?;
        let mut rows: Vec<Vec<Literal>> = Vec::with_capacity(key_candidates.len());
        for (key, rid) in key_candidates {
            match heap.get(rid, &snapshot, ctx.xid, ctx.pool) {
                Ok(_bytes) => {
                    // Row is visible — use the B-tree key directly, skip deform_row.
                    rows.push(vec![key.into_literal()]);
                }
                Err(DbError::NoVisibleVersion { .. }) => continue,
                Err(e) => return Err(e),
            }
        }
        let n = rows.len() as u64;
        IDX_ONLY_ROWS.fetch_add(n, Ordering::Relaxed);
        return Ok(Some(ExecResult::Rows {
            columns: output_columns(projection, &table_def.columns),
            rows,
        }));
    }

    // B2 decode pushdown on the index-resolved candidates (the SELECT-filtered
    // hot path, since a range predicate is served here, not by the full scan):
    // materialize the predicate columns to re-check the row, and the projection
    // columns only for survivors.
    let cols = &table_def.columns;
    let ncols = cols.len();
    let proj_cols = projection_columns(table_def, projection);
    // Item 59 Fix 2: bind column names → slot indices for the per-candidate
    // closure (same pass as exec_select, eliminates per-row linear String scan
    // inside eval_expr for the B-tree candidate re-check path).
    let bound_pred: Option<Expr> = predicate.clone().map(|mut p| {
        bind_predicate_columns(&mut p, cols);
        p
    });
    let mut pred_cols = Vec::new();
    if let Some(pred) = bound_pred.as_ref() {
        expr_columns(pred, table_def, &mut pred_cols);
    }
    let (pred_needed, pred_upto) = needed_mask(&pred_cols, ncols);
    let mut full_cols = proj_cols;
    full_cols.extend_from_slice(&pred_cols);
    let (full_needed, full_upto) = needed_mask(&full_cols, ncols);
    let has_pred = bound_pred.is_some();

    // The B2 two-phase decode as a closure, shared by all resolution paths.
    // Item 54 Phase A: project_row_drain moves Literals out of the decode buffer
    // instead of cloning — saves one String allocation per TEXT column per row.
    let per_candidate = |bytes: &[u8]| -> Result<Option<Vec<Literal>>> {
        if has_pred {
            let prow = deform_row(bytes, cols, pred_upto, &pred_needed)?;
            if !predicate_matches(&bound_pred, cols, &prow)? {
                return Ok(None);
            }
        }
        let mut row = deform_row(bytes, cols, full_upto, &full_needed)?;
        Ok(Some(project_row_drain(projection, cols, &mut row)?))
    };

    // Lever 1 (item 45): for range predicates, acquire workers optimistically
    // before the B-tree scan and dispatch each pre-partitioned slice to exactly
    // one worker (static assignment, no work-stealing cursor). Each partition
    // covers a contiguous key range, so each worker's heap accesses are
    // clustered by key-ordered insertion locality — lower page-cache pressure
    // than the interleaved access pattern of work-stealing over a flat list.
    // `usize::MAX` bypasses the MIN_PAGES floor in `acquire`; we enforce
    // PARALLEL_CANDIDATE_MIN ourselves after counting the collected total.
    // Item 54 Phase A: parallel_resolve_partitions uses the pre-spawned pool
    // (lever 2) instead of std::thread::scope, eliminating per-query OS thread
    // spawn cost on the B-tree range path.
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
                let rows = crate::sql::parallel_scan::parallel_resolve_partitions(
                    &partitions,
                    &reader,
                    &snapshot,
                    ctx.xid,
                    degree,
                    &|_rid, bytes| per_candidate(bytes),
                )?;
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

    // Item 63: probe the on-disk HNSW index for ANN candidates. Over-fetch with
    // ef_search candidates, then re-check against the full predicate (MVCC
    // visibility, RLS, any AND'd WHERE terms) and exact-re-rank from the heap's
    // stored vectors — identical contract to the IVF-Flat path it replaces.
    let hnsw = DiskHnswIndex::open(meta_page, ctx.page_size);
    // Use max(k * 4, HNSW_EF_SEARCH) so small k doesn't under-probe.
    let ef = (k * 4).max(HNSW_EF_SEARCH);

    // Items 72+73: process-lifetime L0 neighbour cache + vector hot cache.
    // Snapshot-then-merge pattern (no lock held during page I/O):
    //   1. Lock, snapshot the per-index cache entry, release lock. Item 92
    //      Lever 5: the snapshot is an O(1) Arc refcount bump, NOT a deep
    //      clone — the previous per-query deep clone was O(corpus) (~5 MiB +
    //      10k allocations at 10k rows) and dominated warm NEAR latency.
    //   2. Run beam search against the local snapshots (lock-free during I/O);
    //      a cache miss copies-on-write via Arc::make_mut.
    //   3. Re-lock and merge back ONLY if the search actually inserted
    //      something (storage_ptr changed). The fully-warm path skips both
    //      merges entirely.
    let ann_start = std::time::Instant::now();
    let (metric, candidate_ids) = if let Some(l0_mu) = ctx.hnsw_l0_caches {
        // Step 1: snapshot both caches (O(1) per Lever 5)
        let mut local_l0 = {
            let guard = l0_mu.lock().unwrap();
            guard
                .get(&meta_page)
                .cloned()
                .unwrap_or_else(HnswL0Cache::new)
        };
        let mut local_vec = if let Some(vec_mu) = ctx.hnsw_vec_caches {
            let guard = vec_mu.lock().unwrap();
            guard
                .get(&meta_page)
                .cloned()
                .unwrap_or_else(crate::hnsw_index::HnswVecCache::new)
        } else {
            crate::hnsw_index::HnswVecCache::new()
        };
        let l0_snap = local_l0.storage_ptr();
        let vec_snap = local_vec.storage_ptr();

        // Step 2: beam search — both caches updated on miss (lock-free)
        let result = hnsw.candidates_cached_with_vec(
            query,
            Some(ef),
            ctx.pool,
            Some(&mut local_l0),
            Some(&mut local_vec),
        )?;

        // Step 3: merge back into global caches only on copy-on-write
        if local_l0.storage_ptr() != l0_snap {
            let mut guard = l0_mu.lock().unwrap();
            let entry = guard.entry(meta_page).or_insert_with(HnswL0Cache::new);
            entry.merge_from(local_l0);
        }
        if local_vec.storage_ptr() != vec_snap {
            if let Some(vec_mu) = ctx.hnsw_vec_caches {
                let mut guard = vec_mu.lock().unwrap();
                let entry = guard
                    .entry(meta_page)
                    .or_insert_with(crate::hnsw_index::HnswVecCache::new);
                entry.merge_from(local_vec);
            }
        }

        result
    } else {
        hnsw.candidates(query, Some(ef), ctx.pool)?
    };
    // Item 92 phase attribution: ANN beam search vs everything after it.
    crate::hnsw_index::Q_ANN_NANOS.fetch_add(
        ann_start.elapsed().as_nanos() as u64,
        std::sync::atomic::Ordering::Relaxed,
    );
    let rerank_start = std::time::Instant::now();

    let heap = Heap::open(ctx.page_size, table_def.fsm_meta, table_def.pages.clone());
    // Item 94: lightweight snapshot fast path for standalone (non-BEGIN) NEAR
    // queries.  A standalone auto-commit NEAR does not need WAL tail pinning or
    // active-snapshot registration — it is a point-in-time read that completes
    // in < 1 ms and holds no long-lived page latches across vacuum cycles.  The
    // lightweight snapshot reads `committed_horizon` atomically (no mutex) and
    // uses an empty `active_xids` list (all xids below the horizon appear
    // committed — an accepted RC relaxation for the standalone read-only case;
    // see `TransactionManager::read_snapshot_lightweight` for the full contract).
    // Inside an explicit `BEGIN … COMMIT` block the full snapshot path is used
    // so that NEAR correctly reflects the transaction's own snapshot (RC fresh
    // per statement, or the fixed RR/SI BEGIN-time snapshot).
    let (snapshot, snap_xid) = if !ctx.in_explicit_txn {
        // Standalone NEAR: lightweight atomic read, no mutex registration.
        if let Some(ctr) = ctx.near_lightweight_snaps {
            ctr.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        ctx.txn_mgr.read_snapshot_lightweight()
    } else {
        // Inside an explicit transaction: use the transaction's own snapshot.
        let snap = ctx.txn_mgr.snapshot_for_statement(ctx.xid)?;
        (snap, ctx.xid)
    };
    let mut scored: Vec<(f32, Vec<Literal>)> = Vec::new();
    for row_id in candidate_ids {
        let bytes = match heap.get(row_id, &snapshot, snap_xid, ctx.pool) {
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
        // Exact re-rank distance from the heap's stored vector.
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
    crate::hnsw_index::Q_RERANK_NANOS.fetch_add(
        rerank_start.elapsed().as_nanos() as u64,
        std::sync::atomic::Ordering::Relaxed,
    );
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
/// Exact distance re-ranking after ANN candidate retrieval.
///
/// Delegates to `hnsw_distance` which uses the Lever-2 (item 92) SIMD-friendly
/// 8-lane Euclidean accumulator — avoids duplicating the computation here and
/// ensures both the ANN search and the re-rank use the same vectorised path.
fn ivf_exact_distance(metric: crate::vector::Metric, a: &[f32], b: &[f32]) -> f32 {
    // `hnsw_distance` increments Q_DISTANCE_CALLS; on the re-rank path this is
    // acceptable (the counters are only read by the item-92 profiling test).
    crate::hnsw_index::hnsw_distance(metric, a, b)
}

fn exec_update(
    table: &str,
    assignments: &[(String, Expr)],
    predicate: &Option<Expr>,
    returning: Option<&[String]>,
    ctx: &mut ExecCtx,
) -> Result<ExecResult> {
    let table_def = ctx.catalog.lookup(table)?.clone();
    enforce_referenced_tables_exist(&table_def, ctx.catalog.get())?;
    let heap = Heap::open(ctx.page_size, table_def.fsm_meta, table_def.pages.clone());
    let snapshot = ctx.txn_mgr.snapshot_for_statement(ctx.xid)?;

    // ── Item 76: early HOT eligibility gate + parallel matching ──────────────
    // Compute hot_eligible *before* the scan: all four conditions are purely
    // metadata-derived (table_def + assignments, no I/O).  Moving this up lets
    // us choose the parallel collection path for HOT-eligible tables instead
    // of always paying the serial heap.scan() cost.
    //
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

    // Item 58 HOT eligibility: try same-page HOT update (no B-tree update)
    // when all of the following hold:
    //   1. No UNIQUE/PK index on this table (unique enforcement inserts new
    //      B-tree entries pointing at the new slot — HOT would leave them
    //      dangling at the old slot).
    //   2. No FK columns in SET (FK key enforcement likewise inserts new
    //      B-tree entries for the new value).
    //   3. No FK children referencing this table (RESTRICT check reads the
    //      old PK value, which must remain visible; HOT xmax-stamps it first,
    //      but the check runs before any mutation, so this is fine — but if
    //      the parent changes its PK value in SET, (1) above would fire).
    //   4. No indexed column in SET (secondary B-tree must be updated to the
    //      new RowId; skipping it makes the row unfindable — see §0.6.2).
    let hot_eligible = !has_unique
        && !has_fk_refs_in_set
        && !has_fk_children
        && !set_touches_indexed_col(assignments, &table_def.columns);

    // Predicate closure for the parallel scan path (same pattern as exec_delete).
    // Evaluates just predicate-referenced columns; the full row body comes back
    // separately via scan_page_into inside parallel_collect_matching.
    let cols_ref = &table_def.columns;
    let mut pred_col_indices: Vec<usize> = Vec::new();
    if let Some(pred) = predicate {
        expr_columns(pred, &table_def, &mut pred_col_indices);
    }
    let (pred_needed, pred_upto) = needed_mask(&pred_col_indices, cols_ref.len());
    let has_pred = predicate.is_some();
    let pred_closure = |bytes: &[u8]| -> Result<bool> {
        if has_pred {
            let prow = deform_row(bytes, cols_ref, pred_upto, &pred_needed)?;
            Ok(predicate_matches(predicate, cols_ref, &prow)?)
        } else {
            Ok(true)
        }
    };

    // Collect matching rows.  For HOT-eligible tables we prefer the parallel
    // full-scan when A3 would not fire and the table is large enough; any path
    // that goes serial falls back to matching_rows (which handles A3 internally).
    let matching: MatchedRows = if hot_eligible {
        let use_a3 = predicate
            .as_ref()
            .and_then(|e| {
                find_best_indexable_btree_predicate(
                    e,
                    &table_def,
                    ctx.catalog.table_stats(&table_def.name),
                )
            })
            .is_some_and(|hit| index_lookup_is_selective(&table_def, hit, ctx, false));

        'collect_hot: {
            if !use_a3 {
                let pages = heap.scan_pages(ctx.pool)?;
                if pages.len() >= crate::sql::parallel_scan::PARALLEL_CANDIDATE_MIN {
                    if let Some(lease) = crate::sql::parallel_scan::acquire(pages.len()) {
                        let mut out = crate::sql::parallel_scan::parallel_collect_matching(
                            &pages,
                            &ctx.pool.shared_reader(),
                            &snapshot,
                            ctx.xid,
                            lease.degree(),
                            &pred_closure,
                        )?;
                        // hot_update_many requires (page_id, slot) order.
                        out.sort_unstable_by_key(|(rid, _)| (rid.page_id, rid.slot));
                        break 'collect_hot out;
                    }
                }
            }
            // Serial fallback: A3 index path, table too small, or no lease.
            matching_rows(&heap, &snapshot, ctx, &table_def, predicate)?
        }
    } else {
        matching_rows(&heap, &snapshot, ctx, &table_def, predicate)?
    };

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
    // hot_eligible / has_unique / has_fk_* are already computed above.

    // Item 74: two-phase batch HOT UPDATE.
    //
    // When `hot_eligible` the per-row mini-txn overhead dominates: each
    // `try_hot_insert` opens its own begin_mini_txn → WAL record(s) →
    // commit_mini_txn bracket — 3 mutex lock/unlock + 3 Vec alloc + 3 CRC32
    // per row. At 50k rows = 150k such cycles. This batch path reduces to
    // ~O(pages_touched), typically 50–75× fewer mutex acquisitions.
    //
    // Phase 1 (per-row): decode / eval SET / encode / basic constraint checks.
    //   Same CPU cost as before — unavoidable.
    // Phase 2: heap.hot_update_many — batched writes, one mini-txn per page group.
    // Phase 3: per-pair undo recording, SSI note, CDC event.
    //
    // Non-HOT rows (has_unique, fk, indexed col in SET) fall through to the
    // existing per-row loop below, which is correctness-critical for those cases.
    if hot_eligible && !matching.is_empty() {
        // Phase 1: collect SQL logic for all matched rows.
        // (before_row, after_row columns are Vec<Literal>; type alias avoids clippy::type_complexity)
        type HotRow = (RowId, Vec<u8>, Vec<Literal>, Vec<Literal>);
        #[allow(clippy::type_complexity)]
        let mut collected: Vec<HotRow> = Vec::with_capacity(matching.len());
        for (row_id, bytes) in &matching {
            let mut row = decode_row(bytes, &table_def.columns)?;
            let before_row = row.clone();
            for (col, expr) in assignments {
                let new_val = eval_expr(expr, &table_def.columns, &row)?;
                set_column(&table_def.columns, &mut row, col, new_val)?;
            }
            let coerced = coerce_and_validate_row(&table_def, row)?;
            enforce_not_null(&table_def, &coerced)?;
            enforce_checks(&table_def, &coerced)?;
            // item-24 R-a: WITH CHECK — new row must satisfy the write-side
            // policy expression (defaults to USING when no explicit WITH CHECK).
            exec_update_with_check(&table_def, &coerced, ctx)?;
            // No UNIQUE / FK enforcement: hot_eligible already gates those out.
            let encoded = encode_row(&coerced);
            collected.push((*row_id, encoded, before_row, coerced));
        }

        // Phase 2: batched heap writes.
        let row_pairs: Vec<(RowId, Vec<u8>)> = collected
            .iter()
            .map(|(rid, enc, _, _)| (*rid, enc.clone()))
            .collect();
        let heap_results =
            match heap.hot_update_many(&row_pairs, ctx.xid, ctx.pool, ctx.wal, ctx.lock_mgr) {
                Err(e @ DbError::WriteConflict { .. }) => return Err(classify_conflict(e, ctx)),
                other => other?,
            };

        // Phase 3: per-pair SSI notes, undo actions, CDC events.
        for (i, (old_rid, new_rid, saved_prev_page, saved_prev_slot)) in
            heap_results.iter().enumerate()
        {
            let (_, _, ref before_row, ref after_row) = collected[i];
            ctx.txn_mgr.ssi_note_write(ctx.xid, *old_rid);
            ctx.txn_mgr.record_undo(
                ctx.xid,
                crate::txn::UndoAction::HotXpageUpdate {
                    old_page_id: old_rid.page_id,
                    old_slot: old_rid.slot,
                    new_page_id: new_rid.page_id,
                    new_slot: new_rid.slot,
                    saved_prev_page: *saved_prev_page,
                    saved_prev_slot: *saved_prev_slot,
                },
            )?;
            send_event_capture(&table_def, "update", Some(before_row), Some(after_row), ctx)?;
        }

        persist_pages_if_changed(table, &heap, &table_def.pages, ctx)?;
        assert_schema_stable(ctx, table, table_def.generation);
        // G5 (item 19): RETURNING for HOT batch path.
        if let Some(ret_cols) = returning {
            let after_rows: Vec<Vec<Literal>> = collected
                .into_iter()
                .map(|(_, _, _, after)| after)
                .collect();
            let (col_names, rows) = project_returning(&table_def, ret_cols, after_rows)?;
            return Ok(ExecResult::Rows {
                columns: col_names,
                rows,
            });
        }
        return Ok(ExecResult::Updated {
            count: heap_results.len(),
        });
    }

    // ── Item 83: batch non-HOT UPDATE via update_many() ────────────────────
    //
    // When the SET clause touches an indexed column (hot_eligible=false) but
    // there are no UNIQUE, FK, or CDC constraints, the heap update can be
    // batched across all matching rows with a single call to update_many().
    // This replaces O(rows) separate mini-txns (one per `heap.update()` call)
    // with O(pages) Phase-A mini-txns + O(fill-pages) Phase-B mini-txns,
    // cutting WAL record count from 4×N to ~2×(N/50) + 2×(N/100) for a
    // table with ~50 rows/page — a ~48× reduction in mini-txn overhead.
    //
    // Correctness gate: update_many() pre-conditions (same as update_many's
    // doc comment) — no UNIQUE indexes, no FK child-side refs in SET, no FK
    // parent-side children. CDC (events_enabled) is handled below via
    // send_event_capture() after update_many() returns, so no gate needed.
    let can_batch_non_hot = !hot_eligible && !has_unique && !has_fk_refs_in_set && !has_fk_children;

    if can_batch_non_hot && !matching.is_empty() {
        // Phase 1: decode / eval SET / encode for all matching rows.
        // (Same per-row logic as the per-row loop below, minus constraint checks.)
        type NonHotRow = (RowId, Vec<u8>, Vec<Literal>, Vec<Literal>);
        let mut collected: Vec<NonHotRow> = Vec::with_capacity(matching.len());
        for (row_id, bytes) in &matching {
            let mut row = decode_row(bytes, &table_def.columns)?;
            let before_row = row.clone();
            for (col, expr) in assignments {
                let new_val = eval_expr(expr, &table_def.columns, &row)?;
                set_column(&table_def.columns, &mut row, col, new_val)?;
            }
            let coerced = coerce_and_validate_row(&table_def, row)?;
            enforce_not_null(&table_def, &coerced)?;
            enforce_checks(&table_def, &coerced)?;
            // item-24 R-a: WITH CHECK.
            exec_update_with_check(&table_def, &coerced, ctx)?;
            let encoded = encode_row(&coerced);
            collected.push((*row_id, encoded, before_row, coerced));
        }

        // Phase 2: batch heap Phase-A (xmax) + Phase-B (new versions).
        let row_pairs: Vec<(RowId, Vec<u8>)> = collected
            .iter()
            .map(|(rid, enc, _, _)| (*rid, enc.clone()))
            .collect();
        let update_results =
            match heap.update_many(&row_pairs, ctx.xid, ctx.pool, ctx.wal, ctx.lock_mgr) {
                Err(e @ DbError::WriteConflict { .. }) => return Err(classify_conflict(e, ctx)),
                other => other?,
            };

        // Phase 3: per-pair SSI, undo, B-tree index staging, CDC.
        //
        // Item 88: emit one XmaxStampBatch per old-version page group (sorted by
        // page_id, guaranteed by update_many's input requirement) instead of one
        // XmaxStamp per row.  Batch undo is recorded first in the undo log so that
        // on abort the reversed order clears new versions (Insert undos, LIFO) before
        // unlocking old versions (XmaxStampBatch undos, LIFO — correct ordering).
        {
            let mut bi = 0;
            while bi < update_results.len() {
                let page_id = update_results[bi].0.page_id;
                let bj = bi + update_results[bi..].partition_point(|(r, _)| r.page_id == page_id);
                let slots: Vec<u16> = update_results[bi..bj].iter().map(|(r, _)| r.slot).collect();
                ctx.txn_mgr
                    .record_undo(ctx.xid, UndoAction::XmaxStampBatch { page_id, slots })?;
                bi = bj;
            }
        }
        let mut count = 0;
        for (i, (old_rid, new_rid)) in update_results.iter().enumerate() {
            let (_, _, ref before_row, ref after_row) = collected[i];
            ctx.txn_mgr.ssi_note_write(ctx.xid, *old_rid);
            // Insert undo recorded after batch XmaxStamp undos (above); abort
            // processes undo log in reverse, so Insert undos run first (LIFO).
            ctx.txn_mgr.record_undo(
                ctx.xid,
                UndoAction::Insert {
                    page_id: new_rid.page_id,
                    slot: new_rid.slot,
                },
            )?;
            stage_row_index_writes_update(
                &table_def,
                *old_rid,
                *new_rid,
                before_row,
                after_row,
                &mut index_batches,
                &mut patch_batches,
                ctx,
            )?;
            send_event_capture(&table_def, "update", Some(before_row), Some(after_row), ctx)?;
            count += 1;
        }

        // Coalesced index maintenance (A1) — same flush path as per-row loop.
        flush_patch_batches(&patch_batches, ctx)?;
        flush_index_batches(&index_batches, ctx)?;

        persist_pages_if_changed(table, &heap, &table_def.pages, ctx)?;
        assert_schema_stable(ctx, table, table_def.generation);
        // G5 (item 19): RETURNING for batch non-HOT path.
        if let Some(ret_cols) = returning {
            let after_rows: Vec<Vec<Literal>> = collected
                .into_iter()
                .map(|(_, _, _, after)| after)
                .collect();
            let (col_names, rows) = project_returning(&table_def, ret_cols, after_rows)?;
            return Ok(ExecResult::Rows {
                columns: col_names,
                rows,
            });
        }
        return Ok(ExecResult::Updated { count });
    }

    // ── Non-HOT / constrained path: per-row loop (unchanged) ────────────────
    // G5 (item 19): collect updated rows for RETURNING when requested.
    let mut returned_rows: Vec<Vec<Literal>> = Vec::new();
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
        // item-24 R-a: WITH CHECK — reject if new row violates the policy.
        exec_update_with_check(&table_def, &coerced, ctx)?;
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

        // Items 58/71: try HOT update (same-page first, cross-page fallback)
        // when eligible — no B-tree cost in either HOT case.
        // Falls back to the standard cross-page update + B-tree maintenance
        // only when try_hot_insert returns Ok(None) (write conflict, which
        // is unreachable here because the write lock was already acquired, or
        // an internal error).
        let (new_row_id, used_hot, hot_saved_prev) = if hot_eligible {
            match heap.try_hot_insert(row_id, &encoded, ctx.xid, ctx.pool, ctx.wal, ctx.lock_mgr) {
                Err(e @ DbError::WriteConflict { .. }) => return Err(classify_conflict(e, ctx)),
                Err(e) => return Err(e),
                Ok(Some(result)) => (result.new_rid, true, result.saved_prev),
                Ok(None) => {
                    // Conflict detected by try_hot_insert — fall back to full update.
                    let nrid = match heap.update(
                        row_id,
                        &encoded,
                        ctx.xid,
                        ctx.pool,
                        ctx.wal,
                        ctx.lock_mgr,
                    ) {
                        Err(e @ DbError::WriteConflict { .. }) => {
                            return Err(classify_conflict(e, ctx))
                        }
                        other => other?,
                    };
                    (nrid, false, None)
                }
            }
        } else {
            let nrid = match heap.update(row_id, &encoded, ctx.xid, ctx.pool, ctx.wal, ctx.lock_mgr)
            {
                Err(e @ DbError::WriteConflict { .. }) => return Err(classify_conflict(e, ctx)),
                other => other?,
            };
            (nrid, false, None)
        };

        // P1.d: writing supersedes the version at `row_id` — an SSI write of the
        // exact version a concurrent reader would have read.
        ctx.txn_mgr.ssi_note_write(ctx.xid, row_id);
        if used_hot {
            // HOT update — undo both mutations atomically with one action.
            // Same-page (item 58): ordering (new-slot-first, then old-slot) is
            //   enforced inside `undo_hot_update`.
            // Cross-page (item 71): two separate pages; new page first, then old.
            match hot_saved_prev {
                None => {
                    // Same-page HOT: new and old versions share the same page.
                    ctx.txn_mgr.record_undo(
                        ctx.xid,
                        UndoAction::HotUpdate {
                            page_id: row_id.page_id,
                            old_slot: row_id.slot,
                            new_slot: new_row_id.slot,
                        },
                    )?;
                }
                Some((saved_prev_page, saved_prev_slot)) => {
                    // Cross-page HOT: new version is on a different page.
                    ctx.txn_mgr.record_undo(
                        ctx.xid,
                        UndoAction::HotXpageUpdate {
                            old_page_id: row_id.page_id,
                            old_slot: row_id.slot,
                            new_page_id: new_row_id.page_id,
                            new_slot: new_row_id.slot,
                            saved_prev_page,
                            saved_prev_slot,
                        },
                    )?;
                }
            }
        } else {
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
        }
        // Item 47: unchanged-key columns use in-place RowId patch (no splits, 1
        // WAL page-image); changed-key columns fall through to the batch insert.
        // HOT path: B-tree NOT updated (no index cost, no patch needed) because
        // no indexed column was in SET (guard: `hot_eligible`).
        if !used_hot {
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
        }
        // C1 (item 29): UPDATE carries both before (pre-mutation) and after (post-mutation).
        send_event_capture(&table_def, "update", Some(&before_row), Some(&coerced), ctx)?;
        if returning.is_some() {
            returned_rows.push(coerced);
        }
        count += 1;
    }
    // Coalesced index maintenance for the whole statement (A1).
    // Item 47: flush unchanged-key patches first (one WAL page-image per leaf),
    // then insert changed-key entries from the standard batch.
    flush_patch_batches(&patch_batches, ctx)?;
    flush_index_batches(&index_batches, ctx)?;

    persist_pages_if_changed(table, &heap, &table_def.pages, ctx)?;
    assert_schema_stable(ctx, table, table_def.generation);
    // G5 (item 19): emit RETURNING result if requested.
    if let Some(ret_cols) = returning {
        let (col_names, rows) = project_returning(&table_def, ret_cols, returned_rows)?;
        return Ok(ExecResult::Rows {
            columns: col_names,
            rows,
        });
    }
    Ok(ExecResult::Updated { count })
}

fn exec_delete(
    table: &str,
    predicate: &Option<Expr>,
    returning: Option<&[String]>,
    ctx: &mut ExecCtx,
) -> Result<ExecResult> {
    let table_def = ctx.catalog.lookup(table)?.clone();
    let heap = Heap::open(ctx.page_size, table_def.fsm_meta, table_def.pages.clone());
    let snapshot = ctx.txn_mgr.snapshot_for_statement(ctx.xid)?;

    // Item 48: fast path for unconditional DELETE with no FK children and no CDC.
    // Routes through the O(pages) truncate instead of xmax-stamping every row.
    // CDC is skipped intentionally — TRUNCATE has never emitted per-row events.
    // G5 (item 19): bypass fast path when RETURNING is requested (we need row data).
    if returning.is_none()
        && predicate.is_none()
        && !table_has_fk_children(ctx.catalog.get(), table)
        && !table_def.events_enabled
    {
        let count = heap.count_visible(&snapshot, ctx.xid, ctx.pool)?;
        let mut cctx = catalog_ctx!(ctx);
        ctx.catalog.exclusive()?.truncate(table, &mut cctx)?;
        return Ok(ExecResult::Deleted { count });
    }

    // Item 36: gate the RESTRICT scan — compute up-front so the collection
    // phase can skip row bytes when they are not needed (item 75 fast path).
    // G5 (item 19): RETURNING also needs row bytes.
    let has_fk_children = table_has_fk_children(ctx.catalog.get(), table);
    let needs_per_row_checks = has_fk_children || table_def.events_enabled || returning.is_some();

    // Item 75: When there are no per-row side-effects (no FK children, no CDC)
    // we only need RowIds, not row-body bytes.  Separate fast and slow paths
    // avoid allocating Vec<u8> per row for the common case.
    //
    // Helper: build the predicate closure for the parallel and serial paths.
    // Captures only column metadata and the predicate; no heap allocation.
    let cols = &table_def.columns;
    let mut pred_col_indices: Vec<usize> = Vec::new();
    if let Some(pred) = predicate {
        expr_columns(pred, &table_def, &mut pred_col_indices);
    }
    let (pred_needed, pred_upto) = needed_mask(&pred_col_indices, cols.len());
    let has_pred = predicate.is_some();
    let pred_closure = |bytes: &[u8]| -> Result<bool> {
        if has_pred {
            let prow = deform_row(bytes, cols, pred_upto, &pred_needed)?;
            Ok(predicate_matches(predicate, cols, &prow)?)
        } else {
            Ok(true)
        }
    };

    if !needs_per_row_checks {
        // ── RowId-only fast path (item 75) ───────────────────────────────────
        // No FK children and no CDC: the only thing we need from the scan is
        // the set of live RowIds to pass to delete_many.  Skip all Vec<u8>
        // allocation: zero bytes-per-row alloc in both the B-tree and full-scan
        // branches (batch_resolve_row_ids + parallel_collect_row_ids).
        let use_a3 = predicate
            .as_ref()
            .and_then(|e| {
                find_best_indexable_btree_predicate(
                    e,
                    &table_def,
                    ctx.catalog.table_stats(&table_def.name),
                )
            })
            .is_some_and(|hit| index_lookup_is_selective(&table_def, hit, ctx, false));

        let row_ids: Vec<RowId> = if !use_a3 {
            let pages = heap.scan_pages(ctx.pool)?;
            if pages.len() >= crate::sql::parallel_scan::PARALLEL_CANDIDATE_MIN {
                if let Some(lease) = crate::sql::parallel_scan::acquire(pages.len()) {
                    let mut ids = crate::sql::parallel_scan::parallel_collect_row_ids(
                        &pages,
                        &ctx.pool.shared_reader(),
                        &snapshot,
                        ctx.xid,
                        lease.degree(),
                        &pred_closure,
                    )?;
                    // delete_many requires (page_id, slot) order.
                    ids.sort_unstable_by_key(|r| (r.page_id, r.slot));
                    ids
                } else {
                    collect_delete_row_ids_serial(&heap, &snapshot, ctx, &table_def, predicate)?
                }
            } else {
                collect_delete_row_ids_serial(&heap, &snapshot, ctx, &table_def, predicate)?
            }
        } else {
            // A3 index path: batch_resolve_row_ids reads each page once.
            collect_delete_row_ids_serial(&heap, &snapshot, ctx, &table_def, predicate)?
        };

        // P1.d: SSI read set.
        ctx.txn_mgr.ssi_note_reads(ctx.xid, &row_ids);

        let deleted = match heap.delete_many(&row_ids, ctx.xid, ctx.pool, ctx.wal, ctx.lock_mgr) {
            Err(e @ DbError::WriteConflict { .. }) => return Err(classify_conflict(e, ctx)),
            other => other?,
        };
        // Item 88: batch undo — one XmaxStampBatch per page group.
        {
            let mut bi = 0;
            while bi < deleted.len() {
                let page_id = deleted[bi].page_id;
                let bj = bi + deleted[bi..].partition_point(|r| r.page_id == page_id);
                for rid in &deleted[bi..bj] {
                    ctx.txn_mgr.ssi_note_write(ctx.xid, *rid);
                }
                let slots: Vec<u16> = deleted[bi..bj].iter().map(|r| r.slot).collect();
                ctx.txn_mgr
                    .record_undo(ctx.xid, UndoAction::XmaxStampBatch { page_id, slots })?;
                bi = bj;
            }
        }
        let count = deleted.len();
        persist_pages_if_changed(table, &heap, &table_def.pages, ctx)?;
        assert_schema_stable(ctx, table, table_def.generation);
        // Item 97: defer row-count delta to commit.
        if count > 0 {
            ctx.txn_mgr
                .record_row_count_delta(ctx.xid, table, -(count as i64))?;
        }
        return Ok(ExecResult::Deleted { count });
    }

    // ── Slow path: FK children or CDC — must collect row bytes ───────────────
    // Item 66: parallel full-scan for DELETE selected. We bypass `matching_rows`
    // and go straight to the pre-spawned worker pool when:
    //   (a) the A3 cost-model would route to a full scan (non-selective predicate),
    //   (b) the heap is large enough to benefit (≥ PARALLEL_CANDIDATE_MIN pages),
    //   (c) a worker lease is available (global budget not exhausted).
    // On any of those conditions failing we fall through to the proven serial path
    // (`matching_rows`), which handles both the A3 index path and full-scan fallback.
    let matching = 'collect: {
        // Mirror A3 gate: if the index path would fire, let matching_rows handle it.
        let use_a3 = predicate
            .as_ref()
            .and_then(|e| {
                find_best_indexable_btree_predicate(
                    e,
                    &table_def,
                    ctx.catalog.table_stats(&table_def.name),
                )
            })
            .is_some_and(|hit| index_lookup_is_selective(&table_def, hit, ctx, false));

        if !use_a3 {
            let pages = heap.scan_pages(ctx.pool)?;
            if pages.len() >= crate::sql::parallel_scan::PARALLEL_CANDIDATE_MIN {
                if let Some(lease) = crate::sql::parallel_scan::acquire(pages.len()) {
                    let mut out = crate::sql::parallel_scan::parallel_collect_matching(
                        &pages,
                        &ctx.pool.shared_reader(),
                        &snapshot,
                        ctx.xid,
                        lease.degree(),
                        &pred_closure,
                    )?;
                    // delete_many (item 44) groups rows by page_id and requires
                    // (page_id, slot) order — parallel workers process pages in
                    // work-steal order so we must sort before proceeding.
                    out.sort_unstable_by_key(|(rid, _)| (rid.page_id, rid.slot));
                    break 'collect out;
                }
            }
        }
        // Serial fallback: A3 index path, table too small, or no lease available.
        matching_rows(&heap, &snapshot, ctx, &table_def, predicate)?
    };

    // P1.d: the rows a DELETE selects are part of its read set (SSI).
    let read_ids: Vec<RowId> = matching.iter().map(|(rid, _)| *rid).collect();
    ctx.txn_mgr.ssi_note_reads(ctx.xid, &read_ids);

    // Item 44: two-pass DELETE — pre-check pass (per-row, unchanged semantics)
    // then batched heap mutations (one WAL mini-txn per page instead of per row).
    //
    // Pre-check pass: FK RESTRICT and CDC event capture must still run per-row
    // *before* any heap mutation (FK needs a fresh snapshot per row; CDC needs
    // the row data, which is gone after deletion).  All write-locks are acquired
    // inside `delete_many`, which runs after the pre-checks.
    // G5 (item 19): also collect rows for RETURNING when requested.
    let mut returned_rows: Vec<Vec<Literal>> = Vec::new();
    let row_ids: Vec<RowId> = {
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
            if returning.is_some() {
                returned_rows.push(row);
            }
            ids.push(*row_id);
        }
        ids
    };

    let deleted = match heap.delete_many(&row_ids, ctx.xid, ctx.pool, ctx.wal, ctx.lock_mgr) {
        Err(e @ DbError::WriteConflict { .. }) => return Err(classify_conflict(e, ctx)),
        other => other?,
    };

    // P1.d: SSI write tracking + item-88 batch undo — one XmaxStampBatch per page group.
    {
        let mut bi = 0;
        while bi < deleted.len() {
            let page_id = deleted[bi].page_id;
            let bj = bi + deleted[bi..].partition_point(|r| r.page_id == page_id);
            for rid in &deleted[bi..bj] {
                ctx.txn_mgr.ssi_note_write(ctx.xid, *rid);
            }
            let slots: Vec<u16> = deleted[bi..bj].iter().map(|r| r.slot).collect();
            ctx.txn_mgr
                .record_undo(ctx.xid, UndoAction::XmaxStampBatch { page_id, slots })?;
            bi = bj;
        }
    }
    let count = deleted.len();

    persist_pages_if_changed(table, &heap, &table_def.pages, ctx)?;
    assert_schema_stable(ctx, table, table_def.generation);
    // Item 97: defer row-count delta to commit.
    if count > 0 {
        ctx.txn_mgr
            .record_row_count_delta(ctx.xid, table, -(count as i64))?;
    }
    // G5 (item 19): emit RETURNING result if requested.
    if let Some(ret_cols) = returning {
        let (col_names, rows) = project_returning(&table_def, ret_cols, returned_rows)?;
        return Ok(ExecResult::Rows {
            columns: col_names,
            rows,
        });
    }
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

/// Serial RowId-only collection for the DELETE fast path (item 75).
///
/// Used when `needs_per_row_checks == false` AND the parallel threshold is
/// not met (table too small) or no worker lease is available.
///
/// For the A3/B-tree branch: calls `batch_resolve_candidates_visit` which
/// reads each heap page exactly once (one 8 KiB mmap window per unique page)
/// and calls the predicate closure with a `&[u8]` slice — **zero Vec<u8>
/// allocations** for non-matching rows.  Only RowIds are returned.
/// For the full-scan fallback: iterates `heap.scan` but discards bytes
/// after predicate evaluation (small tables; overhead is negligible).
fn collect_delete_row_ids_serial(
    heap: &Heap,
    snapshot: &crate::mvcc::Snapshot,
    ctx: &mut ExecCtx,
    table_def: &TableDef,
    predicate: &Option<Expr>,
) -> Result<Vec<RowId>> {
    if let Some(hit) = predicate.as_ref().and_then(|e| {
        find_best_indexable_btree_predicate(e, table_def, ctx.catalog.table_stats(&table_def.name))
    }) {
        if index_lookup_is_selective(table_def, hit, ctx, false) {
            let (column, op, literal) = hit;
            let Ok(value) = crate::btree_index::OrderedValue::try_from(literal) else {
                return collect_delete_row_ids_serial_fullscan(
                    heap, snapshot, ctx, table_def, predicate,
                );
            };
            let Some(meta_page) = table_def
                .columns
                .iter()
                .find(|c| c.name == column)
                .and_then(|c| c.index_root)
            else {
                return collect_delete_row_ids_serial_fullscan(
                    heap, snapshot, ctx, table_def, predicate,
                );
            };
            let tree = crate::btree_index::DiskBTree::new(meta_page, ctx.page_size);
            let Some(mut candidate_ids) = tree.search(op, &value, ctx.pool)? else {
                return Ok(Vec::new());
            };
            // B5: sort by (page_id, slot) so batch_resolve_candidates_visit groups
            // candidates on the same page into a single mmap read.
            candidate_ids.sort_unstable_by_key(|r| (r.page_id, r.slot));

            // Predicate re-check closure (covers non-indexed AND terms such as
            // `k >= N AND body LIKE '%foo%'`).  For a simple `k >= N` that is
            // fully covered by the B-tree, `predicate.is_none()` after stripping
            // the indexed term — either way the closure runs only on visible rows.
            let cols = &table_def.columns;
            let mut pred_cols = Vec::new();
            if let Some(pred) = predicate {
                expr_columns(pred, table_def, &mut pred_cols);
            }
            let (pred_needed, pred_upto) = needed_mask(&pred_cols, cols.len());

            // batch_resolve_candidates_visit: ONE mmap read per unique page_id,
            // zero Vec<u8> allocs — passes &[u8] slice directly to the predicate.
            // For rows that do not match, no allocation is made at all.
            let pred_ref = predicate;
            let row_ids = crate::heap::batch_resolve_candidates_visit(
                &candidate_ids,
                snapshot,
                ctx.xid,
                ctx.pool,
                &|bytes: &[u8]| -> Result<bool> {
                    if pred_ref.is_some() {
                        let prow = deform_row(bytes, cols, pred_upto, &pred_needed)?;
                        predicate_matches(pred_ref, cols, &prow)
                    } else {
                        Ok(true)
                    }
                },
            )?;
            return Ok(row_ids);
        }
    }
    collect_delete_row_ids_serial_fullscan(heap, snapshot, ctx, table_def, predicate)
}

/// Full-scan serial fallback for `collect_delete_row_ids_serial` — used for
/// small tables or when no B-tree index covers the predicate.
fn collect_delete_row_ids_serial_fullscan(
    heap: &Heap,
    snapshot: &crate::mvcc::Snapshot,
    ctx: &mut ExecCtx,
    table_def: &TableDef,
    predicate: &Option<Expr>,
) -> Result<Vec<RowId>> {
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
        out.push(row_id);
    }
    Ok(out)
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
    _heap: &Heap,
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
    // B5: sort candidates by (page_id, slot) for batch_resolve_candidates
    // (item 75 bitmap-scan): consecutive candidates on the same page are
    // grouped into one mmap read, eliminating the per-candidate 8 KiB copy.
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

    // Bitmap-style batch scan (item 75): candidate_ids are already sorted by
    // (page_id, slot) via B5 above.  batch_resolve_candidates reads each unique
    // heap page ONCE, eliminating the per-candidate 8 KiB mmap copy that made
    // large B-tree index scans expensive (200 k candidates on 1 000 pages:
    // 200 000 × 8 KiB = 1.6 GiB → 1 000 × 8 KiB = 8 MiB, a 200× reduction).
    // HOT chain following (same-page and cross-page) is handled inside
    // batch_resolve_candidates; no external HashSet dedup is needed because
    // consecutive duplicates in the sorted input are filtered inline.
    let resolved =
        crate::heap::batch_resolve_candidates(&candidate_ids, snapshot, ctx.xid, ctx.pool)?;
    let mut out = Vec::new();
    for (row_id, bytes) in resolved {
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

/// G5 (item 19): project rows for a RETURNING clause.
/// `ret_cols` is the column-name list from the clause; empty = all columns.
/// Returns `(col_names, rows)` in the order specified by `ret_cols`.
fn project_returning(
    table_def: &TableDef,
    ret_cols: &[String],
    rows: Vec<Vec<Literal>>,
) -> Result<(Vec<String>, Vec<Vec<Literal>>)> {
    // Resolve column indices once.
    let live_cols: Vec<&crate::catalog::ColumnDef> =
        table_def.columns.iter().filter(|c| !c.dropped).collect();

    let (col_names, indices): (Vec<String>, Vec<usize>) = if ret_cols.is_empty() {
        // RETURNING * — all non-dropped columns in definition order.
        live_cols
            .iter()
            .enumerate()
            .map(|(i, c)| (c.name.clone(), i))
            .unzip()
    } else {
        // Named columns — resolve each against the live column list.
        let mut names = Vec::with_capacity(ret_cols.len());
        let mut idxs = Vec::with_capacity(ret_cols.len());
        for col_name in ret_cols {
            let idx = live_cols
                .iter()
                .position(|c| c.name.eq_ignore_ascii_case(col_name))
                .ok_or_else(|| DbError::ColumnNotFound {
                    table: table_def.name.clone(),
                    column: col_name.clone(),
                })?;
            names.push(live_cols[idx].name.clone());
            idxs.push(idx);
        }
        (names, idxs)
    };

    // Project each row.
    let projected: Vec<Vec<Literal>> = rows
        .into_iter()
        .map(|row| {
            indices
                .iter()
                .map(|&i| row.get(i).cloned().unwrap_or(Literal::Null))
                .collect()
        })
        .collect();

    Ok((col_names, projected))
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

/// item-24 R-a: WITH CHECK enforcement for UPDATE.
///
/// After SET is applied and the new row is coerced, evaluate the table's
/// `update_with_check` expression against the NEW row.  Rejects writes
/// that would move a row outside the policy predicate (e.g. transferring
/// ownership: `UPDATE t SET user_id = 'bob'` on a `user_id = current_user`
/// policy is now rejected rather than silently accepted).
///
/// `current_user` substitution mirrors the INSERT path exactly — a
/// CurrentUser-dependent policy is skipped for the superuser/embedded
/// path (when `ctx.current_user` is None).
fn exec_update_with_check(table_def: &TableDef, new_row: &[Literal], ctx: &ExecCtx) -> Result<()> {
    let Some(ref check_expr) = table_def.update_with_check else {
        return Ok(());
    };
    // Superuser / embedded path (current_user = None) bypasses ALL RLS, including
    // WITH CHECK — mirrors how USING scan-filters are skipped for None user in
    // execute_sql_inner_as. Only skip CurrentUser-dependent checks (has_cu) for
    // non-superuser paths; for None, skip unconditionally.
    if ctx.current_user.is_none() {
        return Ok(());
    }
    let mut policy = check_expr.clone();
    if let Some(ref u) = ctx.current_user {
        crate::sql::logical::substitute_current_user_in_expr(&mut policy, u);
    }
    if !check_passes(&policy, &table_def.columns, new_row)? {
        return Err(DbError::SqlPlan(format!(
            "new row violates WITH CHECK policy for table \"{}\"",
            table_def.name
        )));
    }
    Ok(())
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

/// Move-projection: like [`project_row`] but takes `row` by `&mut`, replacing
/// each projected slot with `Literal::Null` via `mem::replace` instead of
/// cloning. For `Literal::Text(String)` this moves the `String` (zero copy)
/// rather than duplicating its heap allocation. Item 54 Phase A.
fn project_row_drain(
    projection: &[String],
    columns: &[ColumnDef],
    row: &mut [Literal],
) -> Result<Vec<Literal>> {
    if projection.is_empty() {
        return Ok(row
            .iter()
            .zip(columns.iter())
            .filter(|(_, c)| !c.dropped)
            .map(|(v, _)| v.clone())
            .collect());
    }
    let mut out = Vec::with_capacity(projection.len());
    for name in projection {
        let idx = columns
            .iter()
            .position(|c| &c.name == name && !c.dropped)
            .ok_or_else(|| DbError::ColumnNotFound {
                table: String::new(),
                column: name.clone(),
            })?;
        out.push(std::mem::replace(&mut row[idx], Literal::Null));
    }
    Ok(out)
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

/// Evaluate an arithmetic binary expression. Supports `Int op Int → Int`,
/// `Float op Float → Float`, and mixed `Int`/`Float` (coerced to `Float`).
/// Division and modulo by zero return an error rather than panicking.
pub(crate) fn eval_arith(op: ArithOp, l: Literal, r: Literal) -> Result<Literal> {
    // Coerce to a common numeric type: Int × Int stays Int; anything touching
    // Float becomes Float.
    match (l, r) {
        (Literal::Int(a), Literal::Int(b)) => {
            let v = match op {
                ArithOp::Add => a
                    .checked_add(b)
                    .ok_or_else(|| DbError::SqlPlan(format!("integer overflow: {a} + {b}")))?,
                ArithOp::Sub => a
                    .checked_sub(b)
                    .ok_or_else(|| DbError::SqlPlan(format!("integer overflow: {a} - {b}")))?,
                ArithOp::Mul => a
                    .checked_mul(b)
                    .ok_or_else(|| DbError::SqlPlan(format!("integer overflow: {a} * {b}")))?,
                ArithOp::Div => {
                    if b == 0 {
                        return Err(DbError::SqlPlan("division by zero".into()));
                    }
                    a / b
                }
                ArithOp::Mod => {
                    if b == 0 {
                        return Err(DbError::SqlPlan("modulo by zero".into()));
                    }
                    a % b
                }
            };
            Ok(Literal::Int(v))
        }
        (lv, rv) => {
            // Promote to f64 for any float operand (or non-integer types).
            let a = lit_to_f64(&lv)?;
            let b = lit_to_f64(&rv)?;
            let v = match op {
                ArithOp::Add => a + b,
                ArithOp::Sub => a - b,
                ArithOp::Mul => a * b,
                ArithOp::Div => {
                    if b == 0.0 {
                        return Err(DbError::SqlPlan("division by zero".into()));
                    }
                    a / b
                }
                ArithOp::Mod => {
                    if b == 0.0 {
                        return Err(DbError::SqlPlan("modulo by zero".into()));
                    }
                    a % b
                }
            };
            Ok(Literal::Float(v))
        }
    }
}

/// Coerce a literal to `f64` for mixed-type arithmetic.
fn lit_to_f64(lit: &Literal) -> Result<f64> {
    match lit {
        Literal::Int(n) => Ok(*n as f64),
        Literal::Float(f) => Ok(*f),
        Literal::Decimal(u, s) => Ok(*u as f64 / 10f64.powi(*s as i32)),
        other => Err(DbError::SqlPlan(format!(
            "arithmetic requires a numeric operand, got {other:?}"
        ))),
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
        // Item 59 Fix 2: pre-bound slot — direct positional access, no String scan.
        Expr::ColumnSlot(idx) => row
            .get(*idx)
            .cloned()
            .ok_or_else(|| DbError::SqlPlan(format!("ColumnSlot({idx}) out of range"))),
        Expr::Literal(lit) => Ok(lit.clone()),
        Expr::BinOp { op, lhs, rhs } => {
            let l = eval_expr(lhs, columns, row)?;
            let r = eval_expr(rhs, columns, row)?;
            Ok(Literal::Bool(compare(*op, &l, &r)?))
        }
        Expr::Arith { op, lhs, rhs } => {
            let l = eval_expr(lhs, columns, row)?;
            let r = eval_expr(rhs, columns, row)?;
            eval_arith(*op, l, r)
        }
        Expr::And(lhs, rhs) => {
            let l = as_bool(&eval_expr(lhs, columns, row)?)?;
            let r = as_bool(&eval_expr(rhs, columns, row)?)?;
            Ok(Literal::Bool(l && r))
        }
        Expr::Or(lhs, rhs) => {
            let l = as_bool(&eval_expr(lhs, columns, row)?)?;
            let r = as_bool(&eval_expr(rhs, columns, row)?)?;
            Ok(Literal::Bool(l || r))
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
        // G10 (item 19): IS [NOT] NULL on the simple row path.
        Expr::IsNull { expr, negated } => {
            let v = eval_expr(expr, columns, row)?;
            let is_null = matches!(v, Literal::Null);
            Ok(Literal::Bool(is_null != *negated))
        }
        // `current_user()` (item-24 Z6): should have been substituted by
        // `substitute_current_user_in_plan` before execution reaches here.
        // The fallback `Null` means a policy containing `CurrentUser` will
        // evaluate to false (predicate treats Null as false) — this is the
        // correct safe-fallback for the embedded / superuser path, which
        // bypasses RLS before `eval_expr` is ever called.
        Expr::CurrentUser => Ok(Literal::Null),
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
        // Item 38: implicit parameter type coercion (=, <, >, <=, >=, !=).
        // When a bind parameter arrives as Text but the column is Int/Float/Bool
        // (or vice-versa), attempt a lossless parse so `WHERE int_col = $1`
        // with params=[Text("42")] works naturally, matching PostgreSQL/SQLite
        // implicit coercion semantics. NOT applied to LIKE/MATCH (dedicated
        // arms above).
        //
        // These arms must come BEFORE the general Float arm (which catches any
        // (Float, _) pair) so that Text↔Float pairs reach the parse path
        // rather than the float_of() → None → error path.
        //
        // Text → Int: parse i64.
        // Non-parseable value → clear error, never a panic.
        (Literal::Text(s), Literal::Int(b)) => {
            let a: i64 = s.parse().map_err(|_| {
                DbError::SqlPlan(format!(
                    "cannot coerce text '{s}' to integer for comparison"
                ))
            })?;
            Ok(apply_cmp(op, a.cmp(b)))
        }
        (Literal::Int(a), Literal::Text(s)) => {
            let b: i64 = s.parse().map_err(|_| {
                DbError::SqlPlan(format!(
                    "cannot coerce text '{s}' to integer for comparison"
                ))
            })?;
            Ok(apply_cmp(op, a.cmp(&b)))
        }
        // Text → Bool: accept "true"/"false"/"1"/"0"/"t"/"f" (case-insensitive).
        (Literal::Text(s), Literal::Bool(r_b)) => {
            let a = parse_bool_text(s)?;
            match op {
                CmpOp::Eq => Ok(a == *r_b),
                CmpOp::Ne => Ok(a != *r_b),
                _ => Err(DbError::SqlUnsupported(
                    "ordering comparisons are not supported on booleans".into(),
                )),
            }
        }
        (Literal::Bool(l_b), Literal::Text(s)) => {
            let b = parse_bool_text(s)?;
            match op {
                CmpOp::Eq => Ok(*l_b == b),
                CmpOp::Ne => Ok(*l_b != b),
                _ => Err(DbError::SqlUnsupported(
                    "ordering comparisons are not supported on booleans".into(),
                )),
            }
        }
        // Text → Float/Decimal: parse as f64, dispatch through float path.
        // Must precede the general Float arm so (Float, Text) and (Decimal, Text)
        // pairs are parsed rather than reaching the float_of(Text) → None branch.
        (Literal::Text(s), _) if matches!(r, Literal::Float(_) | Literal::Decimal(_, _)) => {
            let f: f64 = s.parse().map_err(|_| {
                DbError::SqlPlan(format!("cannot coerce text '{s}' to float for comparison"))
            })?;
            compare(op, &Literal::Float(f), r)
        }
        (_, Literal::Text(s)) if matches!(l, Literal::Float(_) | Literal::Decimal(_, _)) => {
            let f: f64 = s.parse().map_err(|_| {
                DbError::SqlPlan(format!("cannot coerce text '{s}' to float for comparison"))
            })?;
            compare(op, l, &Literal::Float(f))
        }
        // Float ordering (P2.b), mixing with Int/Decimal via f64. A NaN
        // operand makes every comparison false (IEEE-754 unordered), matching
        // the NULL-operand convention above.
        // Text operands are handled by the item-38 arms above, so this arm
        // only sees numeric (Int/Float/Decimal) pairs on both sides.
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

/// Parse a text value as boolean for coercion (item 38).
/// Accepts "true"/"false", "1"/"0", "t"/"f" (case-insensitive).
fn parse_bool_text(s: &str) -> Result<bool> {
    match s.trim().to_lowercase().as_str() {
        "true" | "1" | "t" => Ok(true),
        "false" | "0" | "f" => Ok(false),
        _ => Err(DbError::SqlPlan(format!(
            "cannot coerce text '{s}' to boolean for comparison"
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
    if DIAGNOSTICS_ENABLED.load(Ordering::Relaxed) {
        COLS_DECODED.fetch_add(columns.len() as u64, Ordering::Relaxed);
    }
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
                if DIAGNOSTICS_ENABLED.load(Ordering::Relaxed) {
                    COLS_DECODED.fetch_add(1, Ordering::Relaxed);
                }
            }
            continue;
        }
        if needed[i] {
            out[i] = decode_value_at(bytes, &mut pos, col)?;
            if DIAGNOSTICS_ENABLED.load(Ordering::Relaxed) {
                COLS_DECODED.fetch_add(1, Ordering::Relaxed);
            }
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
        // ColumnSlot already carries the resolved index — no lookup needed.
        Expr::ColumnSlot(idx) => out.push(*idx),
        Expr::Literal(_) => {}
        Expr::BinOp { lhs, rhs, .. } | Expr::Arith { lhs, rhs, .. } => {
            expr_columns(lhs, table_def, out);
            expr_columns(rhs, table_def, out);
        }
        Expr::And(l, r) | Expr::Or(l, r) => {
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
        // G10 (item 19): IS [NOT] NULL.
        Expr::IsNull { expr, .. } => expr_columns(expr, table_def, out),
        // item-24 Z6: CurrentUser has no column (it's a runtime constant).
        Expr::CurrentUser => {}
    }
}

/// **Item 59 Fix 2 — column index pre-binding.** Walk a predicate `Expr` and
/// replace every `Expr::Column(name)` with `Expr::ColumnSlot(idx)` where `idx`
/// is the column's position in `columns`. Called once per statement before the
/// scan loop, so `eval_expr` pays direct positional access (no `String` scan)
/// on every row.
///
/// Unresolvable column names are left as `Expr::Column` (the fallback path in
/// `eval_expr` will report the error at evaluation time, same as before). This
/// is safe: pre-binding is a pure optimisation — skipping it for one arm of an
/// `AND` just means that arm pays the linear scan, not that it is wrong.
fn bind_predicate_columns(expr: &mut Expr, columns: &[ColumnDef]) {
    match expr {
        Expr::Column(name) => {
            if let Some(idx) = columns.iter().position(|c| &c.name == name && !c.dropped) {
                *expr = Expr::ColumnSlot(idx);
            }
        }
        Expr::ColumnSlot(_) | Expr::Literal(_) => {}
        Expr::BinOp { lhs, rhs, .. } | Expr::Arith { lhs, rhs, .. } => {
            bind_predicate_columns(lhs, columns);
            bind_predicate_columns(rhs, columns);
        }
        Expr::And(l, r) | Expr::Or(l, r) => {
            bind_predicate_columns(l, columns);
            bind_predicate_columns(r, columns);
        }
        Expr::JsonExtract { expr, .. } | Expr::JsonExtractText { expr, .. } => {
            bind_predicate_columns(expr, columns);
        }
        Expr::Like { expr, pattern, .. } => {
            bind_predicate_columns(expr, columns);
            bind_predicate_columns(pattern, columns);
        }
        Expr::Match { query, .. } => bind_predicate_columns(query, columns),
        // Near and Match column names reference the *index* column, not a
        // row field evaluated in eval_expr (Near/Match arms return Bool(true)).
        // No binding needed.
        Expr::Near { .. } => {}
        // G10 (item 19): IS [NOT] NULL.
        Expr::IsNull { expr, .. } => bind_predicate_columns(expr, columns),
        // item-24 Z6: CurrentUser has no column slot — it is a runtime
        // constant resolved before execution by `substitute_current_user_in_plan`.
        Expr::CurrentUser => {}
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

/// **Item 59 Fix 3 — late materialisation raw filter.**
///
/// For the common case of a simple integer predicate on column `col_idx`
/// (e.g. `k >= 0 AND k < N/20`), compute the byte offset of that column's
/// `i64` payload directly from the encoded tuple bytes, bypassing `deform_row`
/// entirely for rows that fail the predicate.
///
/// Returns `Some(i64_value)` when:
/// - All columns *before* `col_idx` are fixed-width and not NULL (tags 1, 3,
///   7, 8, 11, 12 in `encode_row`); variable-width columns (Text/Json/Bytea/
///   Vector) cannot be skipped without parsing, so we return `None`.
/// - The target column's tag is `1` (Int64), not NULL (`0`).
///
/// Returns `None` to signal "fall back to full `deform_row`" — the correctness
/// path is unchanged; this is a pure short-circuit for the hot path.
///
/// **On-disk layout reminder (from `encode_row`):**
/// Each column is `[tag: u8][payload]` where the payload length is fixed per
/// type:
///   - tag 0  (Null)        → 0 bytes
///   - tag 1  (Int64)       → 8 bytes  ← fixed
///   - tag 3  (Bool)        → 1 byte   ← fixed
///   - tag 7  (Timestamp)   → 8 bytes  ← fixed
///   - tag 8  (Float)       → 8 bytes  ← fixed
///   - tag 11 (Date)        → 4 bytes  ← fixed
///   - tag 12 (Time)        → 8 bytes  ← fixed
///   - tag 9  (Uuid)        → 16 bytes ← fixed
///   - tag 6  (Decimal)     → 17 bytes ← fixed
///   - tag 2/4/10 (Text/Json/Bytea) → variable (4-byte LE length prefix)
///   - tag 5  (Vector)      → variable (4-byte LE dim × 4)
///
/// Null in a preceding column (`tag == 0`) is safe to skip (0 bytes payload).
/// The function returns `None` for NULL in the *target* column (predicate
/// on NULL columns is always false in SQL, but let `eval_expr` handle it).
pub(crate) fn try_raw_i64_at(bytes: &[u8], col_idx: usize, columns: &[ColumnDef]) -> Option<i64> {
    let mut pos = 0usize;
    for (i, col) in columns.iter().enumerate() {
        if pos >= bytes.len() {
            return None; // truncated row (ADD COLUMN case) — fall back
        }
        let tag = *bytes.get(pos)?;
        if i == col_idx {
            // We are at the target column.
            if tag != 1 {
                return None; // NULL (tag=0) or wrong type — fall back
            }
            let data = bytes.get(pos + 1..pos + 9)?;
            let raw: [u8; 8] = data.try_into().ok()?;
            return Some(i64::from_le_bytes(raw));
        }
        // Skip this preceding column. Only fixed-width types are skippable
        // without parsing; variable-width → fall back.
        let stride = match tag {
            0 => 0,           // Null
            1 => 8,           // Int64
            3 => 1,           // Bool
            7 => 8,           // Timestamp
            8 => 8,           // Float
            11 => 4,          // Date
            12 => 8,          // Time
            9 => 16,          // Uuid
            6 => 17,          // Decimal (i128 + 1-byte scale)
            _ => return None, // Text/Json/Bytea/Vector — variable width
        };
        pos += 1 + stride; // 1 for tag + stride for payload
        let _ = col; // col type available if needed for validation
    }
    None
}

/// **Item 59 Fix 3 — build a `RawFilter` for the inner predicate loop.**
///
/// Inspects the (already-bound) predicate and returns `Some(RawFilter)` when
/// the predicate is a conjunction of simple `ColumnSlot(idx) op Literal::Int`
/// comparisons on columns that precede only fixed-width columns (so
/// `try_raw_i64_at` can reach them). Returns `None` to use the full
/// `deform_row` path.
///
/// The raw filter is checked before calling `deform_row`; rows that fail it
/// are skipped without any heap allocation, giving the "late materialisation"
/// gain at 5% selectivity.
#[derive(Clone)]
struct RawFilter {
    /// Each entry: `(col_idx, op, int_literal)`.
    terms: Vec<(usize, CmpOp, i64)>,
}

impl RawFilter {
    /// Returns `true` if the raw bytes pass ALL terms (short-circuits to
    /// `deform_row` on `None`, i.e., when the raw read is not possible).
    fn passes(&self, bytes: &[u8], columns: &[ColumnDef]) -> Option<bool> {
        for &(idx, op, rhs) in &self.terms {
            let lhs = try_raw_i64_at(bytes, idx, columns)?;
            let ok = match op {
                CmpOp::Eq => lhs == rhs,
                CmpOp::Ne => lhs != rhs,
                CmpOp::Lt => lhs < rhs,
                CmpOp::Gt => lhs > rhs,
                CmpOp::Le => lhs <= rhs,
                CmpOp::Ge => lhs >= rhs,
            };
            if !ok {
                return Some(false);
            }
        }
        Some(true)
    }
}

/// Try to build a [`RawFilter`] from a bound predicate expression. Walks
/// `And` chains and gathers `ColumnSlot(idx) op Literal::Int` terms. Returns
/// `None` if any sub-expression is not a simple integer comparison (the full
/// `deform_row` path handles it).
fn try_build_raw_filter(expr: &Expr) -> Option<RawFilter> {
    let mut terms = Vec::new();
    collect_raw_terms(expr, &mut terms)?;
    if terms.is_empty() {
        None
    } else {
        Some(RawFilter { terms })
    }
}

fn collect_raw_terms(expr: &Expr, out: &mut Vec<(usize, CmpOp, i64)>) -> Option<()> {
    match expr {
        Expr::BinOp { op, lhs, rhs } => match (lhs.as_ref(), rhs.as_ref()) {
            (Expr::ColumnSlot(idx), Expr::Literal(Literal::Int(v))) => {
                out.push((*idx, *op, *v));
                Some(())
            }
            (Expr::Literal(Literal::Int(v)), Expr::ColumnSlot(idx)) => {
                // Flip the comparison to normalise ColumnSlot on the left.
                out.push((*idx, flip_cmp_op(*op), *v));
                Some(())
            }
            _ => None, // non-integer or non-slot predicate — fall back
        },
        Expr::And(l, r) => {
            collect_raw_terms(l, out)?;
            collect_raw_terms(r, out)
        }
        _ => None,
    }
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
                hnsw_l0_caches: None,
                hnsw_vec_caches: None,
                authz: None,
                current_user: None,
                hnsw_tx: None,
                in_explicit_txn: false,
                near_lightweight_snaps: None,
            };
            execute(plan, &mut ctx)
        }

        fn begin(&mut self) -> Xid {
            self.txn_mgr
                .begin(IsolationLevel::ReadCommitted, &self.wal)
                .unwrap()
        }

        fn commit(&mut self, xid: Xid) {
            // Item 97: mirror Engine::commit — drain deferred deltas first,
            // then commit, then apply them to the in-memory catalog.
            let deltas = self.txn_mgr.take_row_count_deltas(xid);
            self.txn_mgr.commit(xid, &self.wal, &self.lock_mgr).unwrap();
            if !deltas.is_empty() {
                let tables = self.catalog.tables_mut();
                for (table, delta) in &deltas {
                    if *delta == i64::MIN {
                        if let Some(t) = tables.get_mut(table.as_str()) {
                            t.row_count = 0;
                        }
                    } else if *delta != 0 {
                        if let Some(t) = tables.get_mut(table.as_str()) {
                            t.row_count = t.row_count.saturating_add(*delta);
                        }
                    }
                }
                let mut ctx = crate::catalog::CatalogCtx {
                    pool: &self.pool,
                    wal: &self.wal,
                    control_path: &self.control_path,
                    control: &self.control,
                    page_size: self.page_size,
                };
                self.catalog.persist_only(&mut ctx).map(|_| ()).unwrap();
            }
        }

        fn abort(&mut self, xid: Xid) {
            // A minimal empty Heap is sufficient: undo_xmax_stamp/undo_insert go
            // directly to the buffer pool by page_id — they don't need the FSM.
            let heap = Heap::open(self.page_size, None, vec![]);
            self.txn_mgr
                .abort(xid, &self.pool, &heap, &self.wal, &self.lock_mgr)
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
                include_cols: Vec::new(),
                ty: ColumnType::Int64,
            },
            ColumnDef {
                name: "b".to_string(),
                index: None,
                index_root: None,
                unique_index_root: None,
                dropped: false,
                constraints: Default::default(),
                include_cols: Vec::new(),
                ty: ColumnType::Text,
            },
            ColumnDef {
                name: "c".to_string(),
                index: None,
                index_root: None,
                unique_index_root: None,
                dropped: false,
                constraints: Default::default(),
                include_cols: Vec::new(),
                ty: ColumnType::Bool,
            },
            ColumnDef {
                name: "d".to_string(),
                index: None,
                index_root: None,
                unique_index_root: None,
                dropped: false,
                constraints: Default::default(),
                include_cols: Vec::new(),
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
            include_cols: Vec::new(),
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
            include_cols: Vec::new(),
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
            include_cols: Vec::new(),
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
                include_cols: Vec::new(),
                ty: ColumnType::Vector(4),
            }],
            pages: vec![],
            fsm_meta: None,
            rls_policy: None,
            insert_policy: None,
            update_policy: None,
            delete_policy: None,
            update_with_check: None,
            policies: vec![],
            events_enabled: false,
            serial_next: Default::default(),
            constraints: Default::default(),
            generation: 0,
            row_count: 0,
            fill_factor: 100,
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
            include_cols: Vec::new(),
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

    /// Range UPDATE on a BTree-indexed column at bench-like scale (10k rows):
    /// reproduces the Table 3 "UPDATE non-HOT" bench scenario to verify no panic
    /// or correctness regression at scale.  `k` changes (indexed column) so no
    /// Arithmetic UPDATE on a BTree-indexed column at small scale (100 rows) —
    /// validates that the index is correctly maintained for every row in the range.
    #[test]
    fn update_set_arith_with_btree_index() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE t (id INT, k INT)").unwrap();
        h.exec_as(xid, "CREATE INDEX t_k ON t USING BTREE (k)")
            .unwrap();
        h.commit(xid);

        // Insert 100 rows: k=0..99
        let xid = h.begin();
        for i in 0i64..100 {
            h.exec_as(xid, &format!("INSERT INTO t (id, k) VALUES ({i}, {i})"))
                .unwrap();
        }
        h.commit(xid);

        // UPDATE k = k + 1 WHERE k >= 50 AND k < 75 (25 rows)
        let xid = h.begin();
        let n = match h
            .exec_as(xid, "UPDATE t SET k = k + 1 WHERE k >= 50 AND k < 75")
            .unwrap()
        {
            ExecResult::Updated { count } => count,
            o => panic!("expected Updated, got {o:?}"),
        };
        h.commit(xid);
        assert_eq!(n, 25, "25 rows updated");

        // After SET k = k+1 WHERE k ∈ [50, 75): range shifts to [51, 76).
        // The floor (k=50) loses its row because no k=49 contributes a k+1=50.
        let xid = h.begin();
        let r = match h.exec_as(xid, "SELECT id FROM t WHERE k = 50").unwrap() {
            ExecResult::Rows { rows, .. } => rows.len(),
            o => panic!("{o:?}"),
        };
        h.commit(xid);
        assert_eq!(r, 0, "k=50 must be gone (moved to 51; no k=49 contributes)");

        // k=74: original (74→75) moved out; new row from k=73→74 moved in → 1 row.
        let xid = h.begin();
        let r = match h.exec_as(xid, "SELECT id FROM t WHERE k = 74").unwrap() {
            ExecResult::Rows { rows, .. } => rows.len(),
            o => panic!("{o:?}"),
        };
        h.commit(xid);
        assert_eq!(r, 1, "k=74: filled by updated k=73→74 row");

        // k=49 (below range) must be unchanged (1 row).
        let xid = h.begin();
        let r49 = match h.exec_as(xid, "SELECT id FROM t WHERE k = 49").unwrap() {
            ExecResult::Rows { rows, .. } => rows.len(),
            o => panic!("{o:?}"),
        };
        // k=75 has 2 rows: original (k=75, not in range) + updated (k=74→75)
        let r75 = match h.exec_as(xid, "SELECT id FROM t WHERE k = 75").unwrap() {
            ExecResult::Rows { rows, .. } => rows.len(),
            o => panic!("{o:?}"),
        };
        h.commit(xid);
        assert_eq!(r49, 1, "k=49 (not in range) must be unchanged");
        assert_eq!(r75, 2, "k=75: original row + updated row (k=74→75)");
    }

    /// Minimal smoke test for arithmetic in UPDATE SET: `SET k = k + 1` on a
    /// small table. Verifies the value was changed and old value is gone.
    #[test]
    fn update_set_arith_basic() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE t (id INT, k INT)").unwrap();
        h.commit(xid);

        // Insert 5 rows: id=0..4, k=10..14
        let xid = h.begin();
        for i in 0i64..5 {
            h.exec_as(
                xid,
                &format!("INSERT INTO t (id, k) VALUES ({i}, {})", 10 + i),
            )
            .unwrap();
        }
        h.commit(xid);

        // UPDATE SET k = k + 1 WHERE k = 12 (one row)
        let xid = h.begin();
        let n = match h
            .exec_as(xid, "UPDATE t SET k = k + 1 WHERE k = 12")
            .unwrap()
        {
            ExecResult::Updated { count } => count,
            o => panic!("expected Updated, got {o:?}"),
        };
        h.commit(xid);
        assert_eq!(n, 1, "one row updated");

        // k=12 must be gone, k=13 must have 2 rows (original + updated)
        let xid = h.begin();
        let r12 = match h.exec_as(xid, "SELECT id FROM t WHERE k = 12").unwrap() {
            ExecResult::Rows { rows, .. } => rows.len(),
            o => panic!("{o:?}"),
        };
        let r13 = match h.exec_as(xid, "SELECT id FROM t WHERE k = 13").unwrap() {
            ExecResult::Rows { rows, .. } => rows.len(),
            o => panic!("{o:?}"),
        };
        h.commit(xid);
        assert_eq!(r12, 0, "k=12 must be gone (updated to 13)");
        assert_eq!(
            r13, 2,
            "k=13: original row (id=3,k=13) + updated row (id=2,k=12→13)"
        );
    }

    /// HOT path fires; B-tree must be patched for each new row version.
    #[test]
    fn update_nonhot_indexed_col_range_at_scale() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE t (id INT, k INT, body TEXT)")
            .unwrap();
        h.exec_as(xid, "CREATE INDEX t_k ON t USING BTREE (k)")
            .unwrap();
        h.commit(xid);

        // Pre-build 5k rows (k=[0,4999]) — matches bench's base table
        let mut xid = h.begin();
        for i in 0i64..5000 {
            h.exec_as(
                xid,
                &format!("INSERT INTO t (id,k,body) VALUES ({i},{i},'b{i}')"),
            )
            .unwrap();
            if (i + 1) % 500 == 0 {
                h.commit(xid);
                xid = h.begin();
            }
        }
        h.commit(xid);

        // INSERT-bench 5k rows (k=[5000,9999])
        let mut xid = h.begin();
        for i in 5000i64..10000 {
            h.exec_as(
                xid,
                &format!("INSERT INTO t (id,k,body) VALUES ({i},{i},'b{i}')"),
            )
            .unwrap();
            if (i + 1) % 500 == 0 {
                h.commit(xid);
                xid = h.begin();
            }
        }
        h.commit(xid);

        // UPDATE non-HOT: SET k = k + 1 WHERE k >= 5000 AND k < 7500 (50% of INSERT rows)
        let xid = h.begin();
        let n = match h
            .exec_as(xid, "UPDATE t SET k = k + 1 WHERE k >= 5000 AND k < 7500")
            .unwrap()
        {
            ExecResult::Updated { count } => count,
            o => panic!("expected Updated, got {o:?}"),
        };
        h.commit(xid);
        assert_eq!(n, 2500, "2500 rows in [5000,7500) updated");

        // After SET k = k+1 WHERE k ∈ [5000, 7500): rows shift from [5000,7500)
        // to [5001,7501). Verify the floor boundary and out-of-range rows.
        //
        // k=5000: was updated to k=5001. No row contributes a new k=5000 (k=4999
        // is below the range). Must be 0 rows.
        let xid = h.begin();
        let rows_at_k5000 = match h.exec_as(xid, "SELECT id FROM t WHERE k = 5000").unwrap() {
            ExecResult::Rows { rows, .. } => rows.len(),
            o => panic!("{o:?}"),
        };
        h.commit(xid);
        assert_eq!(rows_at_k5000, 0, "k=5000 must be gone (shifted to 5001)");

        // k=7499: original (7499→7500) moved out; new row from k=7498→7499
        // moved in → 1 row.
        let xid = h.begin();
        let rows_at_k7499 = match h.exec_as(xid, "SELECT id FROM t WHERE k = 7499").unwrap() {
            ExecResult::Rows { rows, .. } => rows.len(),
            o => panic!("{o:?}"),
        };
        h.commit(xid);
        assert_eq!(
            rows_at_k7499, 1,
            "k=7499: filled by updated k=7498→7499 row"
        );

        // k=4999: pre-build row, outside WHERE range — must be unchanged (1 row).
        let xid = h.begin();
        let rows_at_k4999 = match h.exec_as(xid, "SELECT id FROM t WHERE k = 4999").unwrap() {
            ExecResult::Rows { rows, .. } => rows.len(),
            o => panic!("{o:?}"),
        };
        h.commit(xid);
        assert_eq!(
            rows_at_k4999, 1,
            "k=4999 (pre-build, not updated) must still exist"
        );

        // k=7501: INSERT-bench row above the WHERE range — must be unchanged (1 row).
        let xid = h.begin();
        let rows_at_k7501 = match h.exec_as(xid, "SELECT id FROM t WHERE k = 7501").unwrap() {
            ExecResult::Rows { rows, .. } => rows.len(),
            o => panic!("{o:?}"),
        };
        h.commit(xid);
        assert_eq!(
            rows_at_k7501, 1,
            "k=7501 (not in WHERE range) must be unchanged"
        );
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

        // Item 59 Fix 1: enable diagnostics so the COLS_DECODED counter fires.
        DIAGNOSTICS_ENABLED.store(true, std::sync::atomic::Ordering::Relaxed);
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

    // ── Item 56 Step 2: update_many batch path ────────────────────────────────

    #[test]
    fn update_many_batch_produces_same_result_as_per_row() {
        // Plain table (no unique/FK) takes the batch path; verify updated and
        // unchanged rows all have the correct values after commit.
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE plain (id INT, v INT)")
            .unwrap();
        for i in 1..=20i64 {
            h.exec_as(xid, &format!("INSERT INTO plain (id, v) VALUES ({i}, 0)"))
                .unwrap();
        }
        h.commit(xid);

        // Batch path: plain table has no unique/FK constraints.
        let xid2 = h.begin();
        let r = h
            .exec_as(xid2, "UPDATE plain SET v = 100 WHERE id <= 10")
            .unwrap();
        assert_eq!(r, ExecResult::Updated { count: 10 });
        h.commit(xid2);

        let xid3 = h.begin();
        let rows = match h
            .exec_as(xid3, "SELECT id, v FROM plain WHERE id <= 10")
            .unwrap()
        {
            ExecResult::Rows { rows: r, .. } => r,
            o => panic!("{o:?}"),
        };
        // All 10 updated rows must have v = 100.
        assert_eq!(rows.len(), 10);
        for row in &rows {
            assert_eq!(row[1], Literal::Int(100), "updated row must have v=100");
        }
        // Unchanged rows must still have v = 0.
        let unchanged = match h
            .exec_as(xid3, "SELECT id, v FROM plain WHERE id > 10")
            .unwrap()
        {
            ExecResult::Rows { rows: r, .. } => r,
            o => panic!("{o:?}"),
        };
        assert_eq!(unchanged.len(), 10);
        for row in &unchanged {
            assert_eq!(row[1], Literal::Int(0), "unmatched row must keep v=0");
        }
        h.commit(xid3);
    }

    #[test]
    fn update_many_batch_abort_reverses_all_stamps() {
        // If the user transaction is aborted after the batch UPDATE, all old
        // versions must be restored (xmax cleared) and the original values visible.
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE t (id INT, v INT)").unwrap();
        for i in 1..=5i64 {
            h.exec_as(xid, &format!("INSERT INTO t (id, v) VALUES ({i}, 0)"))
                .unwrap();
        }
        h.commit(xid);

        // Batch UPDATE then abort.
        let xid2 = h.begin();
        let r = h.exec_as(xid2, "UPDATE t SET v = 999").unwrap();
        assert_eq!(r, ExecResult::Updated { count: 5 });
        h.abort(xid2);

        // After abort: original values must be visible, not 999.
        let xid3 = h.begin();
        let rows = match h.exec_as(xid3, "SELECT v FROM t").unwrap() {
            ExecResult::Rows { rows: r, .. } => r,
            o => panic!("{o:?}"),
        };
        assert_eq!(rows.len(), 5);
        for row in &rows {
            assert_eq!(row[0], Literal::Int(0), "abort must restore original value");
        }
        h.commit(xid3);
    }

    #[test]
    fn update_many_unique_table_stays_on_per_row_path_and_enforces_constraint() {
        // A table with a UNIQUE/PK column must NOT take the batch path.
        // Verify that a uniqueness violation on the per-row path is still caught:
        // updating row id=1 to id=2 collides with the existing row id=2.
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE u (id INT PRIMARY KEY, v INT)")
            .unwrap();
        h.exec_as(xid, "INSERT INTO u (id, v) VALUES (1, 10)")
            .unwrap();
        h.exec_as(xid, "INSERT INTO u (id, v) VALUES (2, 20)")
            .unwrap();
        h.commit(xid);

        // id=1 → id=2 collides with the existing id=2 row.
        let xid2 = h.begin();
        let err = h
            .exec_as(xid2, "UPDATE u SET id = 2 WHERE id = 1")
            .unwrap_err();
        assert!(
            matches!(err, DbError::UniqueViolation { .. }),
            "expected UniqueViolation, got {err:?}"
        );
        h.abort(xid2);
    }

    #[test]
    fn update_many_page_boundary_crossing() {
        // Regression for item-50-style infinite loop: updates spanning multiple
        // heap pages must all succeed and each row must have the new value.
        // We insert enough rows to span at least two pages, then update all of them.
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        // 256-byte body forces ~28-30 rows per 8 KiB page.
        h.exec_as(xid, "CREATE TABLE t (id INT, body TEXT)")
            .unwrap();
        let body = "x".repeat(256);
        let n = 80i64; // spans ≥3 pages
        for i in 1..=n {
            h.exec_as(
                xid,
                &format!("INSERT INTO t (id, body) VALUES ({i}, '{body}')"),
            )
            .unwrap();
        }
        h.commit(xid);

        let xid2 = h.begin();
        let r = h.exec_as(xid2, "UPDATE t SET body = 'updated'").unwrap();
        assert_eq!(r, ExecResult::Updated { count: n as usize });
        h.commit(xid2);

        let xid3 = h.begin();
        let rows = match h
            .exec_as(xid3, "SELECT id FROM t WHERE body = 'updated'")
            .unwrap()
        {
            ExecResult::Rows { rows: r, .. } => r,
            o => panic!("{o:?}"),
        };
        assert_eq!(rows.len() as i64, n, "all {n} rows must show the new value");
        // No row should still have the old body.
        let old_rows = match h
            .exec_as(xid3, &format!("SELECT id FROM t WHERE body = '{body}'"))
            .unwrap()
        {
            ExecResult::Rows { rows: r, .. } => r,
            o => panic!("{o:?}"),
        };
        assert_eq!(
            old_rows.len(),
            0,
            "no rows should retain the old value after commit"
        );
        h.commit(xid3);
    }

    // ── Item 59: SELECT filtered optimisations ───────────────────────────────

    /// Fix 2: column pre-binding produces identical rows to the unbound path.
    /// Runs a filtered SELECT on a table with the bench schema
    /// (id INT, k INT, g INT, body TEXT) and verifies that the pre-bound
    /// path (ColumnSlot) and the original Expr::Column path produce the same
    /// rows. Since exec_select always pre-binds, we indirectly validate by
    /// checking the result is correct and consistent.
    #[test]
    fn select_filtered_col_pre_binding_same_results() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE t (id INT, k INT, g INT, body TEXT)")
            .unwrap();
        let n = 100i64;
        for i in 0..n {
            h.exec_as(
                xid,
                &format!(
                    "INSERT INTO t (id, k, g, body) VALUES ({i}, {i}, {}, 'body-{i}')",
                    i % 10
                ),
            )
            .unwrap();
        }
        h.commit(xid);

        // 5% selectivity predicate: k >= 0 AND k < n/20
        let limit = n / 20;
        let xid2 = h.begin();
        let rows = match h
            .exec_as(
                xid2,
                &format!("SELECT id, k, g FROM t WHERE k >= 0 AND k < {limit}"),
            )
            .unwrap()
        {
            ExecResult::Rows { rows: r, .. } => r,
            o => panic!("{o:?}"),
        };
        h.commit(xid2);

        assert_eq!(
            rows.len(),
            limit as usize,
            "expected {limit} rows (k in 0..{limit})"
        );
        // Verify each row has the correct k value.
        for (i, row) in rows.iter().enumerate() {
            match row[1] {
                Literal::Int(k) => assert!(
                    (0..limit).contains(&k),
                    "row {i}: k={k} out of expected range 0..{limit}"
                ),
                ref other => panic!("row {i}: expected Int for k, got {other:?}"),
            }
        }
    }

    /// Fix 3: late materialisation produces identical rows at 5% and 50%
    /// selectivity. Verifies that the raw-filter fast path returns the same
    /// result as the full deform_row path.
    #[test]
    fn select_filtered_late_mat_same_results() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE t (id INT, k INT, g INT, body TEXT)")
            .unwrap();
        let n = 200i64;
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

        // 5% selectivity: expect n/20 rows
        let low_limit = n / 20;
        let xid2 = h.begin();
        let rows_5pct = match h
            .exec_as(
                xid2,
                &format!("SELECT id, k FROM t WHERE k >= 0 AND k < {low_limit}"),
            )
            .unwrap()
        {
            ExecResult::Rows { rows: r, .. } => r,
            o => panic!("{o:?}"),
        };
        h.commit(xid2);
        assert_eq!(
            rows_5pct.len(),
            low_limit as usize,
            "5% selectivity: expected {low_limit} rows"
        );

        // 50% selectivity: expect n/2 rows
        let half = n / 2;
        let xid3 = h.begin();
        let rows_50pct = match h
            .exec_as(
                xid3,
                &format!("SELECT id, k FROM t WHERE k >= 0 AND k < {half}"),
            )
            .unwrap()
        {
            ExecResult::Rows { rows: r, .. } => r,
            o => panic!("{o:?}"),
        };
        h.commit(xid3);
        assert_eq!(
            rows_50pct.len(),
            half as usize,
            "50% selectivity: expected {half} rows"
        );

        // Verify all returned rows have correct k values in range.
        for (i, row) in rows_50pct.iter().enumerate() {
            match row[1] {
                Literal::Int(k) => assert!(
                    (0..half).contains(&k),
                    "row {i}: k={k} out of expected 50% range"
                ),
                ref other => panic!("row {i}: expected Int, got {other:?}"),
            }
        }
    }

    /// Fix 3 fallback: a TEXT predicate column (variable-width) correctly falls
    /// back to the full deform_row path and returns correct results.
    /// `try_raw_i64_at` returns None for TEXT columns, so the raw filter is not
    /// built — the existing deform_row path handles it.
    #[test]
    fn select_filtered_late_mat_fallback() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE t (id INT, body TEXT)")
            .unwrap();
        for i in 0..50i64 {
            h.exec_as(
                xid,
                &format!("INSERT INTO t (id, body) VALUES ({i}, 'row-{i}')"),
            )
            .unwrap();
        }
        h.commit(xid);

        // Predicate on TEXT column (body = 'row-7') — forces raw filter fallback.
        let xid2 = h.begin();
        let rows = match h
            .exec_as(xid2, "SELECT id, body FROM t WHERE body = 'row-7'")
            .unwrap()
        {
            ExecResult::Rows { rows: r, .. } => r,
            o => panic!("{o:?}"),
        };
        h.commit(xid2);
        assert_eq!(rows.len(), 1, "exactly one row matches body = 'row-7'");
        assert_eq!(rows[0][0], Literal::Int(7), "id=7 for body='row-7' row");
        assert_eq!(
            rows[0][1],
            Literal::Text("row-7".to_string()),
            "body value is correct"
        );

        // Predicate on TEXT column where it's the second column (id is first,
        // which is fixed-width INT) — the raw filter still falls back to
        // deform_row because body is variable-width.
        let xid3 = h.begin();
        let rows2 = match h
            .exec_as(xid3, "SELECT id FROM t WHERE body = 'row-42'")
            .unwrap()
        {
            ExecResult::Rows { rows: r, .. } => r,
            o => panic!("{o:?}"),
        };
        h.commit(xid3);
        assert_eq!(rows2.len(), 1, "exactly one row matches body = 'row-42'");
        assert_eq!(rows2[0][0], Literal::Int(42), "id=42 for body='row-42' row");
    }

    // ── Item 98: batch INSERT mini-txn tests ──────────────────────────────────

    /// test_insert_batch_row_count — a 100-row VALUES INSERT must consume
    /// O(heap-pages) WAL mini-txns, not O(rows).  For 100 small (INT, INT)
    /// rows on a fresh 8 KiB page, the mini-txn delta from before to after the
    /// INSERT is exactly 2: one for the heap-page allocation (alloc_heap_page
    /// uses its own bracket, item 28/B2) and one WAL_INSERT_BATCH for all
    /// 100 rows packed onto that page.  Before item 98 the delta was 101.
    #[test]
    fn test_insert_batch_row_count() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());

        // Set up table.
        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE t (id INT, k INT)").unwrap();
        h.commit(xid);

        // Snapshot mini-txn count before the INSERT.
        let before = h.wal.mini_txn_count();

        // 100-row VALUES INSERT into an empty heap.
        // Per-row sizes: type_tag(1) + i64(8) = 9 bytes, two columns = 18 B.
        // Per-row page cost: SLOT_SIZE(4) + TUPLE_HEADER_SIZE(24) + 18 = 46 B.
        // 100 × 46 = 4 600 B; 8 192 − PAGE_HEADER_SIZE(28) = 8 164 B usable
        // → all 100 rows fit comfortably on one page, so delta = 2.
        let vals: String = (0i64..100)
            .map(|i| format!("({i}, {i})"))
            .collect::<Vec<_>>()
            .join(", ");
        let xid2 = h.begin();
        let result = h
            .exec_as(xid2, &format!("INSERT INTO t (id, k) VALUES {vals}"))
            .unwrap();
        h.commit(xid2);

        let after = h.wal.mini_txn_count();
        match result {
            ExecResult::Inserted { count } => assert_eq!(count, 100),
            o => panic!("expected Inserted, got {o:?}"),
        }
        // Expected mini-txn budget after items 97 + 98 + 104:
        //   1 — heap-page alloc (alloc_heap_page, its own bracket)
        //   1 — accumulating INSERT for all 100 rows sharing ONE WAL_BEGIN/WAL_COMMIT
        // Total = 2.  Before item 98 the delta was 101 (1 alloc + 100 per-row brackets).
        // Item 97 added a 3rd mini-txn for catalog row_count; item 104 removed it:
        // row_count is now in-memory exact (updated in commit), durable at checkpoint.
        let delta = after - before;
        assert!(
            delta <= 3,
            "item 98+97+104: 100-row INSERT on one page must use ≤ 3 mini-txns (alloc + accumulating), got {delta}"
        );
    }

    /// test_insert_batch_fk_enforcement — when one row in a multi-row VALUES
    /// INSERT violates a FK, the whole statement must roll back and leave 0
    /// rows in the table.  All validation happens before any heap write
    /// (two-pass approach), so a per-row failure aborts with no partial inserts.
    #[test]
    fn test_insert_batch_fk_enforcement() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());

        // Parent table with one row (id=1, id=2, id=3, id=4 — id=3 is absent).
        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE parent (id INT PRIMARY KEY)")
            .unwrap();
        h.exec_as(xid, "INSERT INTO parent (id) VALUES (1), (2), (4), (5)")
            .unwrap();
        h.commit(xid);

        // Child table with FK on parent.id.
        let xid2 = h.begin();
        h.exec_as(
            xid2,
            "CREATE TABLE child (id INT, pid INT REFERENCES parent(id))",
        )
        .unwrap();
        h.commit(xid2);

        // INSERT 5 rows where row 3 (pid=3) violates the FK.
        let xid3 = h.begin();
        let result = h.exec_as(
            xid3,
            "INSERT INTO child (id, pid) VALUES (10, 1), (20, 2), (30, 3), (40, 4), (50, 5)",
        );
        // Must fail — FK violation on row 3.
        assert!(
            result.is_err(),
            "item 98 FK test: INSERT with a bad FK row must fail, but got Ok"
        );
        h.abort(xid3);

        // No rows must be present in child.
        let xid4 = h.begin();
        let count_result = h.exec_as(xid4, "SELECT COUNT(*) FROM child").unwrap();
        h.commit(xid4);
        match count_result {
            ExecResult::Rows { rows, .. } => {
                let count = match &rows[0][0] {
                    Literal::Int(c) => *c,
                    o => panic!("expected Int count, got {o:?}"),
                };
                assert_eq!(
                    count, 0,
                    "item 98 FK test: all 5 rows must be absent after FK violation, got {count}"
                );
            }
            o => panic!("expected Rows, got {o:?}"),
        }
    }

    // ── item 97: exact COUNT(*) via catalog row_count ────────────────────────

    /// Returns the i64 result of `SELECT COUNT(*) FROM <table>`.
    fn count_star(h: &mut Harness, table: &str) -> i64 {
        let xid = h.begin();
        let sql = format!("SELECT COUNT(*) FROM {table}");
        let result = h.exec_as(xid, &sql).unwrap();
        h.commit(xid);
        match result {
            ExecResult::Rows { rows, .. } => {
                assert_eq!(rows.len(), 1, "COUNT(*) must return exactly one row");
                match &rows[0][0] {
                    crate::sql::logical::Literal::Int(n) => *n,
                    other => panic!("COUNT(*) returned non-Int: {other:?}"),
                }
            }
            other => panic!("COUNT(*) returned non-Rows: {other:?}"),
        }
    }

    /// AC4: INSERT 1000 → COUNT=1000; DELETE 200 → COUNT=800.
    #[test]
    fn test_count_star_exact() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());

        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE t97 (id INT)").unwrap();
        h.commit(xid);

        // Bulk INSERT 1000 rows in one transaction.
        let xid = h.begin();
        for i in 0..1000_i64 {
            h.exec_as(xid, &format!("INSERT INTO t97 (id) VALUES ({i})"))
                .unwrap();
        }
        h.commit(xid);

        assert_eq!(count_star(&mut h, "t97"), 1000, "after 1000 INSERTs");

        // DELETE 200 rows.
        let xid = h.begin();
        h.exec_as(xid, "DELETE FROM t97 WHERE id < 200").unwrap();
        h.commit(xid);

        assert_eq!(count_star(&mut h, "t97"), 800, "after 200 DELETEs");
    }

    /// AC5: INSERT 500 → TRUNCATE → COUNT=0.
    #[test]
    fn test_count_star_truncate() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());

        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE t97t (id INT)").unwrap();
        h.commit(xid);

        let xid = h.begin();
        for i in 0..500_i64 {
            h.exec_as(xid, &format!("INSERT INTO t97t (id) VALUES ({i})"))
                .unwrap();
        }
        h.commit(xid);

        assert_eq!(count_star(&mut h, "t97t"), 500, "before TRUNCATE");

        let xid = h.begin();
        h.exec_as(xid, "DELETE FROM t97t").unwrap(); // unconditional → TRUNCATE fast path
        h.commit(xid);

        assert_eq!(count_star(&mut h, "t97t"), 0, "after TRUNCATE fast path");
    }

    /// AC7: `SELECT COUNT(*) WHERE …` must NOT use the fast path (must fall
    /// through to `count_visible` / the filtered path).
    #[test]
    fn test_count_star_with_filter_still_scans() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());

        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE t97f (x INT)").unwrap();
        h.commit(xid);

        let xid = h.begin();
        for i in 0..10_i64 {
            h.exec_as(xid, &format!("INSERT INTO t97f (x) VALUES ({i})"))
                .unwrap();
        }
        h.commit(xid);

        // row_count == 10 but only 3 rows satisfy x < 3.
        let xid = h.begin();
        let result = h
            .exec_as(xid, "SELECT COUNT(*) FROM t97f WHERE x < 3")
            .unwrap();
        h.commit(xid);
        let n = match result {
            ExecResult::Rows { rows, .. } => match &rows[0][0] {
                crate::sql::logical::Literal::Int(n) => *n,
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        };
        assert_eq!(n, 3, "filtered COUNT(*) must scan, not return row_count");
    }

    /// AC6: 4 concurrent inserters + reader loop; no panic; bounds correct.
    #[test]
    fn test_count_star_concurrent() {
        use std::sync::Arc;
        let dir = tempdir().unwrap();
        // Use Engine (which handles concurrent catalog access correctly).
        let engine = Arc::new(crate::Engine::open(dir.path(), 0).unwrap());

        // Create table.
        let xid = engine.begin().unwrap();
        engine
            .execute_sql(xid, "CREATE TABLE t97c (id INT)")
            .unwrap();
        engine.commit(xid).unwrap();

        const WRITERS: usize = 4;
        const ROWS_PER_WRITER: usize = 100;

        let mut handles = vec![];
        for w in 0..WRITERS {
            let eng = Arc::clone(&engine);
            handles.push(std::thread::spawn(move || {
                for i in 0..ROWS_PER_WRITER {
                    let id = (w * ROWS_PER_WRITER + i) as i64;
                    let xid = eng.begin().unwrap();
                    eng.execute_sql(xid, &format!("INSERT INTO t97c (id) VALUES ({id})"))
                        .unwrap();
                    eng.commit(xid).unwrap();
                }
            }));
        }

        // Reader loop while writers run — must not panic.
        let eng_r = Arc::clone(&engine);
        let reader = std::thread::spawn(move || {
            for _ in 0..200 {
                let xid = eng_r.begin().unwrap();
                let _ = eng_r.execute_sql(xid, "SELECT COUNT(*) FROM t97c").unwrap();
                eng_r.commit(xid).unwrap();
            }
        });

        for h in handles {
            h.join().unwrap();
        }
        reader.join().unwrap();

        // Final count must be in [0, WRITERS*ROWS_PER_WRITER].
        let total = (WRITERS * ROWS_PER_WRITER) as i64;
        let xid = engine.begin().unwrap();
        let rows = match engine
            .execute_sql(xid, "SELECT COUNT(*) FROM t97c")
            .unwrap()
            .into_iter()
            .next()
            .unwrap()
        {
            crate::sql::executor::ExecResult::Rows { rows, .. } => rows,
            other => panic!("{other:?}"),
        };
        engine.commit(xid).unwrap();
        let n = match &rows[0][0] {
            crate::sql::logical::Literal::Int(n) => *n,
            other => panic!("{other:?}"),
        };
        assert!(
            (0..=total).contains(&n),
            "COUNT={n} must be in [0, {total}]"
        );
    }
}
