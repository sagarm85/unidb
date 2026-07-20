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
use crate::sql::logical::{CmpOp, Literal};
use crate::sql::query::{AggFunc, FromNode, JoinType, OrderKey, Projection, QExpr, QuerySpec};

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
    /// G8 (item 19): `SELECT` without `FROM`. Yields exactly one row with zero
    /// columns so the projection above it can evaluate pure literal / arithmetic
    /// expressions. Analogous to Oracle's `DUAL` table or SQLite's virtual row.
    Dual,
    /// Scan of a materialized CTE (P4.c): the executor reads its rows from the
    /// once-computed CTE batch rather than a heap.
    CteScan {
        name: String,
        qualifier: String,
        output: Vec<ColumnRef>,
    },
    /// Index scan (P4.d): probe a base table's durable B-Tree for the rows
    /// matching `column <op> value`, then fetch them. Chosen by the cost-based
    /// optimizer when the predicate is estimated selective.
    ///
    /// When `index_only` is true the projection contains only the indexed
    /// column itself, so the B-tree leaf already holds the value and no heap
    /// fetch is needed (Item 102-A).
    IndexScan {
        table: String,
        qualifier: String,
        column: String,
        op: crate::sql::logical::CmpOp,
        value: Literal,
        output: Vec<ColumnRef>,
        /// Item 102-A: true when every projected column equals `column` —
        /// the B-tree leaf value is sufficient; heap fetch is skipped.
        index_only: bool,
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
    /// Hash aggregation (P4.b): group `input` by `group_exprs` and compute
    /// `aggs` per group. Output schema is the group columns (`__g0`..) followed
    /// by the aggregate columns (`__a0`..); the surrounding projection/having/
    /// order expressions are rewritten to reference those synthetic names.
    Aggregate {
        input: Box<PlanNode>,
        group_exprs: Vec<QExpr>,
        aggs: Vec<AggCall>,
        output: Vec<ColumnRef>,
    },
    /// `SELECT DISTINCT` (P4.b): dedupe whole output rows.
    Distinct {
        input: Box<PlanNode>,
        output: Vec<ColumnRef>,
    },
    /// `ORDER BY` (P4.b): sort by output-column keys. Spills to an external
    /// merge sort past a row budget — see [`crate::sql::sort`].
    Sort {
        input: Box<PlanNode>,
        keys: Vec<SortKey>,
        output: Vec<ColumnRef>,
    },
    /// `LIMIT` / `OFFSET` (P4.b).
    Limit {
        input: Box<PlanNode>,
        limit: Option<usize>,
        offset: usize,
        output: Vec<ColumnRef>,
    },
}

/// One aggregate to compute (P4.b): function, argument expression (`None` for
/// `COUNT(*)`), and whether the argument is de-duplicated first.
#[derive(Debug, Clone)]
pub struct AggCall {
    pub func: AggFunc,
    pub arg: Option<QExpr>,
    pub distinct: bool,
}

/// One `ORDER BY` key resolved to an output-column index + direction (P4.b).
#[derive(Debug, Clone, Copy)]
pub struct SortKey {
    pub column: usize,
    pub asc: bool,
}

impl PlanNode {
    pub fn output(&self) -> &[ColumnRef] {
        match self {
            PlanNode::Scan { output, .. }
            | PlanNode::CteScan { output, .. }
            | PlanNode::IndexScan { output, .. }
            | PlanNode::NestedLoopJoin { output, .. }
            | PlanNode::HashJoin { output, .. }
            | PlanNode::MergeJoin { output, .. }
            | PlanNode::IndexNestedLoopJoin { output, .. }
            | PlanNode::Filter { output, .. }
            | PlanNode::Projection { output, .. }
            | PlanNode::Aggregate { output, .. }
            | PlanNode::Distinct { output, .. }
            | PlanNode::Sort { output, .. }
            | PlanNode::Limit { output, .. } => output,
            // G8: Dual has zero input columns; projection above it supplies them.
            PlanNode::Dual => &[],
        }
    }
}

/// Output schemas of the CTEs visible to a query, keyed by CTE name. Column
/// names carry an empty qualifier; a [`PlanNode::CteScan`] requalifies them.
pub type CteSchemas = std::collections::HashMap<String, Vec<ColumnRef>>;

/// Build the physical plan for a [`QuerySpec`] against the catalog and the
/// visible CTEs. Pure (no storage access): scans are placeholders the executor
/// fills by reading heaps (or the materialized CTE batch for a `CteScan`).
///
/// Pipeline (SQL logical order): FROM → WHERE → [Aggregate → HAVING] →
/// Projection → DISTINCT → ORDER BY → LIMIT/OFFSET.
pub fn plan_query(spec: &QuerySpec, catalog: &Catalog, ctes: &CteSchemas) -> Result<PlanNode> {
    // The cost-based optimizer (P4.d) builds the join + base-access + WHERE
    // subtree: join order + algorithm from statistics, and index-vs-scan for
    // base access. It falls back to the rule-based `plan_from` + `Filter`
    // whenever it can't improve (no stats, outer joins, CTE relations).
    let mut node = crate::sql::optimizer::plan_access(&spec.from, &spec.selection, catalog, ctes)?;

    // Does this query aggregate? Either an explicit GROUP BY, or an aggregate
    // anywhere in the SELECT list / HAVING / ORDER BY.
    let proj_has_agg = spec.projection.iter().any(|p| match p {
        Projection::Expr { expr, .. } => expr.has_aggregate(),
        _ => false,
    });
    let having_has_agg = spec.having.as_ref().is_some_and(|h| h.has_aggregate());
    let has_agg = !spec.group_by.is_empty() || proj_has_agg || having_has_agg;

    let items = if has_agg {
        node = plan_aggregate(node, spec)?;
        let (group_exprs, aggs) = match &node {
            PlanNode::Aggregate {
                group_exprs, aggs, ..
            } => (group_exprs.clone(), aggs.clone()),
            _ => unreachable!("plan_aggregate returns an Aggregate node"),
        };
        let agg_schema = node.output().to_vec();
        if let Some(having) = &spec.having {
            let pred = rewrite_over_agg(having, &group_exprs, &aggs)?;
            let output = agg_schema.clone();
            node = PlanNode::Filter {
                input: Box::new(node),
                predicate: pred,
                output,
            };
        }
        resolve_projection_agg(&spec.projection, spec, &agg_schema)?
    } else {
        resolve_projection(&spec.projection, node.output())?
    };

    // G4 (item 19): ORDER BY on a non-projected expression. Standard SQL
    // requires sorting over the pre-projection schema when the ORDER BY key
    // isn't in the SELECT list. Strategy: if any ORDER BY key resolves against
    // the input schema but not the projected output, sort *before* projecting.
    //
    // For non-aggregate queries without DISTINCT, we resolve ORDER BY keys
    // against both the input schema (pre-projection) and the projected output:
    // - Keys found in the projected output: resolved by name/position as before.
    // - Keys found only in the input: sort before the projection, then project.
    //
    // DISTINCT + ORDER BY on non-projected cols is not supported (SQL standard
    // forbids it, Postgres rejects it too).
    let pre_proj_schema = node.output().to_vec();
    let proj_output: Vec<ColumnRef> = items
        .iter()
        .map(|it| ColumnRef {
            qualifier: String::new(),
            name: it.name.clone(),
            ty: ColumnType::Text, // projected type is not tracked past here
        })
        .collect();

    if !spec.order_by.is_empty() && !has_agg && !spec.distinct {
        // Check whether any ORDER BY key is absent from the projected output.
        let needs_pre_proj_sort = spec
            .order_by
            .iter()
            .any(|k| order_key_needs_pre_proj_sort(&k.expr, &proj_output, &pre_proj_schema));
        if needs_pre_proj_sort {
            // Sort over the pre-projection schema, then project.
            let keys = resolve_order_by_pre_proj(&spec.order_by, &pre_proj_schema, &proj_output)?;
            node = PlanNode::Sort {
                input: Box::new(node),
                keys,
                output: pre_proj_schema.clone(),
            };
            node = PlanNode::Projection {
                input: Box::new(node),
                items,
                output: proj_output.clone(),
            };
            if spec.limit.is_some() || spec.offset > 0 {
                node = PlanNode::Limit {
                    input: Box::new(node),
                    limit: spec.limit,
                    offset: spec.offset,
                    output: proj_output,
                };
            }
            return Ok(node);
        }
    }

    node = PlanNode::Projection {
        input: Box::new(node),
        items,
        output: proj_output.clone(),
    };

    if spec.distinct {
        node = PlanNode::Distinct {
            input: Box::new(node),
            output: proj_output.clone(),
        };
    }

    if !spec.order_by.is_empty() {
        let keys = resolve_order_by(&spec.order_by, &proj_output)?;
        node = PlanNode::Sort {
            input: Box::new(node),
            keys,
            output: proj_output.clone(),
        };
    }

    if spec.limit.is_some() || spec.offset > 0 {
        node = PlanNode::Limit {
            input: Box::new(node),
            limit: spec.limit,
            offset: spec.offset,
            output: proj_output,
        };
    }

    Ok(node)
}

/// Build the [`PlanNode::Aggregate`] for a grouped/aggregated query: collect the
/// distinct aggregate calls appearing in projection/having/order, and produce a
/// synthetic output schema (`__g*` group cols, `__a*` aggregate cols).
fn plan_aggregate(input: PlanNode, spec: &QuerySpec) -> Result<PlanNode> {
    let mut aggs: Vec<AggCall> = Vec::new();
    for p in &spec.projection {
        if let Projection::Expr { expr, .. } = p {
            collect_aggs(expr, &mut aggs);
        }
    }
    if let Some(h) = &spec.having {
        collect_aggs(h, &mut aggs);
    }
    for k in &spec.order_by {
        collect_aggs(&k.expr, &mut aggs);
    }

    let mut output = Vec::new();
    for (i, _) in spec.group_by.iter().enumerate() {
        output.push(ColumnRef {
            qualifier: String::new(),
            name: format!("__g{i}"),
            ty: ColumnType::Text,
        });
    }
    for (i, _) in aggs.iter().enumerate() {
        output.push(ColumnRef {
            qualifier: String::new(),
            name: format!("__a{i}"),
            ty: ColumnType::Text,
        });
    }
    Ok(PlanNode::Aggregate {
        input: Box::new(input),
        group_exprs: spec.group_by.clone(),
        aggs,
        output,
    })
}

/// Collect distinct aggregate calls (by structural equality) appearing in
/// `expr`.
fn collect_aggs(expr: &QExpr, out: &mut Vec<AggCall>) {
    match expr {
        QExpr::Aggregate {
            func,
            arg,
            distinct,
        } => {
            let call = AggCall {
                func: *func,
                arg: arg.as_deref().cloned(),
                distinct: *distinct,
            };
            if !out.iter().any(|c| agg_call_eq(c, &call)) {
                out.push(call);
            }
        }
        QExpr::Column { .. } | QExpr::Literal(_) => {}
        QExpr::Compare { lhs, rhs, .. } | QExpr::And(lhs, rhs) | QExpr::Or(lhs, rhs) => {
            collect_aggs(lhs, out);
            collect_aggs(rhs, out);
        }
        QExpr::Not(e) | QExpr::IsNull { expr: e, .. } => collect_aggs(e, out),
        QExpr::InList { expr, list, .. } => {
            collect_aggs(expr, out);
            for e in list {
                collect_aggs(e, out);
            }
        }
        QExpr::InSubquery { expr, .. } => collect_aggs(expr, out),
        // Aggregates within a subquery belong to that subquery's own scope.
        QExpr::Exists { .. } | QExpr::ScalarSubquery(_) => {}
        QExpr::Like { expr, pattern, .. } => {
            collect_aggs(expr, out);
            collect_aggs(pattern, out);
        }
        QExpr::Match { column, query } => {
            collect_aggs(column, out);
            collect_aggs(query, out);
        }
        QExpr::Arith { lhs, rhs, .. } => {
            collect_aggs(lhs, out);
            collect_aggs(rhs, out);
        }
    }
}

fn agg_call_eq(a: &AggCall, b: &AggCall) -> bool {
    a.func == b.func && a.distinct == b.distinct && a.arg == b.arg
}

/// Rewrite an expression over the aggregate's synthetic output: a subexpression
/// equal to a group key becomes `__g{i}`; an aggregate call becomes `__a{j}`; a
/// bare column that is neither is an error (it must appear in GROUP BY or an
/// aggregate).
fn rewrite_over_agg(expr: &QExpr, group_exprs: &[QExpr], aggs: &[AggCall]) -> Result<QExpr> {
    // Group-key match first (so `GROUP BY a+b` style keys, once supported, win
    // over descending into children).
    if let Some(i) = group_exprs.iter().position(|g| g == expr) {
        return Ok(QExpr::Column {
            qualifier: None,
            name: format!("__g{i}"),
        });
    }
    match expr {
        QExpr::Aggregate {
            func,
            arg,
            distinct,
        } => {
            let call = AggCall {
                func: *func,
                arg: arg.as_deref().cloned(),
                distinct: *distinct,
            };
            let j = aggs
                .iter()
                .position(|c| agg_call_eq(c, &call))
                .expect("aggregate was collected in plan_aggregate");
            Ok(QExpr::Column {
                qualifier: None,
                name: format!("__a{j}"),
            })
        }
        QExpr::Column { name, .. } => Err(DbError::SqlPlan(format!(
            "column '{name}' must appear in GROUP BY or be used in an aggregate"
        ))),
        QExpr::Literal(l) => Ok(QExpr::Literal(l.clone())),
        QExpr::Compare { op, lhs, rhs } => Ok(QExpr::Compare {
            op: *op,
            lhs: Box::new(rewrite_over_agg(lhs, group_exprs, aggs)?),
            rhs: Box::new(rewrite_over_agg(rhs, group_exprs, aggs)?),
        }),
        QExpr::And(lhs, rhs) => Ok(QExpr::And(
            Box::new(rewrite_over_agg(lhs, group_exprs, aggs)?),
            Box::new(rewrite_over_agg(rhs, group_exprs, aggs)?),
        )),
        QExpr::Or(lhs, rhs) => Ok(QExpr::Or(
            Box::new(rewrite_over_agg(lhs, group_exprs, aggs)?),
            Box::new(rewrite_over_agg(rhs, group_exprs, aggs)?),
        )),
        QExpr::Not(e) => Ok(QExpr::Not(Box::new(rewrite_over_agg(
            e,
            group_exprs,
            aggs,
        )?))),
        QExpr::IsNull { expr, negated } => Ok(QExpr::IsNull {
            expr: Box::new(rewrite_over_agg(expr, group_exprs, aggs)?),
            negated: *negated,
        }),
        QExpr::InList {
            expr,
            list,
            negated,
        } => Ok(QExpr::InList {
            expr: Box::new(rewrite_over_agg(expr, group_exprs, aggs)?),
            list: list
                .iter()
                .map(|e| rewrite_over_agg(e, group_exprs, aggs))
                .collect::<Result<_>>()?,
            negated: *negated,
        }),
        // Subqueries carry their own scope; leave them intact. (A subquery
        // correlated on a group key is a documented v1 gap.)
        QExpr::Exists { .. } | QExpr::InSubquery { .. } | QExpr::ScalarSubquery(_) => {
            Ok(expr.clone())
        }
        QExpr::Like {
            expr,
            pattern,
            negated,
            case_insensitive,
        } => Ok(QExpr::Like {
            expr: Box::new(rewrite_over_agg(expr, group_exprs, aggs)?),
            pattern: Box::new(rewrite_over_agg(pattern, group_exprs, aggs)?),
            negated: *negated,
            case_insensitive: *case_insensitive,
        }),
        QExpr::Match { column, query } => Ok(QExpr::Match {
            column: Box::new(rewrite_over_agg(column, group_exprs, aggs)?),
            query: Box::new(rewrite_over_agg(query, group_exprs, aggs)?),
        }),
        QExpr::Arith { op, lhs, rhs } => Ok(QExpr::Arith {
            op: *op,
            lhs: Box::new(rewrite_over_agg(lhs, group_exprs, aggs)?),
            rhs: Box::new(rewrite_over_agg(rhs, group_exprs, aggs)?),
        }),
    }
}

/// Resolve the SELECT list of an aggregated query: each item is rewritten to
/// reference the aggregate's synthetic output columns.
fn resolve_projection_agg(
    projection: &[Projection],
    spec: &QuerySpec,
    agg_schema: &[ColumnRef],
) -> Result<Vec<ProjItem>> {
    // Re-derive the aggregate calls (same order as plan_aggregate) so rewriting
    // maps to the right `__a{j}`.
    let mut aggs: Vec<AggCall> = Vec::new();
    for p in projection {
        if let Projection::Expr { expr, .. } = p {
            collect_aggs(expr, &mut aggs);
        }
    }
    if let Some(h) = &spec.having {
        collect_aggs(h, &mut aggs);
    }
    for k in &spec.order_by {
        collect_aggs(&k.expr, &mut aggs);
    }

    let mut items = Vec::new();
    for p in projection {
        match p {
            Projection::Wildcard | Projection::QualifiedWildcard(_) => {
                return Err(DbError::SqlPlan(
                    "SELECT * cannot be combined with aggregation".into(),
                ))
            }
            Projection::Expr { expr, alias } => {
                let name = alias.clone().unwrap_or_else(|| default_expr_name(expr));
                let rewritten = rewrite_over_agg(expr, &spec.group_by, &aggs)?;
                // Validate it now resolves against the synthetic schema.
                validate_expr(&rewritten, agg_schema)?;
                items.push(ProjItem {
                    expr: rewritten,
                    name,
                });
            }
        }
    }
    Ok(items)
}

/// Resolve `ORDER BY` keys to output-column indices. v1 supports a bare output
/// column name/alias or a 1-based position (`ORDER BY 1`).
fn resolve_order_by(order_by: &[OrderKey], output: &[ColumnRef]) -> Result<Vec<SortKey>> {
    let mut keys = Vec::new();
    for k in order_by {
        let column = match &k.expr {
            QExpr::Literal(crate::sql::logical::Literal::Int(n)) => {
                let idx = (*n as usize).checked_sub(1).filter(|i| *i < output.len());
                idx.ok_or_else(|| {
                    DbError::SqlPlan(format!("ORDER BY position {n} is out of range"))
                })?
            }
            QExpr::Column { name, .. } => output
                .iter()
                .position(|c| c.name.eq_ignore_ascii_case(name))
                .ok_or_else(|| {
                    DbError::SqlPlan(format!(
                        "ORDER BY '{name}' must be an output column name or position in v1"
                    ))
                })?,
            _ => {
                return Err(DbError::SqlUnsupported(
                    "ORDER BY supports an output column name or 1-based position in v1".into(),
                ))
            }
        };
        keys.push(SortKey { column, asc: k.asc });
    }
    Ok(keys)
}

/// G4 (item 19): determine whether an ORDER BY expression requires pre-projection
/// sorting (i.e., the key is a column present in the input schema but absent from
/// the projected output). Returns `true` if the key is a column name that exists
/// in `pre_proj_schema` but NOT in `proj_output`.
fn order_key_needs_pre_proj_sort(
    expr: &QExpr,
    proj_output: &[ColumnRef],
    pre_proj_schema: &[ColumnRef],
) -> bool {
    match expr {
        QExpr::Column { name, .. } => {
            let in_proj = proj_output
                .iter()
                .any(|c| c.name.eq_ignore_ascii_case(name));
            if in_proj {
                return false;
            }
            // Present in pre-projection schema → needs pre-proj sort.
            pre_proj_schema
                .iter()
                .any(|c| c.name.eq_ignore_ascii_case(name))
        }
        // Positional (ORDER BY 1) and other expressions: handled post-projection.
        _ => false,
    }
}

/// G4 (item 19): resolve ORDER BY keys against the pre-projection schema when
/// any key refers to a column not in the projected output. Each key is resolved:
/// - Column name in the projected output → index in projected output (will sort
///   over pre_proj_schema at the matching pre-proj position).
/// - Column name only in pre_proj_schema → index in pre_proj_schema.
/// - Position literal → index into projected output (post-proj semantics).
fn resolve_order_by_pre_proj(
    order_by: &[OrderKey],
    pre_proj_schema: &[ColumnRef],
    proj_output: &[ColumnRef],
) -> Result<Vec<SortKey>> {
    let mut keys = Vec::new();
    for k in order_by {
        let column = match &k.expr {
            QExpr::Literal(crate::sql::logical::Literal::Int(n)) => {
                // Position is relative to projected output; find it in pre_proj_schema.
                let proj_idx = (*n as usize)
                    .checked_sub(1)
                    .filter(|i| *i < proj_output.len())
                    .ok_or_else(|| {
                        DbError::SqlPlan(format!("ORDER BY position {n} is out of range"))
                    })?;
                let proj_name = &proj_output[proj_idx].name;
                pre_proj_schema
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(proj_name))
                    .unwrap_or(proj_idx)
            }
            QExpr::Column { name, qualifier } => {
                // First look in pre-projection schema (wider set).
                if let Some(idx) = pre_proj_schema.iter().position(|c| {
                    c.name.eq_ignore_ascii_case(name)
                        && qualifier
                            .as_deref()
                            .is_none_or(|q| c.qualifier.eq_ignore_ascii_case(q))
                }) {
                    idx
                } else {
                    return Err(DbError::SqlPlan(format!(
                        "ORDER BY '{name}' is not a column in the FROM clause"
                    )));
                }
            }
            _ => {
                return Err(DbError::SqlUnsupported(
                    "ORDER BY supports a column name or 1-based position in v1".into(),
                ))
            }
        };
        keys.push(SortKey { column, asc: k.asc });
    }
    Ok(keys)
}

/// The output schema a FROM tree produces — used by subquery correlation
/// binding to tell which column references are inner vs. outer (P4.c).
pub fn plan_from_schema(
    from: &FromNode,
    catalog: &Catalog,
    ctes: &CteSchemas,
) -> Result<Vec<ColumnRef>> {
    Ok(plan_from(from, catalog, ctes)?.output().to_vec())
}

pub(crate) fn plan_from(node: &FromNode, catalog: &Catalog, ctes: &CteSchemas) -> Result<PlanNode> {
    match node {
        FromNode::Table(tref) => {
            let qualifier = tref.qualifier().to_string();
            // A FROM name matching a CTE resolves to the CTE, not a base table.
            if let Some(cte_cols) = ctes.get(&tref.table) {
                let output = cte_cols
                    .iter()
                    .map(|c| ColumnRef {
                        qualifier: qualifier.clone(),
                        name: c.name.clone(),
                        ty: c.ty,
                    })
                    .collect();
                return Ok(PlanNode::CteScan {
                    name: tref.table.clone(),
                    qualifier,
                    output,
                });
            }
            // Milestone 18, Epic C: an `information_schema.*` / `unidb_catalog.*`
            // reference resolves to a synthesized virtual relation, not a base
            // table. Its fixed schema comes from the introspection module; the
            // rows are materialized from the catalog at scan time
            // (`query_exec::Runner::scan`). The table name is preserved on the
            // `Scan` node so the runner can dispatch on it.
            if let Some(vcols) = crate::sql::information_schema::virtual_schema(&tref.table) {
                let output = vcols
                    .into_iter()
                    .map(|c| ColumnRef {
                        qualifier: qualifier.clone(),
                        name: c.name,
                        ty: c.ty,
                    })
                    .collect();
                return Ok(PlanNode::Scan {
                    table: tref.table.clone(),
                    qualifier,
                    output,
                });
            }
            let def = catalog.lookup(&tref.table)?;
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
            using,
        } => {
            let left = plan_from(left, catalog, ctes)?;
            let right = plan_from(right, catalog, ctes)?;
            if !using.is_empty() {
                return plan_using_join(left, right, *join_type, using, catalog);
            }
            plan_join(left, right, *join_type, on.clone(), catalog)
        }
        // G8 (item 19): SELECT without FROM — single empty row, no columns.
        FromNode::Dual => Ok(PlanNode::Dual),
    }
}

/// Plan a `JOIN … USING (c1, …)`. `USING` is desugared to the equi-`ON`
/// `left.ci = right.ci AND …` (resolving each shared column's qualifier on both
/// sides), then — per standard SQL — each shared column is **merged** so it
/// appears once in the output: a coalescing [`PlanNode::Projection`] drops the
/// duplicate copy. For `INNER`/`LEFT`/`CROSS` the preserved value is the left
/// side's; for `RIGHT` it is the right side's (the outer-preserved side), so the
/// merged column is non-NULL on the preserved rows without needing a `COALESCE`
/// expression. (`FULL OUTER`, which would need true `COALESCE`, is unsupported.)
fn plan_using_join(
    left: PlanNode,
    right: PlanNode,
    join_type: JoinType,
    using: &[String],
    catalog: &Catalog,
) -> Result<PlanNode> {
    let left_schema = left.output().to_vec();
    let right_schema = right.output().to_vec();

    // Synthesize the equi-`ON` from the shared columns' resolved qualifiers.
    let mut on: Option<QExpr> = None;
    for col in using {
        let li = resolve_column(&left_schema, None, col)?;
        let ri = resolve_column(&right_schema, None, col)?;
        let eq = QExpr::Compare {
            op: CmpOp::Eq,
            lhs: Box::new(QExpr::Column {
                qualifier: Some(left_schema[li].qualifier.clone()),
                name: col.clone(),
            }),
            rhs: Box::new(QExpr::Column {
                qualifier: Some(right_schema[ri].qualifier.clone()),
                name: col.clone(),
            }),
        };
        on = Some(match on {
            None => eq,
            Some(prev) => QExpr::And(Box::new(prev), Box::new(eq)),
        });
    }

    let join = plan_join(left, right, join_type, on, catalog)?;

    // Merge each shared column: keep it from the outer-preserved side (right for
    // RIGHT joins, else left) and drop the other side's copy. `drop_right` names
    // which side's copies of the `using` columns are dropped from the output.
    let drop_right = !matches!(join_type, JoinType::Right);
    let mut items = Vec::new();
    let mut output = Vec::new();
    for (i, col) in join.output().iter().enumerate() {
        let is_shared = using.iter().any(|u| u == &col.name);
        if is_shared {
            let from_left = i < left_schema.len();
            // Drop this copy if it is on the side we merge away.
            if (drop_right && !from_left) || (!drop_right && from_left) {
                continue;
            }
        }
        items.push(ProjItem {
            expr: QExpr::Column {
                qualifier: Some(col.qualifier.clone()),
                name: col.name.clone(),
            },
            name: col.name.clone(),
        });
        output.push(col.clone());
    }

    Ok(PlanNode::Projection {
        input: Box::new(join),
        items,
        output,
    })
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
        // Aggregates are rewritten to synthetic columns before validation; a
        // raw aggregate here means one was used outside an aggregated query.
        QExpr::Aggregate { .. } => Err(DbError::SqlPlan(
            "aggregate functions are only allowed in an aggregated query".into(),
        )),
        QExpr::InList { expr, list, .. } => {
            validate_expr(expr, schema)?;
            for e in list {
                validate_expr(e, schema)?;
            }
            Ok(())
        }
        // A subquery has its own scope; its correlated outer refs are checked
        // when it is planned at execution time. Only the outer `expr` of an
        // `IN (subquery)` resolves against this schema.
        QExpr::InSubquery { expr, .. } => validate_expr(expr, schema),
        QExpr::Exists { .. } | QExpr::ScalarSubquery(_) => Ok(()),
        QExpr::Like { expr, pattern, .. } => {
            validate_expr(expr, schema)?;
            validate_expr(pattern, schema)
        }
        QExpr::Match { column, query } => {
            validate_expr(column, schema)?;
            validate_expr(query, schema)
        }
        QExpr::Arith { lhs, rhs, .. } => {
            validate_expr(lhs, schema)?;
            validate_expr(rhs, schema)
        }
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
        // Aggregates are computed by the Aggregate operator and referenced via
        // synthetic columns; one should never reach per-row evaluation.
        QExpr::Aggregate { .. } => Err(DbError::SqlPlan(
            "internal: aggregate reached row-level evaluation".into(),
        )),
        // `IN (v1, v2, ...)` over a literal list needs no execution context.
        QExpr::InList {
            expr,
            list,
            negated,
        } => {
            let needle = eval_qexpr(expr, schema, row)?;
            if matches!(needle, Literal::Null) {
                return Ok(Literal::Null);
            }
            let mut any = false;
            for item in list {
                let v = eval_qexpr(item, schema, row)?;
                if executor::compare(crate::sql::logical::CmpOp::Eq, &needle, &v)? {
                    any = true;
                    break;
                }
            }
            Ok(Literal::Bool(any != *negated))
        }
        // Subqueries require storage access; the ctx-aware evaluator in
        // `query_exec` intercepts them before the pure evaluator is reached.
        QExpr::Exists { .. } | QExpr::InSubquery { .. } | QExpr::ScalarSubquery(_) => Err(
            DbError::SqlPlan("internal: subquery reached the pure evaluator".into()),
        ),
        // `QExpr::Like` — SQL pattern matching (G9, item 30).
        QExpr::Like {
            expr,
            pattern,
            negated,
            case_insensitive,
        } => {
            let val = eval_qexpr(expr, schema, row)?;
            let pat = eval_qexpr(pattern, schema, row)?;
            match (&val, &pat) {
                (Literal::Null, _) | (_, Literal::Null) => Ok(Literal::Null),
                (Literal::Text(t), Literal::Text(p)) => Ok(Literal::Bool(
                    executor::like_match(t, p, *case_insensitive) != *negated,
                )),
                _ => Err(DbError::SqlUnsupported(format!(
                    "LIKE requires TEXT operands, got {val:?} LIKE {pat:?}"
                ))),
            }
        }
        // `QExpr::Match` — inline text-contains-all-tokens evaluation (G11,
        // item 30). In the multi-relation query path there is no index routing
        // (unlike the single-table `exec_select_match`), so we evaluate MATCH
        // as an AND-all-tokens text-containment check using the same tokenizer
        // as the FULLTEXT index — semantically equivalent, not index-accelerated.
        QExpr::Match { column, query } => {
            let col_val = eval_qexpr(column, schema, row)?;
            let query_val = eval_qexpr(query, schema, row)?;
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
        // Arithmetic in the query path (G8): enables `SELECT 1+1`, etc.
        QExpr::Arith { op, lhs, rhs } => {
            let l = eval_qexpr(lhs, schema, row)?;
            let r = eval_qexpr(rhs, schema, row)?;
            executor::eval_arith(*op, l, r)
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
    let default = std::env::var("UNIDB_HASH_JOIN_MEM_ROWS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(1_000_000);
    crate::query_limits::work_mem_rows(default)
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
            unique_index_root: None,
            constraints: ColumnConstraints::default(),
            dropped: false,
        }
    }

    fn table(name: &str, columns: Vec<ColumnDef>) -> TableDef {
        TableDef {
            name: name.to_string(),
            columns,
            pages: vec![],
            fsm_meta: None,
            rls_policy: None,
            insert_policy: None,
            update_policy: None,
            delete_policy: None,
            policies: vec![],
            events_enabled: false,
            constraints: Default::default(),
            serial_next: Default::default(),
            generation: 0,
            row_count: 0,
        }
    }

    fn join_spec() -> QuerySpec {
        QuerySpec {
            with: vec![],
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
                using: vec![],
            },
            selection: None,
            projection: vec![Projection::Wildcard],
            group_by: vec![],
            having: None,
            distinct: false,
            order_by: vec![],
            limit: None,
            offset: 0,
        }
    }

    fn inner_join_node(catalog: &Catalog) -> PlanNode {
        match plan_query(&join_spec(), catalog, &CteSchemas::new()).unwrap() {
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

    #[test]
    fn planner_picks_hash_join_when_inner_has_only_unique_index() {
        // unique_index_root (PRIMARY KEY / UNIQUE via item 35) is an enforcement
        // index, not a join-optimization hint.  Without an explicit secondary
        // BTree (index_root), the planner uses HashJoin — which for large tables
        // has lower per-probe fetch_page cost than INLJ's random B-tree lookups.
        let mut catalog = Catalog::new();
        catalog.insert_for_test(table("customers", vec![col("id", None, None)]));
        let mut orders_customer_id = col("customer_id", None, None);
        orders_customer_id.unique_index_root = Some(42);
        catalog.insert_for_test(table("orders", vec![orders_customer_id]));
        assert!(matches!(
            inner_join_node(&catalog),
            PlanNode::HashJoin { .. }
        ));
    }
}
