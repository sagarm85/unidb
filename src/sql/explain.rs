//! P4.e `EXPLAIN` / `EXPLAIN ANALYZE`: render the chosen physical plan tree.
//!
//! `EXPLAIN` shows the operator tree with **estimated** output rows (from
//! `ANALYZE` statistics where available). `EXPLAIN ANALYZE` additionally runs
//! the query and reports the **actual** rows returned and wall-clock time — the
//! executor drives that part; this module owns the estimate + rendering.
//!
//! Per-operator actual row counts / timings are a documented follow-up; v1
//! reports the estimated tree plus total actual rows + execution time.

use crate::catalog::Catalog;
use crate::sql::plan::PlanNode;
use crate::sql::query::JoinType;

/// Render the plan tree as indented lines with estimated row counts.
pub fn render_estimated(node: &PlanNode, catalog: &Catalog) -> Vec<String> {
    let mut lines = Vec::new();
    render(node, catalog, 0, &mut lines);
    lines
}

fn render(node: &PlanNode, catalog: &Catalog, depth: usize, lines: &mut Vec<String>) {
    let indent = "  ".repeat(depth);
    let est = estimate_rows(node, catalog);
    lines.push(format!("{indent}{}  (est_rows={:.0})", label(node), est));
    for child in children(node) {
        render(child, catalog, depth + 1, lines);
    }
}

/// One-line label for an operator: its kind plus the salient detail.
pub fn label(node: &PlanNode) -> String {
    match node {
        PlanNode::Scan { table, .. } => format!("Scan {table}"),
        PlanNode::IndexScan {
            table, column, op, ..
        } => format!("IndexScan {table} on {column} {}", op_str(*op)),
        PlanNode::CteScan { name, .. } => format!("CteScan {name}"),
        PlanNode::NestedLoopJoin { join_type, .. } => {
            format!("NestedLoopJoin ({})", join_str(*join_type))
        }
        PlanNode::HashJoin { join_type, .. } => format!("HashJoin ({})", join_str(*join_type)),
        PlanNode::MergeJoin { join_type, .. } => format!("MergeJoin ({})", join_str(*join_type)),
        PlanNode::IndexNestedLoopJoin {
            right_table,
            right_index_column,
            join_type,
            ..
        } => format!(
            "IndexNestedLoopJoin ({}) probe {right_table}.{right_index_column}",
            join_str(*join_type)
        ),
        PlanNode::Filter { .. } => "Filter".to_string(),
        PlanNode::Projection { items, .. } => format!("Projection [{} cols]", items.len()),
        PlanNode::Aggregate {
            group_exprs, aggs, ..
        } => format!(
            "HashAggregate (groups={}, aggs={})",
            group_exprs.len(),
            aggs.len()
        ),
        PlanNode::Distinct { .. } => "Distinct".to_string(),
        PlanNode::Sort { keys, .. } => format!("Sort ({} keys)", keys.len()),
        PlanNode::Limit { limit, offset, .. } => {
            format!("Limit (limit={:?}, offset={offset})", limit)
        }
        // G8: virtual single-row source for SELECT without FROM.
        PlanNode::Dual => "Dual".to_string(),
    }
}

fn children(node: &PlanNode) -> Vec<&PlanNode> {
    match node {
        PlanNode::Scan { .. }
        | PlanNode::IndexScan { .. }
        | PlanNode::CteScan { .. }
        | PlanNode::Dual => vec![],
        PlanNode::NestedLoopJoin { left, right, .. }
        | PlanNode::HashJoin { left, right, .. }
        | PlanNode::MergeJoin { left, right, .. } => vec![left, right],
        PlanNode::IndexNestedLoopJoin { left, .. } => vec![left],
        PlanNode::Filter { input, .. }
        | PlanNode::Projection { input, .. }
        | PlanNode::Aggregate { input, .. }
        | PlanNode::Distinct { input, .. }
        | PlanNode::Sort { input, .. }
        | PlanNode::Limit { input, .. } => vec![input],
    }
}

/// Coarse output-cardinality estimate for a node (uses `ANALYZE` stats at the
/// leaves; applies default selectivities up the tree).
pub fn estimate_rows(node: &PlanNode, catalog: &Catalog) -> f64 {
    match node {
        PlanNode::Scan { table, .. } => catalog
            .table_stats(table)
            .map(|s| s.row_count as f64)
            .unwrap_or(1000.0),
        PlanNode::IndexScan {
            table,
            column,
            op,
            value,
            ..
        } => {
            let base = catalog.table_stats(table);
            match base {
                Some(stats) => {
                    let sel = stats
                        .columns
                        .get(column)
                        .and_then(|c| c.selectivity(*op, value, stats.row_count))
                        .unwrap_or(0.1);
                    (stats.row_count as f64 * sel).max(1.0)
                }
                None => 50.0,
            }
        }
        PlanNode::CteScan { .. } => 100.0,
        PlanNode::Filter { input, .. } => estimate_rows(input, catalog) * 0.33,
        PlanNode::Projection { input, .. } => estimate_rows(input, catalog),
        PlanNode::NestedLoopJoin { left, right, .. } => {
            estimate_rows(left, catalog) * estimate_rows(right, catalog)
        }
        PlanNode::HashJoin { left, right, .. } | PlanNode::MergeJoin { left, right, .. } => {
            // Equi-join: assume the smaller side's keys mostly match.
            let l = estimate_rows(left, catalog);
            let r = estimate_rows(right, catalog);
            l.max(r)
        }
        PlanNode::IndexNestedLoopJoin { left, .. } => estimate_rows(left, catalog),
        PlanNode::Aggregate {
            input, group_exprs, ..
        } => {
            let input_rows = estimate_rows(input, catalog);
            if group_exprs.is_empty() {
                1.0
            } else {
                (input_rows * 0.5).max(1.0)
            }
        }
        PlanNode::Distinct { input, .. } => (estimate_rows(input, catalog) * 0.5).max(1.0),
        PlanNode::Sort { input, .. } => estimate_rows(input, catalog),
        PlanNode::Limit { input, limit, .. } => {
            let n = estimate_rows(input, catalog);
            match limit {
                Some(l) => n.min(*l as f64),
                None => n,
            }
        }
        // G8: Dual emits exactly one row.
        PlanNode::Dual => 1.0,
    }
}

fn op_str(op: crate::sql::logical::CmpOp) -> &'static str {
    use crate::sql::logical::CmpOp::*;
    match op {
        Eq => "=",
        Ne => "!=",
        Lt => "<",
        Gt => ">",
        Le => "<=",
        Ge => ">=",
    }
}

fn join_str(j: JoinType) -> &'static str {
    match j {
        JoinType::Inner => "inner",
        JoinType::Left => "left",
        JoinType::Right => "right",
        JoinType::Cross => "cross",
    }
}
