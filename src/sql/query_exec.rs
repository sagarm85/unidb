//! Phase-4 query driver: turns a [`QuerySpec`] into a physical [`PlanNode`]
//! ([`crate::sql::plan::plan_query`]) and runs it against storage, producing an
//! [`ExecResult::Rows`]. Lives here (not in `plan.rs`/`join.rs`) because it is
//! the one part of Phase-4 execution that touches the engine: base scans read
//! heaps under the statement snapshot, and index-nested-loop probes the durable
//! on-disk B-Tree per outer row.
//!
//! The whole query runs under a **single** statement snapshot taken once here,
//! so every scan sees a consistent view (RC/RR both take one snapshot per
//! statement; a query is one statement).

use crate::btree_index::{DiskBTree, OrderedValue};
use crate::catalog::IndexKind;
use crate::error::{DbError, Result};
use crate::heap::Heap;
use crate::mvcc::Snapshot;
use crate::sql::executor::{decode_row, ExecCtx, ExecResult};
use crate::sql::join;
use crate::sql::logical::Literal;
use crate::sql::plan::{self, eval_qexpr, plan_query, Batch, ColumnRef, PlanNode};
use crate::sql::query::{JoinType, QExpr, QuerySpec};

pub fn exec_query(spec: &QuerySpec, ctx: &mut ExecCtx) -> Result<ExecResult> {
    let node = plan_query(spec, ctx.catalog)?;
    let snapshot = ctx.txn_mgr.snapshot_for_statement(ctx.xid)?;
    let batch = run_node(&node, &snapshot, ctx)?;
    Ok(ExecResult::Rows(batch.rows))
}

fn run_node(node: &PlanNode, snapshot: &Snapshot, ctx: &mut ExecCtx) -> Result<Batch> {
    match node {
        PlanNode::Scan { table, output, .. } => scan(table, output, snapshot, ctx),

        PlanNode::Filter {
            input, predicate, ..
        } => {
            let batch = run_node(input, snapshot, ctx)?;
            let mut rows = Vec::new();
            for row in batch.rows {
                if plan::eval_predicate(predicate, &batch.schema, &row)? {
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
            let batch = run_node(input, snapshot, ctx)?;
            let mut rows = Vec::with_capacity(batch.rows.len());
            for row in &batch.rows {
                let projected = items
                    .iter()
                    .map(|it| eval_qexpr(&it.expr, &batch.schema, row))
                    .collect::<Result<Vec<_>>>()?;
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
            let l = run_node(left, snapshot, ctx)?;
            let r = run_node(right, snapshot, ctx)?;
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
            let l = run_node(left, snapshot, ctx)?;
            let r = run_node(right, snapshot, ctx)?;
            join::merge_join(l, r, *join_type, left_keys, right_keys, residual)
        }

        PlanNode::NestedLoopJoin {
            left,
            right,
            join_type,
            on,
            ..
        } => {
            let l = run_node(left, snapshot, ctx)?;
            let r = run_node(right, snapshot, ctx)?;
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
        } => index_nested_loop_join(
            left,
            right_table,
            right_qualifier,
            right_index_column,
            left_key,
            *join_type,
            residual,
            output,
            snapshot,
            ctx,
        ),

        PlanNode::Aggregate {
            input,
            group_exprs,
            aggs,
            output,
        } => {
            let batch = run_node(input, snapshot, ctx)?;
            crate::sql::aggregate::aggregate(batch, group_exprs, aggs, output)
        }

        PlanNode::Distinct { input, output } => {
            let batch = run_node(input, snapshot, ctx)?;
            let mut seen = std::collections::HashSet::new();
            let mut rows = Vec::new();
            for row in batch.rows {
                let key = crate::sql::executor::encode_row(&row);
                if seen.insert(key) {
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
            let batch = run_node(input, snapshot, ctx)?;
            let rows =
                crate::sql::sort::sort_rows(batch.rows, keys, crate::sql::sort::sort_mem_rows())?;
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
            let batch = run_node(input, snapshot, ctx)?;
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

/// Full scan of a base table's live rows under `snapshot`, projected to its
/// visible columns in `output` order.
fn scan(
    table: &str,
    output: &[ColumnRef],
    snapshot: &Snapshot,
    ctx: &mut ExecCtx,
) -> Result<Batch> {
    let table_def = ctx.catalog.lookup(table)?.clone();
    let heap = Heap::from_pages(ctx.page_size, table_def.pages.clone());
    let mut rows = Vec::new();
    for (_, bytes) in heap.scan(snapshot, ctx.xid, ctx.pool)? {
        let full = decode_row(&bytes, &table_def.columns)?;
        rows.push(visible_row(&full, &table_def));
    }
    Ok(Batch {
        schema: output.to_vec(),
        rows,
    })
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

#[allow(clippy::too_many_arguments)]
fn index_nested_loop_join(
    left: &PlanNode,
    right_table: &str,
    right_qualifier: &str,
    right_index_column: &str,
    left_key: &QExpr,
    join_type: JoinType,
    residual: &Option<QExpr>,
    output: &[ColumnRef],
    snapshot: &Snapshot,
    ctx: &mut ExecCtx,
) -> Result<Batch> {
    let left_batch = run_node(left, snapshot, ctx)?;
    let left_len = left_batch.schema.len();
    let right_len = output.len() - left_len;

    let right_def = ctx.catalog.lookup(right_table)?.clone();
    let meta_page = right_def
        .columns
        .iter()
        .find(|c| {
            c.name == right_index_column && !c.dropped && matches!(c.index, Some(IndexKind::BTree))
        })
        .and_then(|c| c.index_root)
        .ok_or_else(|| {
            DbError::SqlPlan(format!(
                "index-nested-loop join lost the B-Tree on {right_qualifier}.{right_index_column}"
            ))
        })?;
    let tree = DiskBTree::new(meta_page, ctx.page_size);
    let heap = Heap::from_pages(ctx.page_size, right_def.pages.clone());

    let emit_unmatched_left = matches!(join_type, JoinType::Left);
    let mut out_rows = Vec::new();

    for lrow in &left_batch.rows {
        let key_lit = eval_qexpr(left_key, &left_batch.schema, lrow)?;
        let mut matched = false;
        if !matches!(key_lit, Literal::Null) {
            if let Ok(value) = OrderedValue::try_from(&key_lit) {
                for row_id in tree.search_eq(&value, ctx.pool)? {
                    let bytes = match heap.get(row_id, snapshot, ctx.xid, ctx.pool) {
                        Ok(b) => b,
                        // Not visible under this snapshot (superseded / aborted
                        // insert whose durable index entry survives) — skip.
                        Err(DbError::NoVisibleVersion { .. }) => continue,
                        Err(e) => return Err(e),
                    };
                    let rrow = visible_row(&decode_row(&bytes, &right_def.columns)?, &right_def);
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
