//! P4.d cost-based optimizer. Builds the base-access + join + WHERE subtree of
//! a query, using `ANALYZE` statistics ([`crate::sql::statistics`]) to make two
//! decisions the rule-based planner can't:
//!
//! 1. **Index vs. scan** for base access — an equality/range predicate on a
//!    B-Tree-indexed column becomes an [`PlanNode::IndexScan`] when estimated
//!    selective, otherwise a full [`PlanNode::Scan`].
//! 2. **Join order** — a Selinger-style left-deep dynamic program (≤ 10
//!    relations, greedy beyond) minimizing the summed intermediate cardinality,
//!    with equi-join edges realized as hash joins.
//!
//! The optimizer only engages when every base relation involved is a plain
//! table that has been `ANALYZE`d and the join tree is inner/cross-only.
//! Otherwise it falls back to the rule-based [`crate::sql::plan::plan_from`] +
//! `Filter`, which still yields correct plans (and the index-nested-loop join
//! from P4.a). This keeps every pre-P4.d plan and test unchanged.

use crate::catalog::{Catalog, IndexKind};
use crate::error::Result;
use crate::sql::logical::{CmpOp, Literal};
use crate::sql::plan::{plan_from, ColumnRef, CteSchemas, PlanNode};
use crate::sql::query::{FromNode, JoinType, QExpr, TableRef};

/// Use an index scan when the predicate is estimated to select at most this
/// fraction of the table; otherwise a full scan is cheaper (avoids random I/O
/// per matching row). Deliberately conservative.
const INDEX_SELECTIVITY_THRESHOLD: f64 = 0.1;
/// Default selectivity for a predicate the estimator can't reason about.
const DEFAULT_SELECTIVITY: f64 = 0.33;
/// Cardinality assumed for a base relation missing stats (never reached once
/// eligibility requires stats, but keeps the estimator total).
const DEFAULT_CARD: f64 = 1000.0;
/// Beyond this many relations, fall back to greedy join ordering.
const DP_MAX_RELATIONS: usize = 10;

/// Build the access + join + WHERE subtree for a query.
pub fn plan_access(
    from: &FromNode,
    selection: &Option<QExpr>,
    catalog: &Catalog,
    ctes: &CteSchemas,
) -> Result<PlanNode> {
    if let Some(node) = try_cost_based(from, selection, catalog, ctes)? {
        return Ok(node);
    }
    // Fallback: rule-based join tree + a residual WHERE filter.
    let mut node = plan_from(from, catalog, ctes)?;
    if let Some(sel) = selection {
        let output = node.output().to_vec();
        node = PlanNode::Filter {
            input: Box::new(node),
            predicate: sel.clone(),
            output,
        };
    }
    Ok(node)
}

/// A base relation participating in the cost-based join.
struct Rel {
    output: Vec<ColumnRef>,
    /// Fully-materialized access (Scan or IndexScan, wrapped in a Filter for any
    /// residual single-relation predicates).
    access: PlanNode,
    /// Estimated output cardinality after its single-relation predicates.
    card: f64,
}

/// An equi-join predicate `left_col = right_col` between two relations.
struct Edge {
    a: usize,
    b: usize,
    a_col: QExpr,
    b_col: QExpr,
    selectivity: f64,
}

fn try_cost_based(
    from: &FromNode,
    selection: &Option<QExpr>,
    catalog: &Catalog,
    ctes: &CteSchemas,
) -> Result<Option<PlanNode>> {
    // Flatten inner/cross joins into base tables + ON conjuncts; bail on outer.
    let mut tables = Vec::new();
    let mut on_conjuncts = Vec::new();
    if !flatten_inner(from, &mut tables, &mut on_conjuncts) {
        return Ok(None);
    }
    // Every relation must be a plain, ANALYZEd base table (not a CTE).
    for tref in &tables {
        if ctes.contains_key(&tref.table) || catalog.table_stats(&tref.table).is_none() {
            return Ok(None);
        }
    }

    // All conjuncts: join ON + WHERE.
    let mut conjuncts: Vec<QExpr> = on_conjuncts;
    if let Some(sel) = selection {
        conjuncts.extend(sel.conjuncts().into_iter().cloned());
    }

    // Scans + per-relation outputs (used for classifying conjuncts by relation).
    let mut rels: Vec<Rel> = Vec::with_capacity(tables.len());
    let mut outputs: Vec<Vec<ColumnRef>> = Vec::with_capacity(tables.len());
    for tref in &tables {
        let scan = plan_from(&FromNode::Table(tref.clone()), catalog, ctes)?;
        outputs.push(scan.output().to_vec());
        rels.push(Rel {
            output: scan.output().to_vec(),
            access: scan, // replaced below
            card: DEFAULT_CARD,
        });
    }

    // Classify each conjunct: single-relation predicate, equi-join edge, or
    // residual (multi-relation non-equi / unresolved).
    let mut single: Vec<Vec<QExpr>> = vec![Vec::new(); rels.len()];
    let mut edges: Vec<Edge> = Vec::new();
    let mut residual: Vec<QExpr> = Vec::new();
    for c in conjuncts {
        let rel_set = relations_of(&c, &outputs);
        if rel_set.len() == 1 {
            single[rel_set[0]].push(c);
        } else if let Some(edge) = as_edge(&c, &outputs) {
            edges.push(edge);
        } else {
            residual.push(c);
        }
    }

    // Build each relation's access (index-vs-scan) + cardinality estimate.
    for (i, tref) in tables.iter().enumerate() {
        let (access, card) = build_access(tref, &rels[i].output, &single[i], catalog)?;
        rels[i].access = access;
        rels[i].card = card;
    }

    // Choose the join order, then build the left-deep tree.
    let order = join_order(&rels, &edges);
    let mut node = build_join_tree(&order, rels, &edges);

    // Apply any residual (cross-relation non-equi) conjuncts as a final filter.
    for r in residual {
        let output = node.output().to_vec();
        node = PlanNode::Filter {
            input: Box::new(node),
            predicate: r,
            output,
        };
    }
    Ok(Some(node))
}

/// Flatten an inner/cross join tree into base tables + ON conjuncts. Returns
/// `false` (bail to fallback) on any outer join.
fn flatten_inner(node: &FromNode, tables: &mut Vec<TableRef>, on: &mut Vec<QExpr>) -> bool {
    match node {
        FromNode::Table(tref) => {
            tables.push(tref.clone());
            true
        }
        FromNode::Join {
            left,
            right,
            join_type,
            on: cond,
        } => {
            if !matches!(join_type, JoinType::Inner | JoinType::Cross) {
                return false;
            }
            if let Some(c) = cond {
                on.extend(c.conjuncts().into_iter().cloned());
            }
            flatten_inner(left, tables, on) && flatten_inner(right, tables, on)
        }
    }
}

/// Choose base access for one relation: an [`PlanNode::IndexScan`] on the most
/// selective indexable predicate when it clears the threshold, else a full
/// [`PlanNode::Scan`]; remaining single-relation predicates become a Filter.
fn build_access(
    tref: &TableRef,
    output: &[ColumnRef],
    preds: &[QExpr],
    catalog: &Catalog,
) -> Result<(PlanNode, f64)> {
    let def = catalog.lookup(&tref.table)?;
    let stats = catalog.table_stats(&tref.table);
    let base_card = stats.map(|s| s.row_count as f64).unwrap_or(DEFAULT_CARD);

    // Overall cardinality estimate = base * product(pred selectivities).
    let mut card = base_card;
    for p in preds {
        card *= predicate_selectivity(p, &tref.table, catalog);
    }

    // Find the most selective indexable predicate.
    let mut best: Option<(usize, CmpOp, Literal, f64)> = None;
    for (idx, p) in preds.iter().enumerate() {
        if let Some((col, op, lit)) = simple_predicate(p) {
            let indexed = def.columns.iter().any(|c| {
                c.name == col
                    && !c.dropped
                    && matches!(c.index, Some(IndexKind::BTree))
                    && c.index_root.is_some()
            });
            if indexed && op != CmpOp::Ne {
                let sel = column_selectivity(&tref.table, &col, op, &lit, catalog);
                if best.as_ref().is_none_or(|(_, _, _, bs)| sel < *bs) {
                    best = Some((idx, op, lit.clone(), sel));
                }
            }
        }
    }

    let qualifier = tref.qualifier().to_string();
    let (access, remaining): (PlanNode, Vec<QExpr>) = match best {
        Some((idx, op, value, sel)) if sel <= INDEX_SELECTIVITY_THRESHOLD => {
            let column = simple_predicate(&preds[idx]).map(|(c, _, _)| c).unwrap();
            let scan = PlanNode::IndexScan {
                table: tref.table.clone(),
                qualifier,
                column,
                op,
                value,
                output: output.to_vec(),
            };
            let remaining = preds
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != idx)
                .map(|(_, p)| p.clone())
                .collect();
            (scan, remaining)
        }
        _ => {
            let scan = PlanNode::Scan {
                table: tref.table.clone(),
                qualifier,
                output: output.to_vec(),
            };
            (scan, preds.to_vec())
        }
    };

    let access = wrap_filter(access, remaining);
    Ok((access, card))
}

fn wrap_filter(node: PlanNode, preds: Vec<QExpr>) -> PlanNode {
    let Some(pred) = and_all(preds) else {
        return node;
    };
    let output = node.output().to_vec();
    PlanNode::Filter {
        input: Box::new(node),
        predicate: pred,
        output,
    }
}

fn and_all(mut preds: Vec<QExpr>) -> Option<QExpr> {
    let mut acc = preds.pop()?;
    while let Some(p) = preds.pop() {
        acc = QExpr::And(Box::new(p), Box::new(acc));
    }
    Some(acc)
}

/// A `Column <op> Literal` (or the flipped `Literal <op> Column`) predicate.
fn simple_predicate(expr: &QExpr) -> Option<(String, CmpOp, Literal)> {
    if let QExpr::Compare { op, lhs, rhs } = expr {
        match (lhs.as_ref(), rhs.as_ref()) {
            (QExpr::Column { name, .. }, QExpr::Literal(l)) => Some((name.clone(), *op, l.clone())),
            (QExpr::Literal(l), QExpr::Column { name, .. }) => {
                Some((name.clone(), flip_op(*op), l.clone()))
            }
            _ => None,
        }
    } else {
        None
    }
}

fn flip_op(op: CmpOp) -> CmpOp {
    match op {
        CmpOp::Lt => CmpOp::Gt,
        CmpOp::Le => CmpOp::Ge,
        CmpOp::Gt => CmpOp::Lt,
        CmpOp::Ge => CmpOp::Le,
        CmpOp::Eq => CmpOp::Eq,
        CmpOp::Ne => CmpOp::Ne,
    }
}

/// Estimated selectivity of a single-relation predicate (falls back to a
/// default for shapes the estimator doesn't model).
fn predicate_selectivity(pred: &QExpr, table: &str, catalog: &Catalog) -> f64 {
    match simple_predicate(pred) {
        Some((col, op, lit)) => column_selectivity(table, &col, op, &lit, catalog),
        None => DEFAULT_SELECTIVITY,
    }
}

fn column_selectivity(table: &str, col: &str, op: CmpOp, lit: &Literal, catalog: &Catalog) -> f64 {
    let Some(stats) = catalog.table_stats(table) else {
        return DEFAULT_SELECTIVITY;
    };
    let Some(cs) = stats.columns.get(col) else {
        return DEFAULT_SELECTIVITY;
    };
    cs.selectivity(op, lit, stats.row_count)
        .unwrap_or(DEFAULT_SELECTIVITY)
}

/// Which relations a conjunct references, by resolving its columns against each
/// relation's output.
fn relations_of(expr: &QExpr, outputs: &[Vec<ColumnRef>]) -> Vec<usize> {
    let mut cols = Vec::new();
    collect_columns(expr, &mut cols);
    let mut rels = Vec::new();
    for (q, name) in &cols {
        for (i, out) in outputs.iter().enumerate() {
            if crate::sql::plan::resolve_column(out, q.as_deref(), name).is_ok()
                && !rels.contains(&i)
            {
                rels.push(i);
            }
        }
    }
    rels
}

fn collect_columns(expr: &QExpr, out: &mut Vec<(Option<String>, String)>) {
    match expr {
        QExpr::Column { qualifier, name } => out.push((qualifier.clone(), name.clone())),
        QExpr::Literal(_) => {}
        QExpr::Compare { lhs, rhs, .. } | QExpr::And(lhs, rhs) | QExpr::Or(lhs, rhs) => {
            collect_columns(lhs, out);
            collect_columns(rhs, out);
        }
        QExpr::Not(e) | QExpr::IsNull { expr: e, .. } => collect_columns(e, out),
        QExpr::InList { expr, list, .. } => {
            collect_columns(expr, out);
            for e in list {
                collect_columns(e, out);
            }
        }
        QExpr::Aggregate { arg, .. } => {
            if let Some(a) = arg {
                collect_columns(a, out);
            }
        }
        // Subqueries carry their own scope; correlated refs are handled at run
        // time, not here (a subquery-bearing conjunct becomes residual).
        QExpr::InSubquery { .. } | QExpr::Exists { .. } | QExpr::ScalarSubquery(_) => {}
    }
}

/// If `expr` is `Column = Column` across two relations, build the join edge.
fn as_edge(expr: &QExpr, outputs: &[Vec<ColumnRef>]) -> Option<Edge> {
    let (l, r) = expr.as_equi()?;
    let (lq, ln) = column_of(l)?;
    let (rq, rn) = column_of(r)?;
    let a = rel_of(&lq, &ln, outputs)?;
    let b = rel_of(&rq, &rn, outputs)?;
    if a == b {
        return None;
    }
    Some(Edge {
        a,
        b,
        a_col: l.clone(),
        b_col: r.clone(),
        selectivity: DEFAULT_SELECTIVITY.min(0.1),
    })
}

fn column_of(e: &QExpr) -> Option<(Option<String>, String)> {
    if let QExpr::Column { qualifier, name } = e {
        Some((qualifier.clone(), name.clone()))
    } else {
        None
    }
}

fn rel_of(q: &Option<String>, name: &str, outputs: &[Vec<ColumnRef>]) -> Option<usize> {
    outputs
        .iter()
        .position(|out| crate::sql::plan::resolve_column(out, q.as_deref(), name).is_ok())
}

/// Estimated result cardinality of joining the relations in `mask`.
fn card_of(mask: u32, rels: &[Rel], edges: &[Edge]) -> f64 {
    let mut c = 1.0;
    for (i, r) in rels.iter().enumerate() {
        if mask & (1 << i) != 0 {
            c *= r.card;
        }
    }
    for e in edges {
        if mask & (1 << e.a) != 0 && mask & (1 << e.b) != 0 {
            c *= e.selectivity;
        }
    }
    c.max(1.0)
}

fn connected(r: usize, mask: u32, edges: &[Edge]) -> bool {
    edges
        .iter()
        .any(|e| (e.a == r && mask & (1 << e.b) != 0) || (e.b == r && mask & (1 << e.a) != 0))
}

/// Left-deep join order: Selinger DP minimizing summed intermediate
/// cardinality for ≤ [`DP_MAX_RELATIONS`], greedy beyond.
fn join_order(rels: &[Rel], edges: &[Edge]) -> Vec<usize> {
    let n = rels.len();
    if n <= 1 {
        return (0..n).collect();
    }
    if n > DP_MAX_RELATIONS {
        return greedy_order(rels, edges);
    }

    let full = (1usize << n) - 1;
    let inf = f64::INFINITY;
    let mut best = vec![inf; 1 << n];
    let mut choice = vec![(0u32, 0usize); 1 << n];
    for r in 0..n {
        best[1 << r] = rels[r].card;
        // Record the singleton's own relation so order reconstruction emits it
        // (not the default 0) when it bottoms out at a single-relation mask.
        choice[1 << r] = (0, r);
    }
    for mask in 1u32..=(full as u32) {
        if (mask.count_ones() as usize) < 2 {
            continue;
        }
        for r in 0..n {
            if mask & (1 << r) == 0 {
                continue;
            }
            let sub = mask ^ (1 << r);
            if best[sub as usize].is_infinite() {
                continue;
            }
            // Penalize a Cartesian step (r not connected to the sub-plan) so
            // connected orders win.
            let cross_penalty = if connected(r, sub, edges) || sub == 0 {
                0.0
            } else {
                1e12
            };
            let cost = best[sub as usize] + card_of(mask, rels, edges) + cross_penalty;
            if cost < best[mask as usize] {
                best[mask as usize] = cost;
                choice[mask as usize] = (sub, r);
            }
        }
    }

    // Reconstruct the added-relation sequence, then reverse to build order.
    let mut order = Vec::with_capacity(n);
    let mut mask = full as u32;
    while mask != 0 {
        let (sub, r) = choice[mask as usize];
        order.push(r);
        if mask.count_ones() == 1 {
            break;
        }
        mask = sub;
    }
    order.reverse();
    order
}

/// Greedy fallback for many relations: start from the smallest, then repeatedly
/// add the connected relation that yields the smallest running result.
fn greedy_order(rels: &[Rel], edges: &[Edge]) -> Vec<usize> {
    let n = rels.len();
    let mut remaining: Vec<usize> = (0..n).collect();
    remaining.sort_by(|&a, &b| rels[a].card.total_cmp(&rels[b].card));
    let first = remaining.remove(0);
    let mut order = vec![first];
    let mut mask = 1u32 << first;
    while !remaining.is_empty() {
        // Prefer a connected relation; break ties by resulting cardinality.
        let pick = remaining
            .iter()
            .enumerate()
            .min_by(|&(_, &a), &(_, &b)| {
                let ca = (
                    !connected(a, mask, edges),
                    card_of(mask | (1 << a), rels, edges),
                );
                let cb = (
                    !connected(b, mask, edges),
                    card_of(mask | (1 << b), rels, edges),
                );
                ca.0.cmp(&cb.0).then(ca.1.total_cmp(&cb.1))
            })
            .map(|(i, _)| i)
            .unwrap();
        let r = remaining.remove(pick);
        order.push(r);
        mask |= 1 << r;
    }
    order
}

/// Build the left-deep join tree for `order`, realizing equi-join edges as hash
/// joins. Consumes `rels` (each relation's access is moved into the tree once).
fn build_join_tree(order: &[usize], mut rels: Vec<Rel>, edges: &[Edge]) -> PlanNode {
    let mut built_mask = 0u32;
    let first = order[0];
    built_mask |= 1 << first;
    let mut plan = std::mem::replace(
        &mut rels[first].access,
        PlanNode::Scan {
            table: String::new(),
            qualifier: String::new(),
            output: vec![],
        },
    );

    for &r in &order[1..] {
        let right = std::mem::replace(
            &mut rels[r].access,
            PlanNode::Scan {
                table: String::new(),
                qualifier: String::new(),
                output: vec![],
            },
        );
        // Equi keys connecting r to the built set.
        let mut left_keys = Vec::new();
        let mut right_keys = Vec::new();
        for e in edges {
            if e.a == r && built_mask & (1 << e.b) != 0 {
                left_keys.push(e.b_col.clone());
                right_keys.push(e.a_col.clone());
            } else if e.b == r && built_mask & (1 << e.a) != 0 {
                left_keys.push(e.a_col.clone());
                right_keys.push(e.b_col.clone());
            }
        }
        let mut output = plan.output().to_vec();
        output.extend(rels[r].output.clone());
        plan = if left_keys.is_empty() {
            PlanNode::NestedLoopJoin {
                left: Box::new(plan),
                right: Box::new(right),
                join_type: JoinType::Inner,
                on: None,
                output,
            }
        } else {
            PlanNode::HashJoin {
                left: Box::new(plan),
                right: Box::new(right),
                join_type: JoinType::Inner,
                left_keys,
                right_keys,
                residual: None,
                output,
            }
        };
        built_mask |= 1 << r;
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{ColumnConstraints, ColumnDef, ColumnType, TableDef};
    use crate::sql::statistics::{ColumnStats, TableStats};
    use std::collections::HashMap;

    fn col(name: &str, indexed: bool) -> ColumnDef {
        ColumnDef {
            name: name.to_string(),
            ty: ColumnType::Int64,
            index: if indexed {
                Some(IndexKind::BTree)
            } else {
                None
            },
            index_root: if indexed { Some(1) } else { None },
            constraints: ColumnConstraints::default(),
            dropped: false,
        }
    }

    fn table(catalog: &mut Catalog, name: &str, cols: Vec<ColumnDef>) {
        catalog.insert_for_test(TableDef {
            name: name.to_string(),
            columns: cols,
            pages: vec![],
            rls_policy: None,
            events_enabled: false,
            constraints: Default::default(),
            serial_next: Default::default(),
        });
    }

    fn colstats(distinct: u64, min: i64, max: i64) -> ColumnStats {
        // Coarse equi-depth bounds spanning [min, max].
        let mut bounds = Vec::new();
        let n = 16i64;
        for b in 1..=n {
            bounds.push(Literal::Int(min + (max - min) * b / n));
        }
        ColumnStats {
            distinct,
            null_count: 0,
            min: Some(Literal::Int(min)),
            max: Some(Literal::Int(max)),
            bounds,
        }
    }

    fn eq(col: &str, v: i64) -> QExpr {
        QExpr::Compare {
            op: CmpOp::Eq,
            lhs: Box::new(QExpr::Column {
                qualifier: None,
                name: col.to_string(),
            }),
            rhs: Box::new(QExpr::Literal(Literal::Int(v))),
        }
    }

    fn lt(col: &str, v: i64) -> QExpr {
        QExpr::Compare {
            op: CmpOp::Lt,
            lhs: Box::new(QExpr::Column {
                qualifier: None,
                name: col.to_string(),
            }),
            rhs: Box::new(QExpr::Literal(Literal::Int(v))),
        }
    }

    fn analyzed_catalog() -> Catalog {
        let mut catalog = Catalog::new();
        table(&mut catalog, "t", vec![col("id", true), col("kind", true)]);
        let mut columns = HashMap::new();
        // id: unique over 1000 rows (equality selectivity 1/1000 -> index).
        columns.insert("id".to_string(), colstats(1000, 0, 999));
        // kind: only 2 distinct values (equality selectivity 0.5 -> scan).
        columns.insert("kind".to_string(), colstats(2, 0, 1));
        catalog.insert_stats_for_test(
            "t",
            TableStats {
                row_count: 1000,
                columns,
            },
        );
        catalog
    }

    fn from_t() -> FromNode {
        FromNode::Table(TableRef {
            table: "t".to_string(),
            alias: None,
        })
    }

    #[test]
    fn picks_index_scan_for_selective_equality() {
        let catalog = analyzed_catalog();
        let node =
            plan_access(&from_t(), &Some(eq("id", 42)), &catalog, &CteSchemas::new()).unwrap();
        assert!(
            matches!(node, PlanNode::IndexScan { .. }),
            "selective equality on a unique indexed column should use IndexScan, got {node:?}"
        );
    }

    #[test]
    fn picks_full_scan_for_unselective_equality() {
        let catalog = analyzed_catalog();
        // `kind` has 2 distinct values -> 50% selectivity -> a full scan wins.
        let node = plan_access(
            &from_t(),
            &Some(eq("kind", 1)),
            &catalog,
            &CteSchemas::new(),
        )
        .unwrap();
        // The chosen access is a Scan (optionally under a Filter for the pred).
        let has_index_scan = plan_contains_index_scan(&node);
        assert!(
            !has_index_scan,
            "unselective equality should NOT use an index scan, got {node:?}"
        );
    }

    #[test]
    fn range_crossover_is_value_dependent() {
        let catalog = analyzed_catalog();
        // id < 20  -> ~2% of rows -> index scan.
        let selective =
            plan_access(&from_t(), &Some(lt("id", 20)), &catalog, &CteSchemas::new()).unwrap();
        assert!(plan_contains_index_scan(&selective), "got {selective:?}");
        // id < 990 -> ~99% of rows -> full scan.
        let broad = plan_access(
            &from_t(),
            &Some(lt("id", 990)),
            &catalog,
            &CteSchemas::new(),
        )
        .unwrap();
        assert!(!plan_contains_index_scan(&broad), "got {broad:?}");
    }

    fn plan_contains_index_scan(node: &PlanNode) -> bool {
        match node {
            PlanNode::IndexScan { .. } => true,
            PlanNode::Filter { input, .. } => plan_contains_index_scan(input),
            _ => false,
        }
    }
}
