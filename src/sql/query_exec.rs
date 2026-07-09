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

use std::collections::HashMap;

use crate::btree_index::{DiskBTree, OrderedValue};
use crate::catalog::IndexKind;
use crate::error::{DbError, Result};
use crate::heap::Heap;
use crate::mvcc::Snapshot;
use crate::sql::executor::{self, decode_row, ExecCtx, ExecResult};
use crate::sql::join;
use crate::sql::logical::{CmpOp, Literal};
use crate::sql::plan::{
    self, eval_qexpr, plan_query, resolve_column, Batch, ColumnRef, CteSchemas, PlanNode,
};
use crate::sql::query::{JoinType, QExpr, QuerySpec};

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
    let mut lines = crate::sql::explain::render_estimated(&node, runner.ctx.catalog);
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
            let cte_plan = plan_query(cte_spec, self.ctx.catalog, &self.cte_schemas)?;
            let batch = self.run(&cte_plan)?;
            self.cte_schemas.insert(name.clone(), batch.schema.clone());
            self.cte_batches.insert(name.clone(), batch);
        }
        plan_query(spec, self.ctx.catalog, &self.cte_schemas)
    }

    fn run(&mut self, node: &PlanNode) -> Result<Batch> {
        match node {
            PlanNode::Scan { table, output, .. } => self.scan(table, output),

            PlanNode::IndexScan {
                table,
                column,
                op,
                value,
                output,
                ..
            } => self.index_scan(table, column, *op, value, output),

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
        }
    }

    fn scan(&mut self, table: &str, output: &[ColumnRef]) -> Result<Batch> {
        let table_def = self.ctx.catalog.lookup(table)?.clone();
        let heap = Heap::open(
            self.ctx.page_size,
            table_def.fsm_meta,
            table_def.pages.clone(),
        );
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
            let full = decode_row(&bytes, &table_def.columns)?;
            rows.push(visible_row(&full, &table_def));
        }
        Ok(Batch {
            schema: output.to_vec(),
            rows,
        })
    }

    /// Index scan (P4.d): probe the column's durable B-Tree for `column <op>
    /// value`, fetch the matching rows under the snapshot, project to visible
    /// columns. Falls back to a full scan if the index isn't usable.
    fn index_scan(
        &mut self,
        table: &str,
        column: &str,
        op: CmpOp,
        value: &Literal,
        output: &[ColumnRef],
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

        let right_def = self.ctx.catalog.lookup(right_table)?.clone();
        let meta_page = right_def
            .columns
            .iter()
            .find(|c| {
                c.name == right_index_column
                    && !c.dropped
                    && matches!(c.index, Some(IndexKind::BTree))
            })
            .and_then(|c| c.index_root)
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
            // No subquery below here: the pure evaluator handles it.
            QExpr::Column { .. }
            | QExpr::Literal(_)
            | QExpr::IsNull { .. }
            | QExpr::Aggregate { .. } => eval_qexpr(expr, schema, row),
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
            plan::plan_from_schema(&subquery.from, self.ctx.catalog, &self.cte_schemas)?;
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
