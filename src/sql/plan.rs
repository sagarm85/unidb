//! Phase-4 physical plan: the operator tree the executor runs for a
//! [`QuerySpec`], plus column resolution and [`QExpr`] evaluation over the
//! combined rows a join produces.
//!
//! Planning here is deliberately *rule-based* for P4.a — a left-deep tree in
//! FROM order, join algorithm chosen by a simple heuristic (index-nested-loop
//! when the inner side is a base table indexed on the join key, else hash
//! join). The **cost-based** join-order / algorithm / index-vs-scan choice is
//! P4.d ([`crate::sql::optimizer`]), which rewrites the tree built here. Keeping
//! the two apart means P4.a is correct and demoable on its own and the
//! optimizer is a pure improvement layered on top.

use crate::catalog::{Catalog, ColumnType, IndexKind};
use crate::error::{DbError, Result};
use crate::sql::executor;
use crate::sql::logical::Literal;
use crate::sql::query::{FromNode, JoinType, Projection, QExpr, QuerySpec};

/// One output column of an operator: the relation qualifier it came from, its
/// name, and its declared type (for EXPLAIN and future coercion).
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnRef {
    pub qualifier: String,
    pub name: String,
    pub ty: ColumnType,
}

/// A materialized result set flowing between operators. Materialized (not
/// streaming) — matching the engine's row/batch philosophy and "good enough,
/// not OLAP-class" scope; the spill paths (hash join, external sort) bound peak
/// memory where it matters.
#[derive(Debug, Clone)]
pub struct Batch {
    pub schema: Vec<ColumnRef>,
    pub rows: Vec<Vec<Literal>>,
}

/// A resolved SELECT-list item after planning: the expression to evaluate and
/// the output column name.
#[derive(Debug, Clone)]
pub struct ProjItem {
    pub expr: QExpr,
    pub name: String,
}

/// The physical operator tree. Every node carries its `output` schema, computed
/// at plan time so EXPLAIN and column resolution never need to re-derive it.
#[derive(Debug, Clone)]
pub enum PlanNode {
    /// Full scan of a base table's live rows under the statement snapshot.
    Scan {
        table: String,
        qualifier: String,
        output: Vec<ColumnRef>,
    },
    /// Block nested-loop join (cross joins and non-equi conditions).
    NestedLoopJoin {
        left: Box<PlanNode>,
        right: Box<PlanNode>,
        join_type: JoinType,
        on: Option<QExpr>,
        output: Vec<ColumnRef>,
    },
    /// Hash join on one or more equi-keys, with an optional non-equi residual.
    /// Spills to disk (Grace partitioning) when the build side exceeds the row
    /// budget — see [`crate::sql::join::hash_join`].
    HashJoin {
        left: Box<PlanNode>,
        right: Box<PlanNode>,
        join_type: JoinType,
        left_keys: Vec<QExpr>,
        right_keys: Vec<QExpr>,
        residual: Option<QExpr>,
        output: Vec<ColumnRef>,
    },
    /// Sort-merge join: both inputs are sorted on the join keys, then merged.
    /// Selected by the P4.d optimizer for pre-sorted / large inputs; the
    /// operator itself sorts its inputs so it is correct for any input order.
    MergeJoin {
        left: Box<PlanNode>,
        right: Box<PlanNode>,
        join_type: JoinType,
        left_keys: Vec<QExpr>,
        right_keys: Vec<QExpr>,
        residual: Option<QExpr>,
        output: Vec<ColumnRef>,
    },
    /// Index-nested-loop join: for each left row, probe the right base table's
    /// durable B-Tree on `right_index_column`. The Phase-3 durable index is the
    /// enabler — no rebuild, O(1) open, index-nested-loop over on-disk data.
    IndexNestedLoopJoin {
        left: Box<PlanNode>,
        right_table: String,
        right_qualifier: String,
        right_index_column: String,
        left_key: QExpr,
        join_type: JoinType,
        residual: Option<QExpr>,
        output: Vec<ColumnRef>,
    },
    Filter {
        input: Box<PlanNode>,
        predicate: QExpr,
        output: Vec<ColumnRef>,
    },
    Projection {
        input: Box<PlanNode>,
        items: Vec<ProjItem>,
        output: Vec<ColumnRef>,
    },
}

impl PlanNode {
    pub fn output(&self) -> &[ColumnRef] {
        match self {
            PlanNode::Scan { output, .. }
            | PlanNode::NestedLoopJoin { output, .. }
            | PlanNode::HashJoin { output, .. }
            | PlanNode::MergeJoin { output, .. }
            | PlanNode::IndexNestedLoopJoin { output, .. }
            | PlanNode::Filter { output, .. }
            | PlanNode::Projection { output, .. } => output,
        }
    }
}

/// Build the physical plan for a [`QuerySpec`] against the catalog. Pure (no
/// storage access): scans are placeholders the executor fills by reading heaps.
pub fn plan_query(spec: &QuerySpec, catalog: &Catalog) -> Result<PlanNode> {
    let mut node = plan_from(&spec.from, catalog)?;
    if let Some(sel) = &spec.selection {
        let output = node.output().to_vec();
        node = PlanNode::Filter {
            input: Box::new(node),
            predicate: sel.clone(),
            output,
        };
    }
    let items = resolve_projection(&spec.projection, node.output())?;
    let output = items
        .iter()
        .map(|it| ColumnRef {
            qualifier: String::new(),
            name: it.name.clone(),
            ty: ColumnType::Text, // projected type is not tracked past here
        })
        .collect();
    Ok(PlanNode::Projection {
        input: Box::new(node),
        items,
        output,
    })
}

fn plan_from(node: &FromNode, catalog: &Catalog) -> Result<PlanNode> {
    match node {
        FromNode::Table(tref) => {
            let def = catalog.lookup(&tref.table)?;
            let qualifier = tref.qualifier().to_string();
            let output = def
                .columns
                .iter()
                .filter(|c| !c.dropped)
                .map(|c| ColumnRef {
                    qualifier: qualifier.clone(),
                    name: c.name.clone(),
                    ty: c.ty,
                })
                .collect();
            Ok(PlanNode::Scan {
                table: tref.table.clone(),
                qualifier,
                output,
            })
        }
        FromNode::Join {
            left,
            right,
            join_type,
            on,
        } => {
            let left = plan_from(left, catalog)?;
            let right = plan_from(right, catalog)?;
            plan_join(left, right, *join_type, on.clone(), catalog)
        }
    }
}

fn plan_join(
    left: PlanNode,
    right: PlanNode,
    join_type: JoinType,
    on: Option<QExpr>,
    catalog: &Catalog,
) -> Result<PlanNode> {
    let mut output = left.output().to_vec();
    output.extend_from_slice(right.output());

    // Cross join or no condition: block nested loop.
    let Some(on) = on else {
        return Ok(PlanNode::NestedLoopJoin {
            left: Box::new(left),
            right: Box::new(right),
            join_type,
            on: None,
            output,
        });
    };

    // Split the ON condition into equi-key pairs (one column each side) and a
    // non-equi residual.
    let (left_keys, right_keys, residual) =
        split_join_condition(&on, left.output(), right.output())?;

    if left_keys.is_empty() {
        // No usable equi-key: block nested loop with the full condition.
        return Ok(PlanNode::NestedLoopJoin {
            left: Box::new(left),
            right: Box::new(right),
            join_type,
            on: Some(on),
            output,
        });
    }

    // Index-nested-loop when the right (inner) side is a plain base-table scan
    // whose single join column carries a durable B-Tree index, and the join is
    // Inner or Left (Right would need the *left* indexed; the optimizer can
    // commute — for P4.a we keep it simple). This is the Phase-3-durable-index
    // win: probe the on-disk tree per outer row, no full inner scan.
    if left_keys.len() == 1 && matches!(join_type, JoinType::Inner | JoinType::Left) {
        if let PlanNode::Scan {
            table, qualifier, ..
        } = &right
        {
            if let QExpr::Column { name, .. } = &right_keys[0] {
                if base_column_has_btree(catalog, table, name) {
                    return Ok(PlanNode::IndexNestedLoopJoin {
                        left: Box::new(left),
                        right_table: table.clone(),
                        right_qualifier: qualifier.clone(),
                        right_index_column: name.clone(),
                        left_key: left_keys.into_iter().next().unwrap(),
                        join_type,
                        residual,
                        output,
                    });
                }
            }
        }
    }

    Ok(PlanNode::HashJoin {
        left: Box::new(left),
        right: Box::new(right),
        join_type,
        left_keys,
        right_keys,
        residual,
        output,
    })
}

fn base_column_has_btree(catalog: &Catalog, table: &str, column: &str) -> bool {
    catalog
        .lookup(table)
        .ok()
        .and_then(|def| def.columns.iter().find(|c| c.name == column && !c.dropped))
        .map(|c| matches!(c.index, Some(IndexKind::BTree)) && c.index_root.is_some())
        .unwrap_or(false)
}

/// Classify an ON condition's top-level conjuncts into equi-join key pairs
/// (each side referencing exactly one input) and a leftover residual predicate.
/// `left_keys[i] = right_keys[i]` are the equi-keys; an equi conjunct with both
/// columns on the same side, or any non-equi conjunct, goes to the residual.
type SplitCondition = (Vec<QExpr>, Vec<QExpr>, Option<QExpr>);

fn split_join_condition(
    on: &QExpr,
    left_schema: &[ColumnRef],
    right_schema: &[ColumnRef],
) -> Result<SplitCondition> {
    let mut left_keys = Vec::new();
    let mut right_keys = Vec::new();
    let mut residual: Option<QExpr> = None;
    let push_residual = |e: &QExpr, res: &mut Option<QExpr>| {
        *res = Some(match res.take() {
            Some(existing) => QExpr::And(Box::new(existing), Box::new(e.clone())),
            None => e.clone(),
        });
    };

    for conj in on.conjuncts() {
        if let Some((a, b)) = conj.as_equi() {
            let a_side = column_side(a, left_schema, right_schema);
            let b_side = column_side(b, left_schema, right_schema);
            match (a_side, b_side) {
                (Some(Side::Left), Some(Side::Right)) => {
                    left_keys.push(a.clone());
                    right_keys.push(b.clone());
                    continue;
                }
                (Some(Side::Right), Some(Side::Left)) => {
                    left_keys.push(b.clone());
                    right_keys.push(a.clone());
                    continue;
                }
                _ => {}
            }
        }
        push_residual(conj, &mut residual);
    }
    Ok((left_keys, right_keys, residual))
}

enum Side {
    Left,
    Right,
}

/// Which input a column reference belongs to, resolving against each schema.
/// `None` if it resolves in neither or both (ambiguous / unknown) — such a
/// conjunct falls through to the residual, which is always correct (just not an
/// equi-key).
fn column_side(expr: &QExpr, left: &[ColumnRef], right: &[ColumnRef]) -> Option<Side> {
    if let QExpr::Column { qualifier, name } = expr {
        let in_left = resolve_column(left, qualifier.as_deref(), name).is_ok();
        let in_right = resolve_column(right, qualifier.as_deref(), name).is_ok();
        match (in_left, in_right) {
            (true, false) => Some(Side::Left),
            (false, true) => Some(Side::Right),
            _ => None,
        }
    } else {
        None
    }
}

/// Resolve `[qualifier.]name` to a column index in `schema`. Errors on an
/// unknown column, and on an ambiguous unqualified name (present in more than
/// one relation) — the standard SQL rule.
pub fn resolve_column(schema: &[ColumnRef], qualifier: Option<&str>, name: &str) -> Result<usize> {
    let mut found = None;
    for (i, col) in schema.iter().enumerate() {
        let name_ok = col.name == name;
        let qual_ok = qualifier.is_none_or(|q| q.eq_ignore_ascii_case(&col.qualifier));
        if name_ok && qual_ok {
            if found.is_some() {
                return Err(DbError::SqlPlan(format!(
                    "column reference '{name}' is ambiguous"
                )));
            }
            found = Some(i);
        }
    }
    found.ok_or_else(|| DbError::ColumnNotFound {
        table: qualifier.unwrap_or("").to_string(),
        column: name.to_string(),
    })
}

fn resolve_projection(projection: &[Projection], schema: &[ColumnRef]) -> Result<Vec<ProjItem>> {
    let mut items = Vec::new();
    for proj in projection {
        match proj {
            Projection::Wildcard => {
                for col in schema {
                    items.push(ProjItem {
                        expr: QExpr::Column {
                            qualifier: Some(col.qualifier.clone()),
                            name: col.name.clone(),
                        },
                        name: col.name.clone(),
                    });
                }
            }
            Projection::QualifiedWildcard(q) => {
                let mut any = false;
                for col in schema {
                    if col.qualifier.eq_ignore_ascii_case(q) {
                        any = true;
                        items.push(ProjItem {
                            expr: QExpr::Column {
                                qualifier: Some(col.qualifier.clone()),
                                name: col.name.clone(),
                            },
                            name: col.name.clone(),
                        });
                    }
                }
                if !any {
                    return Err(DbError::SqlPlan(format!("unknown relation '{q}' in {q}.*")));
                }
            }
            Projection::Expr { expr, alias } => {
                let name = alias.clone().unwrap_or_else(|| default_expr_name(expr));
                // Validate columns resolve now (fail fast, before execution).
                validate_expr(expr, schema)?;
                items.push(ProjItem {
                    expr: expr.clone(),
                    name,
                });
            }
        }
    }
    Ok(items)
}

fn default_expr_name(expr: &QExpr) -> String {
    match expr {
        QExpr::Column { name, .. } => name.clone(),
        _ => "?column?".to_string(),
    }
}

/// Recursively check that every column reference in `expr` resolves against
/// `schema` — surfaces planning errors before the executor runs.
fn validate_expr(expr: &QExpr, schema: &[ColumnRef]) -> Result<()> {
    match expr {
        QExpr::Column { qualifier, name } => {
            resolve_column(schema, qualifier.as_deref(), name).map(|_| ())
        }
        QExpr::Literal(_) => Ok(()),
        QExpr::Compare { lhs, rhs, .. } | QExpr::And(lhs, rhs) | QExpr::Or(lhs, rhs) => {
            validate_expr(lhs, schema)?;
            validate_expr(rhs, schema)
        }
        QExpr::Not(e) | QExpr::IsNull { expr: e, .. } => validate_expr(e, schema),
    }
}

// ── QExpr evaluation over a combined row ─────────────────────────────────────

/// Evaluate `expr` against `row` interpreted per `schema`. Reuses the executor's
/// `compare` so join/where semantics match the single-table path exactly
/// (same type coercions, same NULL convention).
pub fn eval_qexpr(expr: &QExpr, schema: &[ColumnRef], row: &[Literal]) -> Result<Literal> {
    match expr {
        QExpr::Column { qualifier, name } => {
            let idx = resolve_column(schema, qualifier.as_deref(), name)?;
            Ok(row[idx].clone())
        }
        QExpr::Literal(l) => Ok(l.clone()),
        QExpr::Compare { op, lhs, rhs } => {
            let l = eval_qexpr(lhs, schema, row)?;
            let r = eval_qexpr(rhs, schema, row)?;
            Ok(Literal::Bool(executor::compare(*op, &l, &r)?))
        }
        QExpr::And(lhs, rhs) => {
            let l = executor::as_bool(&eval_qexpr(lhs, schema, row)?)?;
            // Short-circuit: don't force the RHS type if the LHS already fails.
            if !l {
                return Ok(Literal::Bool(false));
            }
            Ok(Literal::Bool(executor::as_bool(&eval_qexpr(
                rhs, schema, row,
            )?)?))
        }
        QExpr::Or(lhs, rhs) => {
            let l = executor::as_bool(&eval_qexpr(lhs, schema, row)?)?;
            if l {
                return Ok(Literal::Bool(true));
            }
            Ok(Literal::Bool(executor::as_bool(&eval_qexpr(
                rhs, schema, row,
            )?)?))
        }
        QExpr::Not(e) => {
            let v = executor::as_bool(&eval_qexpr(e, schema, row)?)?;
            Ok(Literal::Bool(!v))
        }
        QExpr::IsNull { expr, negated } => {
            let v = eval_qexpr(expr, schema, row)?;
            let is_null = matches!(v, Literal::Null);
            Ok(Literal::Bool(is_null != *negated))
        }
    }
}

/// Evaluate a predicate to a bool (NULL / non-bool -> false, matching the
/// single-table `predicate_matches`).
pub fn eval_predicate(expr: &QExpr, schema: &[ColumnRef], row: &[Literal]) -> Result<bool> {
    executor::as_bool(&eval_qexpr(expr, schema, row)?)
}

/// Encode a tuple of join-key literals into an equality/hash key. Two keys are
/// equal iff their bytes are equal — exact for same-typed keys (the common
/// case: FK int = PK int). Cross-type numeric equi-joins (`int = decimal`) are
/// a documented v1 limitation; declare matching key types.
pub fn join_key_bytes(key: &[Literal]) -> Option<Vec<u8>> {
    // A NULL component never equi-matches (SQL semantics): signal "no key".
    if key.iter().any(|l| matches!(l, Literal::Null)) {
        return None;
    }
    Some(executor::encode_row(key))
}

/// Ordering between two join-key tuples for merge join / sort. `None` if any
/// component is NULL or unorderable.
pub fn key_ord(a: &[Literal], b: &[Literal]) -> Option<std::cmp::Ordering> {
    use std::cmp::Ordering;
    for (x, y) in a.iter().zip(b) {
        match executor::literal_ord(x, y)? {
            Ordering::Equal => continue,
            other => return Some(other),
        }
    }
    Some(Ordering::Equal)
}

/// The default hash-join build-side row budget before spilling to disk.
/// Overridable via `UNIDB_HASH_JOIN_MEM_ROWS` (tests force spill with a small
/// value). Not a memory cap in bytes — a row count, deliberately coarse.
pub fn hash_join_mem_rows() -> usize {
    std::env::var("UNIDB_HASH_JOIN_MEM_ROWS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(1_000_000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{Catalog, ColumnConstraints, ColumnDef, TableDef};
    use crate::sql::logical::CmpOp;
    use crate::sql::query::{FromNode, JoinType, Projection, TableRef};

    fn col(name: &str, index: Option<IndexKind>, root: Option<u32>) -> ColumnDef {
        ColumnDef {
            name: name.to_string(),
            ty: ColumnType::Int64,
            index,
            index_root: root,
            constraints: ColumnConstraints::default(),
            dropped: false,
        }
    }

    fn table(name: &str, columns: Vec<ColumnDef>) -> TableDef {
        TableDef {
            name: name.to_string(),
            columns,
            pages: vec![],
            rls_policy: None,
            events_enabled: false,
            constraints: Default::default(),
            serial_next: Default::default(),
        }
    }

    fn join_spec() -> QuerySpec {
        QuerySpec {
            from: FromNode::Join {
                left: Box::new(FromNode::Table(TableRef {
                    table: "customers".into(),
                    alias: None,
                })),
                right: Box::new(FromNode::Table(TableRef {
                    table: "orders".into(),
                    alias: None,
                })),
                join_type: JoinType::Inner,
                on: Some(QExpr::Compare {
                    op: CmpOp::Eq,
                    lhs: Box::new(QExpr::Column {
                        qualifier: Some("customers".into()),
                        name: "id".into(),
                    }),
                    rhs: Box::new(QExpr::Column {
                        qualifier: Some("orders".into()),
                        name: "customer_id".into(),
                    }),
                }),
            },
            selection: None,
            projection: vec![Projection::Wildcard],
        }
    }

    fn inner_join_node(catalog: &Catalog) -> PlanNode {
        match plan_query(&join_spec(), catalog).unwrap() {
            PlanNode::Projection { input, .. } => *input,
            other => panic!("expected Projection root, got {other:?}"),
        }
    }

    #[test]
    fn planner_picks_index_nested_loop_when_inner_is_indexed() {
        let mut catalog = Catalog::new();
        catalog.insert_for_test(table("customers", vec![col("id", None, None)]));
        // orders.customer_id has a durable BTree index (meta page 7).
        catalog.insert_for_test(table(
            "orders",
            vec![col("customer_id", Some(IndexKind::BTree), Some(7))],
        ));
        assert!(matches!(
            inner_join_node(&catalog),
            PlanNode::IndexNestedLoopJoin { .. }
        ));
    }

    #[test]
    fn planner_picks_hash_join_when_inner_unindexed() {
        let mut catalog = Catalog::new();
        catalog.insert_for_test(table("customers", vec![col("id", None, None)]));
        catalog.insert_for_test(table("orders", vec![col("customer_id", None, None)]));
        assert!(matches!(
            inner_join_node(&catalog),
            PlanNode::HashJoin { .. }
        ));
    }
}
