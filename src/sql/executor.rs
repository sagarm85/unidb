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
    bufferpool::BufferPool,
    catalog::{Catalog, CatalogCtx, ColumnDef, ColumnType, IndexKind, TableDef},
    control::ControlData,
    error::{DbError, Result},
    format::{PageId, Xid},
    heap::{Heap, RowId},
    index_worker::{IndexHandle, IndexMsg, IndexedColumn, SecondaryIndex},
    lockmgr::LockManager,
    txn::{TransactionManager, UndoAction},
    wal::Wal,
};

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
    /// `None` in contexts with no live worker (e.g. rebuild-on-open, which
    /// talks to the worker directly rather than through the executor).
    pub index_worker: Option<&'a IndexHandle>,
}

/// Build the `IndexedColumn` list for one decoded `row`, from the subset of
/// `table_def`'s columns that are actually indexed. Shared between live
/// upserts (`send_index_upserts`, below) and every backfill/rebuild path
/// (`exec_create_index` here, and `lib.rs`'s rebuild-on-open) so the
/// column-type-to-`IndexedColumn`-variant mapping exists in exactly one
/// place.
pub fn build_indexed_columns(
    table_def: &TableDef,
    indexed: &[&ColumnDef],
    row: &[Literal],
) -> Vec<IndexedColumn> {
    let mut out = Vec::new();
    for col in indexed {
        let idx = table_def
            .columns
            .iter()
            .position(|c| c.name == col.name)
            .expect("indexed column is drawn from table_def.columns");
        match (&col.index, &row[idx]) {
            (Some(IndexKind::Hnsw), Literal::Vector(v)) => out.push(IndexedColumn::Vector {
                column: col.name.clone(),
                data: v.clone(),
            }),
            (Some(IndexKind::FullText), Literal::Text(t)) => out.push(IndexedColumn::Text {
                column: col.name.clone(),
                data: t.clone(),
            }),
            _ => {}
        }
    }
    out
}

/// If `table` has any indexed columns, send their values from `row` to the
/// background worker keyed by `row_id`. Checked once per statement row, not
/// once per column globally, so tables with no indexed column pay zero
/// overhead (CLAUDE.md's M2 "row write is the only synchronous cost" goal)
/// — this function only *sends*, the worker does all the actual index work
/// off this thread.
fn send_index_upserts(table_def: &TableDef, row_id: RowId, row: &[Literal], ctx: &ExecCtx) {
    let Some(handle) = ctx.index_worker else {
        return;
    };
    let indexed: Vec<&ColumnDef> = table_def
        .columns
        .iter()
        .filter(|c| c.index.is_some())
        .collect();
    if indexed.is_empty() {
        return;
    }
    let indexed_cols = build_indexed_columns(table_def, &indexed, row);
    if !indexed_cols.is_empty() {
        handle.send(IndexMsg::Upsert {
            table: table_def.name.clone(),
            record: row_id,
            indexed_cols,
        });
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExecResult {
    CreatedTable,
    CreatedIndex,
    Inserted { count: usize },
    Rows(Vec<Vec<Literal>>),
    Updated { count: usize },
    Deleted { count: usize },
}

pub fn execute(plan: LogicalPlan, ctx: &mut ExecCtx) -> Result<ExecResult> {
    match plan {
        LogicalPlan::CreateTable { name, columns } => exec_create_table(name, columns, ctx),
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
        LogicalPlan::CreateIndex {
            table,
            column,
            kind,
        } => exec_create_index(&table, &column, kind, ctx),
    }
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

fn exec_create_table(
    name: String,
    columns: Vec<ColumnDef>,
    ctx: &mut ExecCtx,
) -> Result<ExecResult> {
    let def = TableDef {
        name,
        columns,
        pages: Vec::new(),
        rls_policy: None,
    };
    let mut cctx = catalog_ctx!(ctx);
    ctx.catalog.create_table(def, &mut cctx)?;
    Ok(ExecResult::CreatedTable)
}

/// `CREATE INDEX ... ON table USING HNSW|FULLTEXT (column)`: validate the
/// column's type is compatible with the requested index kind, persist the
/// catalog flag, then immediately backfill the worker from every
/// currently-committed row (unlike `Engine::set_column_index`'s Rust-API
/// path from M2.b, which defers population to the next rebuild-on-open).
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
    match (&kind, &col.ty) {
        (IndexKind::Hnsw, ColumnType::Vector(_)) => {}
        (IndexKind::FullText, ColumnType::Text) => {}
        (kind, ty) => {
            return Err(DbError::SqlPlan(format!(
                "{kind:?} index is not valid on column {column} of type {ty:?}"
            )))
        }
    }

    let mut cctx = catalog_ctx!(ctx);
    ctx.catalog
        .set_column_index(table, column, Some(kind), &mut cctx)?;
    let table_def = ctx.catalog.lookup(table)?.clone();

    if let Some(handle) = ctx.index_worker {
        let col = table_def
            .columns
            .iter()
            .find(|c| c.name == column)
            .expect("column just validated above");
        let heap = Heap::from_pages(ctx.page_size, table_def.pages.clone());
        let snapshot = ctx.txn_mgr.snapshot_for_statement(ctx.xid)?;
        for (row_id, bytes) in heap.scan(&snapshot, ctx.xid, ctx.pool)? {
            let row = decode_row(&bytes, &table_def.columns)?;
            let indexed_cols = build_indexed_columns(&table_def, &[col], &row);
            if !indexed_cols.is_empty() {
                handle.send(IndexMsg::Upsert {
                    table: table_def.name.clone(),
                    record: row_id,
                    indexed_cols,
                });
            }
        }
        handle.send(IndexMsg::MarkReady {
            table: table_def.name.clone(),
            column: column.to_string(),
            kind,
        });
    }

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
    let mut heap = Heap::from_pages(ctx.page_size, table_def.pages.clone());

    let mut count = 0;
    for row_values in values {
        let ordered = order_values_by_columns(&table_def, &columns, row_values)?;
        let coerced = coerce_and_validate_row(&table_def, ordered)?;
        let encoded = encode_row(&coerced);
        let row_id = heap.insert(&encoded, ctx.xid, ctx.pool, ctx.wal)?;
        ctx.txn_mgr.record_undo(
            ctx.xid,
            UndoAction::Insert {
                page_id: row_id.page_id,
                slot: row_id.slot,
            },
        )?;
        send_index_upserts(&table_def, row_id, &coerced, ctx);
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

    let heap = Heap::from_pages(ctx.page_size, table_def.pages.clone());
    let snapshot = ctx.txn_mgr.snapshot_for_statement(ctx.xid)?;

    let mut out = Vec::new();
    for (_, bytes) in heap.scan(&snapshot, ctx.xid, ctx.pool)? {
        let row = decode_row(&bytes, &table_def.columns)?;
        if predicate_matches(predicate, &table_def.columns, &row)? {
            out.push(project_row(projection, &table_def.columns, &row)?);
        }
    }
    Ok(ExecResult::Rows(out))
}

/// Find a top-level (or top-level-AND'd) `Expr::Near` in a predicate.
/// Recurses only through `And`, matching the AND-only `WHERE` grammar — an
/// exhaustive search given there's no `OR`/nesting construct that could
/// hide a `Near` from this walk.
fn find_near(expr: &Expr) -> Option<(&str, &[f32], usize)> {
    match expr {
        Expr::Near { column, query, k } => Some((column.as_str(), query.as_slice(), *k)),
        Expr::And(lhs, rhs) => find_near(lhs).or_else(|| find_near(rhs)),
        _ => None,
    }
}

/// `NEAR`'s over-fetch-then-filter execution: look up the column's
/// `VectorIndex`, over-fetch `k`-ish candidates, resolve each back to a
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
    let col = table_def
        .columns
        .iter()
        .find(|c| c.name == column)
        .ok_or_else(|| DbError::ColumnNotFound {
            table: table_def.name.clone(),
            column: column.to_string(),
        })?;
    if !matches!(col.index, Some(IndexKind::Hnsw)) || !matches!(col.ty, ColumnType::Vector(_)) {
        return Err(DbError::SqlPlan(format!(
            "column {column} has no HNSW index; see CREATE INDEX ... USING HNSW"
        )));
    }
    let handle = ctx
        .index_worker
        .ok_or_else(|| DbError::SqlPlan("NEAR requires a live index worker".into()))?;

    // Over-fetch: candidates get re-filtered by the full predicate below
    // (MVCC visibility, RLS, any AND'd WHERE terms), so asking the index for
    // more than `k` raw nearest neighbors compensates for some being
    // filtered out. `4x k` or `k+20`, whichever is larger — a tunable
    // constant, not a locked decision.
    let over_fetch = (k.saturating_mul(4)).max(k + 20);
    let candidate_ids: Vec<RowId> = {
        let guard = handle.indexes.read().unwrap();
        match guard.get(&(table_def.name.clone(), column.to_string())) {
            // Not yet built (e.g. CREATE INDEX just enqueued its backfill,
            // worker hasn't processed the first message yet) — zero
            // candidates so far, not an error; see IndexStatus::Building.
            None => Vec::new(),
            Some(entry) => {
                let SecondaryIndex::Vector(v) = &entry.index else {
                    return Err(DbError::SqlPlan(format!(
                        "column {column}'s index is not a vector index"
                    )));
                };
                v.search(query, over_fetch)
                    .into_iter()
                    .map(|(id, _)| id)
                    .collect()
            }
        }
    };

    let heap = Heap::from_pages(ctx.page_size, table_def.pages.clone());
    let snapshot = ctx.txn_mgr.snapshot_for_statement(ctx.xid)?;
    let mut out = Vec::new();
    for row_id in candidate_ids {
        let bytes = match heap.get(row_id, &snapshot, ctx.xid, ctx.pool) {
            Ok(b) => b,
            // Not visible to this snapshot (superseded, or never committed
            // — e.g. an aborted insert the worker indexed before the
            // abort). Filtered out here, not an index-maintenance bug.
            Err(DbError::NoVisibleVersion { .. }) => continue,
            Err(e) => return Err(e),
        };
        let row = decode_row(&bytes, &table_def.columns)?;
        if predicate_matches(predicate, &table_def.columns, &row)? {
            out.push(project_row(projection, &table_def.columns, &row)?);
        }
    }
    out.truncate(k);
    Ok(ExecResult::Rows(out))
}

fn exec_update(
    table: &str,
    assignments: &[(String, Expr)],
    predicate: &Option<Expr>,
    ctx: &mut ExecCtx,
) -> Result<ExecResult> {
    let table_def = ctx.catalog.lookup(table)?.clone();
    let mut heap = Heap::from_pages(ctx.page_size, table_def.pages.clone());
    let snapshot = ctx.txn_mgr.snapshot_for_statement(ctx.xid)?;

    let matching = matching_rows(&heap, &snapshot, ctx, &table_def, predicate)?;

    let mut count = 0;
    for (row_id, mut row) in matching {
        for (col, expr) in assignments {
            let new_val = eval_expr(expr, &table_def.columns, &row)?;
            set_column(&table_def.columns, &mut row, col, new_val)?;
        }
        let coerced = coerce_and_validate_row(&table_def, row)?;
        let encoded = encode_row(&coerced);
        let new_row_id = heap.update(row_id, &encoded, ctx.xid, ctx.pool, ctx.wal, ctx.lock_mgr)?;
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
        send_index_upserts(&table_def, new_row_id, &coerced, ctx);
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

    let mut count = 0;
    for (row_id, _) in matching {
        heap.delete(row_id, ctx.xid, ctx.pool, ctx.wal, ctx.lock_mgr)?;
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
        None => {
            if values.len() != table.columns.len() {
                return Err(DbError::SqlPlan(format!(
                    "table '{}' has {} columns, but {} values were supplied",
                    table.name,
                    table.columns.len(),
                    values.len()
                )));
            }
            Ok(values)
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
        .position(|c| c.name == name)
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
        (expected, got) => Err(DbError::SqlPlan(format!(
            "table '{table_name}' column '{}': expected {expected:?}, got {got:?}",
            col.name
        ))),
    }
}

fn project_row(
    projection: &[String],
    columns: &[ColumnDef],
    row: &[Literal],
) -> Result<Vec<Literal>> {
    if projection.is_empty() {
        return Ok(row.to_vec());
    }
    projection
        .iter()
        .map(|name| {
            let idx = columns
                .iter()
                .position(|c| &c.name == name)
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
                .position(|c| &c.name == name)
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

fn as_bool(lit: &Literal) -> Result<bool> {
    match lit {
        Literal::Bool(b) => Ok(*b),
        Literal::Null => Ok(false),
        other => Err(DbError::SqlUnsupported(format!(
            "expected a boolean expression, got {other:?}"
        ))),
    }
}

fn compare(op: CmpOp, l: &Literal, r: &Literal) -> Result<bool> {
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
        (a, b) => Err(DbError::SqlUnsupported(format!(
            "cannot compare {a:?} with {b:?}"
        ))),
    }
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
// (4-byte LE dimension + dimension * 4-byte LE f32).

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
        }
    }
    buf
}

pub fn decode_row(bytes: &[u8], columns: &[ColumnDef]) -> Result<Vec<Literal>> {
    let mut out = Vec::with_capacity(columns.len());
    let mut pos = 0usize;
    for col in columns {
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
                index_worker: None,
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
        let rows = heap.scan(&snap, 1000, &mut pool).unwrap();
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
                ty: ColumnType::Int64,
            },
            ColumnDef {
                name: "b".to_string(),
                index: None,
                ty: ColumnType::Text,
            },
            ColumnDef {
                name: "c".to_string(),
                index: None,
                ty: ColumnType::Bool,
            },
            ColumnDef {
                name: "d".to_string(),
                index: None,
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
                ty: ColumnType::Vector(4),
            }],
            pages: vec![],
            rls_policy: None,
        };
        let err = coerce_and_validate_row(&table, vec![Literal::Vector(vec![0.1, 0.2, 0.3])]);
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
    fn near_on_indexed_column_without_live_worker_is_rejected() {
        let dir = tempdir().unwrap();
        let mut h = Harness::new(dir.path());
        let xid = h.begin();
        h.exec_as(xid, "CREATE TABLE t (id INT, embedding VECTOR(2))")
            .unwrap();
        h.exec_as(xid, "CREATE INDEX idx ON t USING HNSW (embedding)")
            .unwrap();
        // Harness never supplies an index_worker (index_worker: None), so
        // even though the column is now validly indexed, there's no live
        // worker to query against.
        let err = h
            .exec_as(xid, "SELECT * FROM t WHERE NEAR(embedding, [0.0, 0.0], 3)")
            .unwrap_err();
        assert!(matches!(err, DbError::SqlPlan(_)));
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
