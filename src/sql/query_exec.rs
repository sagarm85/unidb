//! Phase-4 query driver: turns a [`QuerySpec`] into a physical [`PlanNode`]
//! ([`crate::sql::plan::plan_query`]) and runs it against storage, producing an
//! [`ExecResult::Rows`]. Lives here (not in `plan.rs`/`join.rs`) because it is
//! the one part of Phase-4 execution that touches the engine: base scans read
//! heaps under the statement snapshot, index-nested-loop probes the durable
//! on-disk B-Tree per outer row, and subqueries re-plan + re-run against it.
//!
//! The whole query (including CTEs and subqueries) runs under a **single**
//! statement snapshot so every scan sees one consistent view.
//!
//! Subqueries (P4.c): a correlated subquery is executed once per outer row with
//! its correlated columns substituted by the outer row's values (turning it
//! into an ordinary uncorrelated query); an uncorrelated subquery produces the
//! same bound form every row and is cached so it runs once. CTEs are
//! materialized once up front and referenced by name in FROM.
//!
//! ## Item 51 Phase B — in-memory hash join for equi-joins
//!
//! When the inner relation of an [`PlanNode::IndexNestedLoopJoin`] fits within
//! `HASH_JOIN_INNER_ROW_BUDGET` rows, `try_build_hash_table` scans it once and
//! builds a `HashMap<Vec<u8>, Vec<Vec<Literal>>>` keyed on the encoded join-key
//! column value. The outer loop then probes O(1) per row instead of doing a
//! B-tree search per outer row. Falls back to the INLJ B-tree path when the
//! inner relation exceeds the budget or the join key is not a comparable type.
//! Override the budget via `UNIDB_HASH_JOIN_BUDGET` (e.g. `="1"` forces INLJ).

use std::collections::HashMap;

use crate::btree_index::{DiskBTree, OrderedValue};
use crate::catalog::{ColumnType, IndexKind, ROW_COUNT_UNKNOWN};
use crate::error::{DbError, Result};
use crate::heap::Heap;
use crate::mvcc::Snapshot;
use crate::sql::executor::{self, decode_row, deform_row, ExecCtx, ExecResult};
use crate::sql::join;
use crate::sql::logical::{CmpOp, Literal, SetOpKind};
use crate::sql::plan::{
    self, eval_cast, eval_qexpr, join_key_bytes, plan_query, resolve_column, Batch, ColumnRef,
    CteSchemas, PlanNode,
};
use crate::sql::query::{AggFunc, JoinType, QExpr, QuerySpec};

/// Item 51 Phase B — maximum inner-relation row count before falling back from
/// the in-memory hash join to the B-tree index-nested-loop join. At this scale
/// the one-time full scan + in-memory probes beats per-row B-tree lookups.
/// Override via `UNIDB_HASH_JOIN_BUDGET` env var (e.g. `="1"` forces INLJ in tests).
const HASH_JOIN_INNER_ROW_BUDGET: usize = 500_000;

/// Return the configured hash-join inner budget (row count cap).
fn hash_join_inner_budget() -> usize {
    std::env::var("UNIDB_HASH_JOIN_BUDGET")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(HASH_JOIN_INNER_ROW_BUDGET)
}

/// Item 51 Phase B — build an in-memory hash table keyed on the join column
/// (`join_col`) for every live row in `inner_table`. Returns `None` when the
/// inner relation has more than `budget` rows (caller falls back to INLJ).
///
/// The map key is the canonical byte encoding of the join key value (same
/// encoding used by `join::hash_join`'s general path). NULL join-key rows are
/// skipped — SQL equi-join semantics: NULL never matches.
#[allow(clippy::type_complexity)]
fn try_build_hash_table(
    inner_table: &str,
    join_col: &str,
    snapshot: &Snapshot,
    ctx: &mut ExecCtx,
    budget: usize,
) -> Result<Option<HashMap<Vec<u8>, Vec<Vec<Literal>>>>> {
    let table_def = ctx.catalog.lookup(inner_table)?.clone();

    // Fast row-count gate: if the catalog's row_count already exceeds the budget
    // we can skip the scan entirely. row_count is maintained by INSERT/DELETE, so
    // it is exact under normal operation (may be slightly stale after crash-
    // recovery, but only in a conservative direction — we'll scan if unsure).
    if table_def.row_count > budget as i64 {
        return Ok(None);
    }

    // Find the join column index in the visible (non-dropped) column list.
    let join_col_idx = table_def
        .columns
        .iter()
        .filter(|c| !c.dropped)
        .position(|c| c.name == join_col);
    let Some(join_col_idx) = join_col_idx else {
        // Column not found — fall back to INLJ.
        return Ok(None);
    };

    let heap = Heap::open(ctx.page_size, table_def.fsm_meta, table_def.pages.clone());

    let mut table: HashMap<Vec<u8>, Vec<Vec<Literal>>> = HashMap::new();
    let mut row_count = 0usize;

    for (_, bytes) in heap.scan(snapshot, ctx.xid, ctx.pool)? {
        if row_count > budget {
            // Exceeded budget mid-scan — abort and fall back to INLJ.
            return Ok(None);
        }
        let full = decode_row(&bytes, &table_def.columns)?;
        let row = visible_row(&full, &table_def);
        let key_lit = row[join_col_idx].clone();
        // NULL keys never equi-match — skip them.
        if matches!(key_lit, Literal::Null) {
            row_count += 1;
            continue;
        }
        if let Some(key_bytes) = join_key_bytes(&[key_lit]) {
            table.entry(key_bytes).or_default().push(row);
        }
        row_count += 1;
    }

    Ok(Some(table))
}

pub fn exec_query(spec: &QuerySpec, ctx: &mut ExecCtx) -> Result<ExecResult> {
    let snapshot = ctx.txn_mgr.snapshot_for_statement(ctx.xid)?;
    let mut runner = Runner {
        ctx,
        snapshot,
        cte_schemas: CteSchemas::new(),
        cte_batches: HashMap::new(),
        subquery_cache: HashMap::new(),
    };
    let node = runner.materialize_ctes_and_plan(spec)?;
    let batch = runner.run(&node)?;
    Ok(ExecResult::Rows {
        columns: batch.schema.iter().map(|c| c.name.clone()).collect(),
        rows: batch.rows,
    })
}

/// `EXPLAIN [ANALYZE]` (P4.e): render the chosen plan tree (estimated rows),
/// and with `analyze` also run it and append actual rows + execution time. The
/// result is one text column, one plan line per row.
pub fn exec_explain(spec: &QuerySpec, analyze: bool, ctx: &mut ExecCtx) -> Result<ExecResult> {
    let snapshot = ctx.txn_mgr.snapshot_for_statement(ctx.xid)?;
    let mut runner = Runner {
        ctx,
        snapshot,
        cte_schemas: CteSchemas::new(),
        cte_batches: HashMap::new(),
        subquery_cache: HashMap::new(),
    };
    let node = runner.materialize_ctes_and_plan(spec)?;
    let mut lines = crate::sql::explain::render_estimated(&node, runner.ctx.catalog.get());
    if analyze {
        let start = std::time::Instant::now();
        let batch = runner.run(&node)?;
        let elapsed = start.elapsed();
        lines.push(format!("actual_rows={}", batch.rows.len()));
        lines.push(format!(
            "execution_time_ms={:.3}",
            elapsed.as_secs_f64() * 1000.0
        ));
    }
    Ok(ExecResult::Rows {
        columns: vec!["QUERY PLAN".to_string()],
        rows: lines.into_iter().map(|l| vec![Literal::Text(l)]).collect(),
    })
}

/// Execute one branch of a set-operation (UNION / INTERSECT / EXCEPT).
///
/// A branch is either:
///   - `LogicalPlan::Query(spec)` → run through `exec_query`
///   - `LogicalPlan::SetOp { .. }` → recurse through `exec_set_op` (chained set-ops)
///   - `LogicalPlan::Select { .. }` → run through the executor (simple single-table SELECT)
///
/// Any other plan variant (DML, DDL) is rejected because it cannot appear inside
/// a set-operation.
fn exec_plan_branch(
    plan: &crate::sql::logical::LogicalPlan,
    ctx: &mut ExecCtx,
) -> Result<ExecResult> {
    use crate::sql::logical::LogicalPlan;
    match plan {
        LogicalPlan::Query(spec) => exec_query(spec, ctx),
        LogicalPlan::SetOp {
            op,
            all,
            left,
            right,
        } => exec_set_op(*op, *all, left, right, ctx),
        // Simple single-table SELECT that ended up as the branch of a set-op.
        LogicalPlan::Select { .. } => crate::sql::executor::execute(plan.clone(), ctx),
        other => Err(crate::error::DbError::SqlPlan(format!(
            "unexpected plan variant as set-op branch: {:?}",
            std::mem::discriminant(other)
        ))),
    }
}

/// G3 (item 19): `SELECT … UNION [ALL] SELECT …` / `INTERSECT` / `EXCEPT`.
///
/// Runs both sides as independent queries under the same engine context and
/// combines their results:
/// - UNION ALL: concatenate left + right (no dedup).
/// - UNION: concatenate + dedup by row encoding.
/// - INTERSECT ALL / DISTINCT: rows present in both sides.
/// - EXCEPT ALL / DISTINCT: rows in left but not in right.
pub fn exec_set_op(
    op: SetOpKind,
    all: bool,
    left: &crate::sql::logical::LogicalPlan,
    right: &crate::sql::logical::LogicalPlan,
    ctx: &mut ExecCtx,
) -> Result<ExecResult> {
    // Run each branch with its own Runner (snapshot, CTE scope) so they are
    // independent. Both share the same ExecCtx (catalog, pool, xid, txn_mgr).
    let left_result = exec_plan_branch(left, ctx)?;
    let right_result = exec_plan_branch(right, ctx)?;

    let (l_cols, l_rows) = match left_result {
        ExecResult::Rows { columns, rows } => (columns, rows),
        other => {
            return Err(DbError::SqlPlan(format!(
                "UNION/INTERSECT/EXCEPT left side produced a non-row result: {other:?}"
            )))
        }
    };
    let (_, r_rows) = match right_result {
        ExecResult::Rows { columns, rows } => (columns, rows),
        other => {
            return Err(DbError::SqlPlan(format!(
                "UNION/INTERSECT/EXCEPT right side produced a non-row result: {other:?}"
            )))
        }
    };

    // Synthesise a trivial ColumnRef schema from the left-side column names;
    // type tracking through set-ops is a v2 concern.
    let output_schema: Vec<ColumnRef> = l_cols
        .iter()
        .map(|name| ColumnRef {
            qualifier: String::new(),
            name: name.clone(),
            ty: ColumnType::Text,
        })
        .collect();

    let left_batch = Batch {
        schema: output_schema.clone(),
        rows: l_rows,
    };
    let right_batch = Batch {
        schema: output_schema.clone(),
        rows: r_rows,
    };

    let result_batch = exec_set_op_batches(op, all, left_batch, right_batch, &output_schema)?;
    Ok(ExecResult::Rows {
        columns: l_cols,
        rows: result_batch.rows,
    })
}

/// Core batch-level set-operation implementation shared between the top-level
/// `exec_set_op` (which runs both sides as independent queries) and `Runner::run`
/// (which runs them recursively via the same Runner).
fn exec_set_op_batches(
    op: SetOpKind,
    all: bool,
    left: Batch,
    right: Batch,
    output: &[ColumnRef],
) -> Result<Batch> {
    use std::collections::HashSet;

    let rows = match op {
        SetOpKind::Union => {
            let mut combined = left.rows;
            combined.extend(right.rows);
            if all {
                combined
            } else {
                // Dedup by row encoding.
                let mut seen = HashSet::new();
                combined
                    .into_iter()
                    .filter(|row| seen.insert(executor::encode_row(row)))
                    .collect()
            }
        }
        SetOpKind::Intersect => {
            if all {
                // INTERSECT ALL: for each right row, consume one matching left row.
                let mut left_remaining: Vec<Vec<Literal>> = left.rows;
                let mut result = Vec::new();
                for r_row in right.rows {
                    let key = executor::encode_row(&r_row);
                    if let Some(pos) = left_remaining
                        .iter()
                        .position(|l| executor::encode_row(l) == key)
                    {
                        result.push(left_remaining.remove(pos));
                    }
                }
                result
            } else {
                // INTERSECT DISTINCT: rows that appear in both sides, deduped.
                let right_keys: HashSet<Vec<u8>> =
                    right.rows.iter().map(|r| executor::encode_row(r)).collect();
                let mut seen = HashSet::new();
                left.rows
                    .into_iter()
                    .filter(|row| {
                        let key = executor::encode_row(row);
                        right_keys.contains(&key) && seen.insert(key)
                    })
                    .collect()
            }
        }
        SetOpKind::Except => {
            if all {
                // EXCEPT ALL: for each right row, remove one matching left row.
                let mut left_remaining = left.rows;
                for r_row in right.rows {
                    let key = executor::encode_row(&r_row);
                    if let Some(pos) = left_remaining
                        .iter()
                        .position(|l| executor::encode_row(l) == key)
                    {
                        left_remaining.remove(pos);
                    }
                }
                left_remaining
            } else {
                // EXCEPT DISTINCT: rows in left but not in right, deduped.
                let right_keys: HashSet<Vec<u8>> =
                    right.rows.iter().map(|r| executor::encode_row(r)).collect();
                let mut seen = HashSet::new();
                left.rows
                    .into_iter()
                    .filter(|row| {
                        let key = executor::encode_row(row);
                        !right_keys.contains(&key) && seen.insert(key)
                    })
                    .collect()
            }
        }
    };

    Ok(Batch {
        schema: output.to_vec(),
        rows,
    })
}

struct Runner<'a, 'b> {
    ctx: &'a mut ExecCtx<'b>,
    snapshot: Snapshot,
    cte_schemas: CteSchemas,
    cte_batches: HashMap<String, Batch>,
    /// Uncorrelated-subquery result cache, keyed by the bound subquery's JSON.
    subquery_cache: HashMap<String, Batch>,
}

impl Runner<'_, '_> {
    /// Materialize `spec`'s CTEs (each visible to later CTEs) into batches, then
    /// plan the main query with those CTE schemas in scope.
    fn materialize_ctes_and_plan(&mut self, spec: &QuerySpec) -> Result<PlanNode> {
        for (name, cte_spec) in &spec.with {
            let cte_plan = plan_query(cte_spec, self.ctx.catalog.get(), &self.cte_schemas)?;
            let batch = self.run(&cte_plan)?;
            self.cte_schemas.insert(name.clone(), batch.schema.clone());
            self.cte_batches.insert(name.clone(), batch);
        }
        plan_query(spec, self.ctx.catalog.get(), &self.cte_schemas)
    }

    fn run(&mut self, node: &PlanNode) -> Result<Batch> {
        match node {
            PlanNode::Scan { table, output, .. } => self.scan(table, output),

            // G8 (item 19): SELECT without FROM — emit one empty row so the
            // projection above can evaluate pure literals / arithmetic.
            PlanNode::Dual => Ok(Batch {
                schema: vec![],
                rows: vec![vec![]],
            }),

            PlanNode::IndexScan {
                table,
                column,
                op,
                value,
                output,
                index_only,
                ..
            } => self.index_scan(table, column, *op, value, output, *index_only),

            PlanNode::CteScan { name, output, .. } => {
                let base = self
                    .cte_batches
                    .get(name)
                    .ok_or_else(|| DbError::SqlPlan(format!("unknown CTE '{name}'")))?;
                Ok(Batch {
                    schema: output.clone(),
                    rows: base.rows.clone(),
                })
            }

            PlanNode::Filter {
                input, predicate, ..
            } => {
                let batch = self.run(input)?;
                let mut rows = Vec::new();
                for row in batch.rows {
                    let v = self.eval(predicate, &batch.schema, &row)?;
                    if executor::as_bool(&v)? {
                        rows.push(row);
                    }
                }
                Ok(Batch {
                    schema: batch.schema,
                    rows,
                })
            }

            PlanNode::Projection {
                input,
                items,
                output,
            } => {
                let batch = self.run(input)?;
                let mut rows = Vec::with_capacity(batch.rows.len());
                for row in &batch.rows {
                    let mut projected = Vec::with_capacity(items.len());
                    for it in items {
                        projected.push(self.eval(&it.expr, &batch.schema, row)?);
                    }
                    rows.push(projected);
                }
                Ok(Batch {
                    schema: output.clone(),
                    rows,
                })
            }

            PlanNode::HashJoin {
                left,
                right,
                join_type,
                left_keys,
                right_keys,
                residual,
                ..
            } => {
                let l = self.run(left)?;
                let r = self.run(right)?;
                join::hash_join(
                    l,
                    r,
                    *join_type,
                    left_keys,
                    right_keys,
                    residual,
                    plan::hash_join_mem_rows(),
                )
            }

            PlanNode::MergeJoin {
                left,
                right,
                join_type,
                left_keys,
                right_keys,
                residual,
                ..
            } => {
                let l = self.run(left)?;
                let r = self.run(right)?;
                join::merge_join(l, r, *join_type, left_keys, right_keys, residual)
            }

            PlanNode::NestedLoopJoin {
                left,
                right,
                join_type,
                on,
                ..
            } => {
                let l = self.run(left)?;
                let r = self.run(right)?;
                join::nested_loop_join(l, r, *join_type, on)
            }

            PlanNode::IndexNestedLoopJoin {
                left,
                right_table,
                right_qualifier,
                right_index_column,
                left_key,
                join_type,
                residual,
                output,
            } => self.index_nested_loop_join(
                left,
                right_table,
                right_qualifier,
                right_index_column,
                left_key,
                *join_type,
                residual,
                output,
            ),

            PlanNode::Aggregate {
                input,
                group_exprs,
                aggs,
                output,
            } => {
                // B1: `SELECT COUNT(*) FROM t` — no GROUP BY, every aggregate a
                // plain `COUNT(*)` (no arg, not DISTINCT), directly over a full
                // `Scan` (no filter between).
                //
                // Item 97 fast path: return `row_count` from the catalog in O(1)
                // when the count is exact (no WHERE, no GROUP BY, no DISTINCT,
                // no JOIN, plain Scan input). Any deviation falls through to the
                // proven `Heap::count_visible` path.
                if group_exprs.is_empty()
                    && !aggs.is_empty()
                    && aggs
                        .iter()
                        .all(|a| matches!(a.func, AggFunc::Count) && a.arg.is_none() && !a.distinct)
                {
                    if let PlanNode::Scan { table, .. } = input.as_ref() {
                        if !crate::sql::information_schema::is_virtual_relation(table) {
                            // Honor cancellation/timeout even on the O(1) fast path.
                            crate::query_limits::check()?;
                            let table_def = self.ctx.catalog.lookup(table)?.clone();
                            // Item 97 O(1) path: must NOT be taken for RLS-protected
                            // tables — row_count is the physical count; RLS may hide a
                            // subset of rows (item-24 Z2). Fall through to count_visible
                            // which respects the visibility chain.
                            //
                            // Item 104: also skip when row_count == ROW_COUNT_UNKNOWN —
                            // after a crash, the persisted catalog may have a stale count
                            // (catalog is only checkpointed, not fsynced on every commit).
                            // count_visible performs an exact heap scan; the result is
                            // cached back into row_count so subsequent COUNTs are O(1)
                            // again (until the next crash).
                            if table_def.rls_policy.is_none()
                                && table_def.row_count != ROW_COUNT_UNKNOWN
                            {
                                let row = vec![Literal::Int(table_def.row_count); aggs.len()];
                                return Ok(Batch {
                                    schema: output.clone(),
                                    rows: vec![row],
                                });
                            }
                            // Item 104 calibration path: row_count is unknown after
                            // crash recovery — scan the heap to get the exact count and
                            // cache it back into the in-memory catalog so subsequent
                            // COUNTs are O(1) (until the next crash or catalog reload).
                            if table_def.rls_policy.is_none()
                                && table_def.row_count == ROW_COUNT_UNKNOWN
                            {
                                tracing::debug!(
                                    table = %table,
                                    "item 104: row_count unknown after recovery — \
                                     falling back to heap scan for COUNT(*)"
                                );
                                let heap = Heap::open(
                                    self.ctx.page_size,
                                    table_def.fsm_meta,
                                    table_def.pages.clone(),
                                );
                                let exact = heap.count_visible(
                                    &self.snapshot,
                                    self.ctx.xid,
                                    self.ctx.pool,
                                )? as i64;
                                // Cache the exact count back so future COUNTs are O(1).
                                if let Ok(cat) = self.ctx.catalog.exclusive() {
                                    if let Some(t) = cat.tables_mut().get_mut(table.as_str()) {
                                        if t.row_count == ROW_COUNT_UNKNOWN {
                                            t.row_count = exact;
                                        }
                                    }
                                }
                                let row = vec![Literal::Int(exact); aggs.len()];
                                return Ok(Batch {
                                    schema: output.clone(),
                                    rows: vec![row],
                                });
                            }
                        }
                    }

                    // Partial aggregate: `COUNT(*)` over a `Filter` (subquery-free
                    // predicate) over a `Scan` — push the scan + filter + count
                    // ALL into the workers, so the whole thing is parallel instead
                    // of just the base scan (the fix for the filtered-scan Amdahl
                    // tail). A subquery predicate needs the `Runner` → fall back.
                    if let PlanNode::Filter {
                        input: filter_input,
                        predicate,
                        ..
                    } = input.as_ref()
                    {
                        if let PlanNode::Scan {
                            table,
                            output: scan_output,
                            ..
                        } = filter_input.as_ref()
                        {
                            if !predicate.has_subquery()
                                && !crate::sql::information_schema::is_virtual_relation(table)
                            {
                                let table_def = self.ctx.catalog.lookup(table)?.clone();
                                let heap = Heap::open(
                                    self.ctx.page_size,
                                    table_def.fsm_meta,
                                    table_def.pages.clone(),
                                );
                                let pages = heap.scan_pages(self.ctx.pool)?;
                                if let Some(lease) = crate::sql::parallel_scan::acquire(pages.len())
                                {
                                    let cols = &table_def.columns;
                                    let matches = |bytes: &[u8]| -> Result<bool> {
                                        let full = decode_row(bytes, cols)?;
                                        let row = visible_row(&full, &table_def);
                                        executor::as_bool(&eval_qexpr(
                                            predicate,
                                            scan_output,
                                            &row,
                                        )?)
                                    };
                                    let count = crate::sql::parallel_scan::parallel_count_matching(
                                        &pages,
                                        &self.ctx.pool.shared_reader(),
                                        &self.snapshot,
                                        self.ctx.xid,
                                        lease.degree(),
                                        &matches,
                                    )?;
                                    let row = vec![Literal::Int(count as i64); aggs.len()];
                                    return Ok(Batch {
                                        schema: output.clone(),
                                        rows: vec![row],
                                    });
                                }
                            }
                        }
                    }
                }
                // Item 46: COUNT(*) GROUP BY <simple column refs> over a base Scan.
                // Decode only the group-by columns via deform_row (B2 pushdown).
                // Any complex expr (non-Column ref, subquery, virtual table) falls
                // through to the generic decode-everything path below.
                if !group_exprs.is_empty()
                    && aggs
                        .iter()
                        .all(|a| matches!(a.func, AggFunc::Count) && a.arg.is_none() && !a.distinct)
                {
                    if let PlanNode::Scan { table, .. } = input.as_ref() {
                        if !crate::sql::information_schema::is_virtual_relation(table) {
                            let table_def = self.ctx.catalog.lookup(table)?.clone();
                            let cols = &table_def.columns;
                            let ncols = cols.len();

                            // Extract column indices for each group_expr.
                            // Any complex expression (non-Column) → fall through.
                            let mut needed: Vec<usize> = Vec::new();
                            let mut ok = true;
                            for ge in group_exprs.iter() {
                                if let QExpr::Column { name, .. } = ge {
                                    if let Some(idx) =
                                        cols.iter().position(|c| !c.dropped && &c.name == name)
                                    {
                                        if !needed.contains(&idx) {
                                            needed.push(idx);
                                        }
                                    } else {
                                        ok = false;
                                        break;
                                    }
                                } else {
                                    ok = false;
                                    break;
                                }
                            }

                            if ok && !needed.is_empty() && needed.len() < ncols {
                                let mut mask = vec![false; ncols];
                                let mut upto = 0usize;
                                for &i in &needed {
                                    mask[i] = true;
                                    upto = upto.max(i);
                                }
                                let heap = Heap::open(
                                    self.ctx.page_size,
                                    table_def.fsm_meta,
                                    table_def.pages.clone(),
                                );

                                // Item 56 Step 1: parallel GROUP BY partial aggregation.
                                // Workers each hold a local hash table (key_bytes →
                                // (key_literals, count)) and scan pages via a work-stealing
                                // cursor. Merge partials at the end — no per-row Vec<Literal>
                                // materialization on either the parallel or the serial path.
                                //
                                // Key encoding is done in the closure (not inside
                                // parallel_scan) to avoid a module cycle.
                                let extract_key =
                                    |bytes: &[u8]| -> Result<(Vec<u8>, Vec<Literal>)> {
                                        let row = deform_row(bytes, cols, upto, &mask)?;
                                        let key: Vec<Literal> =
                                            needed.iter().map(|&i| row[i].clone()).collect();
                                        let key_bytes = executor::encode_row(&key);
                                        Ok((key_bytes, key))
                                    };

                                let pages = heap.scan_pages(self.ctx.pool)?;
                                let groups: Vec<(Vec<Literal>, usize)>;

                                if let Some(lease) = crate::sql::parallel_scan::acquire(pages.len())
                                {
                                    groups = crate::sql::parallel_scan::parallel_group_count(
                                        &pages,
                                        &self.ctx.pool.shared_reader(),
                                        &self.snapshot,
                                        self.ctx.xid,
                                        lease.degree(),
                                        &extract_key,
                                    )?;
                                } else {
                                    // Serial streaming fold: no full-row Vec materialization.
                                    let mut local: std::collections::HashMap<
                                        Vec<u8>,
                                        (Vec<Literal>, usize),
                                    > = std::collections::HashMap::new();
                                    for (_, bytes) in
                                        heap.scan(&self.snapshot, self.ctx.xid, self.ctx.pool)?
                                    {
                                        let (key_bytes, key_lits) = extract_key(&bytes)?;
                                        let entry =
                                            local.entry(key_bytes).or_insert_with(|| (key_lits, 0));
                                        entry.1 += 1;
                                    }
                                    groups = local.into_values().collect();
                                }

                                // Assemble output rows: [group_key_cols..., count, ...].
                                // All aggs are COUNT(*) (the guard above ensures this), so
                                // every agg column gets the same per-group count.
                                let rows: Vec<Vec<Literal>> = groups
                                    .into_iter()
                                    .map(|(key_lits, count)| {
                                        let mut row = key_lits;
                                        for _ in aggs {
                                            row.push(Literal::Int(count as i64));
                                        }
                                        row
                                    })
                                    .collect();
                                return Ok(Batch {
                                    schema: output.to_vec(),
                                    rows,
                                });
                            }
                        }
                    }
                }

                let batch = self.run(input)?;
                crate::sql::aggregate::aggregate(batch, group_exprs, aggs, output)
            }

            PlanNode::Distinct { input, output } => {
                let batch = self.run(input)?;
                let mut seen = std::collections::HashSet::new();
                let mut rows = Vec::new();
                for row in batch.rows {
                    if seen.insert(executor::encode_row(&row)) {
                        rows.push(row);
                    }
                }
                Ok(Batch {
                    schema: output.clone(),
                    rows,
                })
            }

            PlanNode::Sort {
                input,
                keys,
                output,
            } => {
                let batch = self.run(input)?;
                let rows = crate::sql::sort::sort_rows(
                    batch.rows,
                    keys,
                    crate::sql::sort::sort_mem_rows(),
                )?;
                Ok(Batch {
                    schema: output.clone(),
                    rows,
                })
            }

            PlanNode::Limit {
                input,
                limit,
                offset,
                output,
            } => {
                let batch = self.run(input)?;
                let rows: Vec<_> = batch
                    .rows
                    .into_iter()
                    .skip(*offset)
                    .take(limit.unwrap_or(usize::MAX))
                    .collect();
                Ok(Batch {
                    schema: output.clone(),
                    rows,
                })
            }

            // G3 (item 19): UNION [ALL] / INTERSECT [ALL] / EXCEPT [ALL].
            PlanNode::SetOp {
                op,
                all,
                left,
                right,
                output,
            } => {
                let l = self.run(left)?;
                let r = self.run(right)?;
                exec_set_op_batches(*op, *all, l, r, output)
            }

            // G6 (item 19): derived table — `(SELECT …) AS alias`.
            // Execute the inner subquery to materialize its rows, then present
            // them under the outer schema (all columns requalified to `alias`).
            // The inner subquery already ran through RLS at plan time, so no
            // additional filtering is needed here.
            PlanNode::DerivedTable {
                subquery, output, ..
            } => {
                let inner = self.run(subquery)?;
                Ok(Batch {
                    schema: output.clone(),
                    rows: inner.rows,
                })
            }
        }
    }

    fn scan(&mut self, table: &str, output: &[ColumnRef]) -> Result<Batch> {
        // Milestone 18, Epic C: a virtual `information_schema.*` / `unidb_catalog.*`
        // relation has no heap — its rows are synthesized from the live catalog.
        // (Row order matches the `virtual_schema` used to build `output`.)
        if crate::sql::information_schema::is_virtual_relation(table) {
            // C3 (item 29): subscription_lag reads __consumers__ + __events__ heaps
            // and needs pool + snapshot — handled separately from the catalog-only path.
            let rows = if table.eq_ignore_ascii_case("unidb_catalog.subscription_lag") {
                crate::sql::information_schema::subscription_lag_rows(
                    self.ctx.catalog.get(),
                    self.ctx.pool,
                    self.ctx.page_size,
                    &self.snapshot,
                    self.ctx.xid,
                    self.ctx.event_seq_index_meta,
                )?
            } else {
                crate::sql::information_schema::virtual_rows(
                    table,
                    self.ctx.catalog.get(),
                    self.ctx.authz,
                )?
            };
            return Ok(Batch {
                schema: output.to_vec(),
                rows,
            });
        }
        let table_def = self.ctx.catalog.lookup(table)?.clone();
        let heap = Heap::open(
            self.ctx.page_size,
            table_def.fsm_meta,
            table_def.pages.clone(),
        );
        let decode_visible = |bytes: &[u8]| -> Result<Option<Vec<Literal>>> {
            Ok(Some(visible_row(
                &decode_row(bytes, &table_def.columns)?,
                &table_def,
            )))
        };

        // P-b: parallelize the base scan across worker threads when the table is
        // large (Milestone P). This is the scan feeding Filter/Aggregate — the
        // Table 3.1 `COUNT(*) WHERE …` / grouped-scan hot path. A base Scan is
        // unordered (any `ORDER BY` is a Sort node above), so concat is correct.
        let pages = heap.scan_pages(self.ctx.pool)?;
        if let Some(lease) = crate::sql::parallel_scan::acquire(pages.len()) {
            let (rows, _ids) = crate::sql::parallel_scan::parallel_filter_project(
                &pages,
                &self.ctx.pool.shared_reader(),
                &self.snapshot,
                self.ctx.xid,
                lease.degree(),
                &|_rid, bytes| decode_visible(bytes),
            )?;
            return Ok(Batch {
                schema: output.to_vec(),
                rows,
            });
        }

        let mut rows = Vec::new();
        for (i, (_, bytes)) in heap
            .scan(&self.snapshot, self.ctx.xid, self.ctx.pool)?
            .into_iter()
            .enumerate()
        {
            // P5.f: honor query timeout / cancellation, batched every 1024 rows
            // so the check itself is free on the hot path.
            if i % 1024 == 0 {
                crate::query_limits::check()?;
            }
            if let Some(row) = decode_visible(&bytes)? {
                rows.push(row);
            }
        }
        Ok(Batch {
            schema: output.to_vec(),
            rows,
        })
    }

    /// Index scan (P4.d): probe the column's durable B-Tree for `column <op>
    /// value`, fetch the matching rows under the snapshot, project to visible
    /// columns. Falls back to a full scan if the index isn't usable.
    ///
    /// Item 102-A: when `index_only` is true and every projected column equals
    /// `column`, the B-tree leaf value is returned directly without a heap fetch.
    fn index_scan(
        &mut self,
        table: &str,
        column: &str,
        op: CmpOp,
        value: &Literal,
        output: &[ColumnRef],
        index_only: bool,
    ) -> Result<Batch> {
        let table_def = self.ctx.catalog.lookup(table)?.clone();
        let meta_page = table_def
            .columns
            .iter()
            .find(|c| c.name == column && !c.dropped)
            .and_then(|c| c.index_root);
        let ordered = OrderedValue::try_from(value).ok();
        let (Some(meta_page), Some(ordered)) = (meta_page, ordered) else {
            // Can't use the index — fall back to a full scan (still correct).
            return self.scan(table, output);
        };
        let tree = DiskBTree::new(meta_page, self.ctx.page_size);

        // Item 102-A: index-only path — when every projected column is the
        // indexed column, use the B-tree leaf key directly.  We still call
        // heap.get() for MVCC visibility (B-tree may hold stale dead-tuple
        // entries not yet vacuumed), but skip the row-decode step.
        if index_only {
            let key_candidates = tree.search_with_keys(op, &ordered, self.ctx.pool)?;
            let heap = Heap::open(
                self.ctx.page_size,
                table_def.fsm_meta,
                table_def.pages.clone(),
            );
            let mut rows: Vec<Vec<Literal>> = Vec::with_capacity(key_candidates.len());
            for (key, rid) in key_candidates {
                match heap.get(rid, &self.snapshot, self.ctx.xid, self.ctx.pool) {
                    Ok(_bytes) => {
                        // Visible — return the key value, skip deform_row.
                        rows.push(vec![key.into_literal()]);
                    }
                    Err(DbError::NoVisibleVersion { .. }) => continue,
                    Err(e) => return Err(e),
                }
            }
            executor::IDX_ONLY_ROWS
                .fetch_add(rows.len() as u64, std::sync::atomic::Ordering::Relaxed);
            return Ok(Batch {
                schema: output.to_vec(),
                rows,
            });
        }

        let Some(candidate_ids) = tree.search(op, &ordered, self.ctx.pool)? else {
            return self.scan(table, output);
        };
        let heap = Heap::open(
            self.ctx.page_size,
            table_def.fsm_meta,
            table_def.pages.clone(),
        );
        let mut rows = Vec::new();
        for row_id in candidate_ids {
            match heap.get(row_id, &self.snapshot, self.ctx.xid, self.ctx.pool) {
                Ok(bytes) => rows.push(visible_row(
                    &decode_row(&bytes, &table_def.columns)?,
                    &table_def,
                )),
                Err(DbError::NoVisibleVersion { .. }) => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(Batch {
            schema: output.to_vec(),
            rows,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn index_nested_loop_join(
        &mut self,
        left: &PlanNode,
        right_table: &str,
        right_qualifier: &str,
        right_index_column: &str,
        left_key: &QExpr,
        join_type: JoinType,
        residual: &Option<QExpr>,
        output: &[ColumnRef],
    ) -> Result<Batch> {
        let left_batch = self.run(left)?;
        let left_len = left_batch.schema.len();
        let right_len = output.len() - left_len;

        // Item 51 Phase B: try to build an in-memory hash table over the inner
        // (right) relation. When the inner fits within the budget, O(1) hash
        // probe per outer row beats a B-tree search per outer row. Falls back to
        // the original INLJ path when the inner is too large.
        let budget = hash_join_inner_budget();
        if let Some(hash_table) = try_build_hash_table(
            right_table,
            right_index_column,
            &self.snapshot,
            self.ctx,
            budget,
        )? {
            let emit_unmatched_left = matches!(join_type, JoinType::Left);
            let mut out_rows = Vec::new();

            for lrow in &left_batch.rows {
                let key_lit = eval_qexpr(left_key, &left_batch.schema, lrow)?;
                let mut matched = false;
                if !matches!(key_lit, Literal::Null) {
                    if let Some(key_bytes) = join_key_bytes(&[key_lit]) {
                        if let Some(rrows) = hash_table.get(&key_bytes) {
                            for rrow in rrows {
                                let mut combined = lrow.clone();
                                combined.extend_from_slice(rrow);
                                if let Some(res) = residual {
                                    if !plan::eval_predicate(res, output, &combined)? {
                                        continue;
                                    }
                                }
                                out_rows.push(combined);
                                matched = true;
                            }
                        }
                    }
                }
                if !matched && emit_unmatched_left {
                    let mut combined = lrow.clone();
                    combined.extend(vec![Literal::Null; right_len]);
                    out_rows.push(combined);
                }
            }

            return Ok(Batch {
                schema: output.to_vec(),
                rows: out_rows,
            });
        }

        // Fallback: original index-nested-loop path (B-tree probe per outer row).
        let right_def = self.ctx.catalog.lookup(right_table)?.clone();
        let meta_page = right_def
            .columns
            .iter()
            .find(|c| {
                c.name == right_index_column
                    && !c.dropped
                    && ((matches!(c.index, Some(IndexKind::BTree)) && c.index_root.is_some())
                        || c.unique_index_root.is_some())
            })
            .and_then(|c| c.index_root.or(c.unique_index_root))
            .ok_or_else(|| {
                DbError::SqlPlan(format!(
                    "index-nested-loop join lost the B-Tree on {right_qualifier}.{right_index_column}"
                ))
            })?;
        let tree = DiskBTree::new(meta_page, self.ctx.page_size);
        let heap = Heap::open(
            self.ctx.page_size,
            right_def.fsm_meta,
            right_def.pages.clone(),
        );

        let emit_unmatched_left = matches!(join_type, JoinType::Left);
        let mut out_rows = Vec::new();

        for lrow in &left_batch.rows {
            let key_lit = eval_qexpr(left_key, &left_batch.schema, lrow)?;
            let mut matched = false;
            if !matches!(key_lit, Literal::Null) {
                if let Ok(value) = OrderedValue::try_from(&key_lit) {
                    for row_id in tree.search_eq(&value, self.ctx.pool)? {
                        let bytes =
                            match heap.get(row_id, &self.snapshot, self.ctx.xid, self.ctx.pool) {
                                Ok(b) => b,
                                Err(DbError::NoVisibleVersion { .. }) => continue,
                                Err(e) => return Err(e),
                            };
                        let rrow =
                            visible_row(&decode_row(&bytes, &right_def.columns)?, &right_def);
                        let mut combined = lrow.clone();
                        combined.extend_from_slice(&rrow);
                        if let Some(res) = residual {
                            if !plan::eval_predicate(res, output, &combined)? {
                                continue;
                            }
                        }
                        out_rows.push(combined);
                        matched = true;
                    }
                }
            }
            if !matched && emit_unmatched_left {
                let mut combined = lrow.clone();
                combined.extend(vec![Literal::Null; right_len]);
                out_rows.push(combined);
            }
        }

        Ok(Batch {
            schema: output.to_vec(),
            rows: out_rows,
        })
    }

    // ── expression evaluation with subqueries (P4.c) ─────────────────────────

    /// Evaluate `expr` against `row`. Subquery-free expressions delegate to the
    /// pure evaluator; subquery nodes are executed here with storage access.
    fn eval(&mut self, expr: &QExpr, schema: &[ColumnRef], row: &[Literal]) -> Result<Literal> {
        match expr {
            QExpr::Exists { subquery, negated } => {
                let batch = self.run_subquery(subquery, schema, row)?;
                let exists = !batch.rows.is_empty();
                Ok(Literal::Bool(exists != *negated))
            }
            QExpr::ScalarSubquery(subquery) => {
                let batch = self.run_subquery(subquery, schema, row)?;
                Ok(batch
                    .rows
                    .first()
                    .and_then(|r| r.first())
                    .cloned()
                    .unwrap_or(Literal::Null))
            }
            QExpr::InSubquery {
                expr: needle,
                subquery,
                negated,
            } => {
                let n = self.eval(needle, schema, row)?;
                if matches!(n, Literal::Null) {
                    return Ok(Literal::Null);
                }
                let batch = self.run_subquery(subquery, schema, row)?;
                let mut found = false;
                let mut saw_null = false;
                for r in &batch.rows {
                    match r.first() {
                        Some(Literal::Null) | None => saw_null = true,
                        Some(v) => {
                            if executor::compare(CmpOp::Eq, &n, v)? {
                                found = true;
                                break;
                            }
                        }
                    }
                }
                // SQL three-valued IN: a match -> true; no match with a NULL
                // present -> unknown (NULL); otherwise false. NOT IN inverts.
                if found {
                    Ok(Literal::Bool(!negated))
                } else if saw_null {
                    Ok(Literal::Null)
                } else {
                    Ok(Literal::Bool(*negated))
                }
            }
            // Structural nodes may contain subqueries in their children, so we
            // recurse here rather than delegating the whole subtree.
            QExpr::Compare { op, lhs, rhs } => {
                let l = self.eval(lhs, schema, row)?;
                let r = self.eval(rhs, schema, row)?;
                Ok(Literal::Bool(executor::compare(*op, &l, &r)?))
            }
            QExpr::And(lhs, rhs) => {
                if !executor::as_bool(&self.eval(lhs, schema, row)?)? {
                    return Ok(Literal::Bool(false));
                }
                Ok(Literal::Bool(executor::as_bool(
                    &self.eval(rhs, schema, row)?,
                )?))
            }
            QExpr::Or(lhs, rhs) => {
                if executor::as_bool(&self.eval(lhs, schema, row)?)? {
                    return Ok(Literal::Bool(true));
                }
                Ok(Literal::Bool(executor::as_bool(
                    &self.eval(rhs, schema, row)?,
                )?))
            }
            QExpr::Not(e) => Ok(Literal::Bool(!executor::as_bool(
                &self.eval(e, schema, row)?,
            )?)),
            QExpr::InList {
                expr: needle,
                list,
                negated,
            } => {
                let n = self.eval(needle, schema, row)?;
                if matches!(n, Literal::Null) {
                    return Ok(Literal::Null);
                }
                let mut found = false;
                for item in list {
                    let v = self.eval(item, schema, row)?;
                    if executor::compare(CmpOp::Eq, &n, &v)? {
                        found = true;
                        break;
                    }
                }
                Ok(Literal::Bool(found != *negated))
            }
            // Like / Match may contain correlated column refs in their
            // sub-expressions, so recurse through the ctx-aware evaluator.
            QExpr::Like {
                expr,
                pattern,
                negated,
                case_insensitive,
            } => {
                let val = self.eval(expr, schema, row)?;
                let pat = self.eval(pattern, schema, row)?;
                match (&val, &pat) {
                    (Literal::Null, _) | (_, Literal::Null) => Ok(Literal::Null),
                    (Literal::Text(t), Literal::Text(p)) => Ok(Literal::Bool(
                        executor::like_match(t, p, *case_insensitive) != *negated,
                    )),
                    _ => Err(crate::error::DbError::SqlUnsupported(format!(
                        "LIKE requires TEXT operands, got {val:?} LIKE {pat:?}"
                    ))),
                }
            }
            QExpr::Match { column, query } => {
                let col_val = self.eval(column, schema, row)?;
                let query_val = self.eval(query, schema, row)?;
                let text = match col_val {
                    Literal::Text(t) => t,
                    Literal::Null => return Ok(Literal::Null),
                    _ => return Ok(Literal::Bool(false)),
                };
                let query_str = match query_val {
                    Literal::Text(q) => q,
                    Literal::Null => return Ok(Literal::Null),
                    _ => return Ok(Literal::Bool(false)),
                };
                let query_tokens = crate::fulltext::tokenize(&query_str);
                if query_tokens.is_empty() {
                    return Ok(Literal::Bool(false));
                }
                let text_tokens: std::collections::HashSet<String> =
                    crate::fulltext::tokenize(&text).into_iter().collect();
                Ok(Literal::Bool(
                    query_tokens.iter().all(|t| text_tokens.contains(t)),
                ))
            }
            // G1 (item 19): CASE/COALESCE/NULLIF — recurse through the ctx-aware
            // evaluator in case sub-expressions contain subqueries.
            QExpr::Case {
                operand,
                conditions,
                else_result,
            } => {
                let op_val = match operand {
                    Some(op) => Some(self.eval(op, schema, row)?),
                    None => None,
                };
                for (cond, then) in conditions {
                    let matched = match &op_val {
                        None => executor::as_bool(&self.eval(cond, schema, row)?)?,
                        Some(v) => {
                            let c = self.eval(cond, schema, row)?;
                            executor::compare(crate::sql::logical::CmpOp::Eq, v, &c)?
                        }
                    };
                    if matched {
                        return self.eval(then, schema, row);
                    }
                }
                match else_result {
                    Some(e) => self.eval(e, schema, row),
                    None => Ok(Literal::Null),
                }
            }
            QExpr::Coalesce(args) => {
                for a in args {
                    let v = self.eval(a, schema, row)?;
                    if !matches!(v, Literal::Null) {
                        return Ok(v);
                    }
                }
                Ok(Literal::Null)
            }
            QExpr::Nullif { lhs, rhs } => {
                let a = self.eval(lhs, schema, row)?;
                let b = self.eval(rhs, schema, row)?;
                if executor::compare(crate::sql::logical::CmpOp::Eq, &a, &b)? {
                    Ok(Literal::Null)
                } else {
                    Ok(a)
                }
            }
            // G2 (item 19): CAST — recurse through the ctx-aware evaluator in
            // case the inner expr contains a subquery, then apply the cast.
            QExpr::Cast { expr, to_type } => {
                let val = self.eval(expr, schema, row)?;
                eval_cast(val, *to_type)
            }
            // No subquery below here: the pure evaluator handles it.
            QExpr::Column { .. }
            | QExpr::Literal(_)
            | QExpr::IsNull { .. }
            | QExpr::Aggregate { .. }
            | QExpr::Arith { .. } => eval_qexpr(expr, schema, row),
        }
    }

    /// Execute a subquery for a given outer row: substitute correlated outer
    /// columns with literals, then plan + run the (now uncorrelated) query.
    /// Uncorrelated subqueries bind to the same form every row and are cached.
    fn run_subquery(
        &mut self,
        subquery: &QuerySpec,
        outer_schema: &[ColumnRef],
        outer_row: &[Literal],
    ) -> Result<Batch> {
        let bound = self.bind_correlated(subquery, outer_schema, outer_row)?;
        let key = serde_json::to_string(&bound)
            .map_err(|e| DbError::SqlPlan(format!("subquery cache key: {e}")))?;
        if let Some(cached) = self.subquery_cache.get(&key) {
            return Ok(cached.clone());
        }
        let node = self.materialize_ctes_and_plan(&bound)?;
        let batch = self.run(&node)?;
        self.subquery_cache.insert(key, batch.clone());
        Ok(batch)
    }

    /// Replace every column reference in `subquery` that resolves to the outer
    /// query (correlation) — but not to the subquery's own FROM — with the
    /// outer row's literal value. Does not descend into deeper nested
    /// subqueries: each level binds against its own immediate outer.
    fn bind_correlated(
        &mut self,
        subquery: &QuerySpec,
        outer_schema: &[ColumnRef],
        outer_row: &[Literal],
    ) -> Result<QuerySpec> {
        let inner_schema =
            plan::plan_from_schema(&subquery.from, self.ctx.catalog.get(), &self.cte_schemas)?;
        let mut bound = subquery.clone();
        let subst =
            |e: &mut QExpr| substitute_correlated(e, &inner_schema, outer_schema, outer_row);
        if let Some(sel) = &mut bound.selection {
            subst(sel)?;
        }
        if let Some(h) = &mut bound.having {
            subst(h)?;
        }
        for g in &mut bound.group_by {
            subst(g)?;
        }
        for p in &mut bound.projection {
            if let crate::sql::query::Projection::Expr { expr, .. } = p {
                subst(expr)?;
            }
        }
        bind_correlated_from(&mut bound.from, &inner_schema, outer_schema, outer_row)?;
        Ok(bound)
    }
}

fn bind_correlated_from(
    node: &mut crate::sql::query::FromNode,
    inner: &[ColumnRef],
    outer: &[ColumnRef],
    outer_row: &[Literal],
) -> Result<()> {
    use crate::sql::query::FromNode;
    if let FromNode::Join {
        left, right, on, ..
    } = node
    {
        bind_correlated_from(left, inner, outer, outer_row)?;
        bind_correlated_from(right, inner, outer, outer_row)?;
        if let Some(on) = on {
            substitute_correlated(on, inner, outer, outer_row)?;
        }
    }
    Ok(())
}

/// Replace outer-correlated column references in `expr` with literals. A column
/// is correlated when it does *not* resolve in the subquery's own `inner`
/// schema but *does* resolve in the `outer` schema.
fn substitute_correlated(
    expr: &mut QExpr,
    inner: &[ColumnRef],
    outer: &[ColumnRef],
    outer_row: &[Literal],
) -> Result<()> {
    match expr {
        QExpr::Column { qualifier, name } => {
            let in_inner = resolve_column(inner, qualifier.as_deref(), name).is_ok();
            if !in_inner {
                if let Ok(idx) = resolve_column(outer, qualifier.as_deref(), name) {
                    *expr = QExpr::Literal(outer_row[idx].clone());
                }
            }
            Ok(())
        }
        QExpr::Literal(_) => Ok(()),
        QExpr::Compare { lhs, rhs, .. } | QExpr::And(lhs, rhs) | QExpr::Or(lhs, rhs) => {
            substitute_correlated(lhs, inner, outer, outer_row)?;
            substitute_correlated(rhs, inner, outer, outer_row)
        }
        QExpr::Not(e) | QExpr::IsNull { expr: e, .. } => {
            substitute_correlated(e, inner, outer, outer_row)
        }
        QExpr::Aggregate { arg, .. } => {
            if let Some(a) = arg {
                substitute_correlated(a, inner, outer, outer_row)?;
            }
            Ok(())
        }
        QExpr::InList { expr, list, .. } => {
            substitute_correlated(expr, inner, outer, outer_row)?;
            for e in list {
                substitute_correlated(e, inner, outer, outer_row)?;
            }
            Ok(())
        }
        QExpr::InSubquery { expr, .. } => substitute_correlated(expr, inner, outer, outer_row),
        // Deeper subqueries bind against their own immediate outer at run time.
        QExpr::Exists { .. } | QExpr::ScalarSubquery(_) => Ok(()),
        QExpr::Like { expr, pattern, .. } => {
            substitute_correlated(expr, inner, outer, outer_row)?;
            substitute_correlated(pattern, inner, outer, outer_row)
        }
        QExpr::Match { column, query } => {
            substitute_correlated(column, inner, outer, outer_row)?;
            substitute_correlated(query, inner, outer, outer_row)
        }
        QExpr::Arith { lhs, rhs, .. } => {
            substitute_correlated(lhs, inner, outer, outer_row)?;
            substitute_correlated(rhs, inner, outer, outer_row)
        }
        QExpr::Case {
            operand,
            conditions,
            else_result,
        } => {
            if let Some(op) = operand {
                substitute_correlated(op, inner, outer, outer_row)?;
            }
            for (cond, then) in conditions {
                substitute_correlated(cond, inner, outer, outer_row)?;
                substitute_correlated(then, inner, outer, outer_row)?;
            }
            if let Some(e) = else_result {
                substitute_correlated(e, inner, outer, outer_row)?;
            }
            Ok(())
        }
        QExpr::Coalesce(args) => {
            for a in args {
                substitute_correlated(a, inner, outer, outer_row)?;
            }
            Ok(())
        }
        QExpr::Nullif { lhs, rhs } => {
            substitute_correlated(lhs, inner, outer, outer_row)?;
            substitute_correlated(rhs, inner, outer, outer_row)
        }
        QExpr::Cast { expr, .. } => substitute_correlated(expr, inner, outer, outer_row),
    }
}

/// Keep only the values of non-dropped columns, in declaration order — matching
/// the `output` schema the planner built for a [`PlanNode::Scan`].
fn visible_row(full: &[Literal], table_def: &crate::catalog::TableDef) -> Vec<Literal> {
    full.iter()
        .zip(&table_def.columns)
        .filter(|(_, c)| !c.dropped)
        .map(|(v, _)| v.clone())
        .collect()
}
