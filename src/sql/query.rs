//! Multi-relation query representation (Phase 4).
//!
//! The pre-Phase-4 `LogicalPlan::Select` is deliberately kept for the trivial
//! single-table filter/project case (it feeds the concurrent-read fast path in
//! [`crate::sql::executor::plan_is_concurrent_read`] and every pre-P4 test).
//! Anything that needs a join, aggregate, grouping, sort, subquery, or CTE is
//! routed by the parser into [`LogicalPlan::Query`](crate::sql::logical::LogicalPlan)
//! carrying a [`QuerySpec`] — a richer tree the Phase-4 planner
//! ([`crate::sql::plan`]) turns into a physical operator tree and the executor
//! runs.
//!
//! Why a separate expression type ([`QExpr`]) rather than extending the flat
//! [`Expr`](crate::sql::logical::Expr): the flat `Expr` is battle-tested across
//! the single-table executor, RLS, CHECK constraints, and the wire DTOs, and its
//! `Expr::Column` is unqualified. Multi-relation queries need *qualified* columns
//! (`t.c`), `OR`, and (later checkpoints) aggregates and subqueries. Keeping the
//! two expression worlds separate means the Phase-4 work adds arms to its own
//! matches only — the single-table path is untouched, so the 258 pre-P4 tests
//! and the merge boundary stay clean.

use serde::{Deserialize, Serialize};

use crate::sql::logical::{CmpOp, Expr, Literal};

/// A parsed multi-relation query. Fields are added per checkpoint as the
/// planner learns to read them (P4.a: `from` / `selection` / `projection`;
/// P4.b adds grouping/sort/limit; P4.c adds `with`/subqueries) so no field is
/// ever written-but-unread (which would trip `clippy -D warnings`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuerySpec {
    /// FROM clause: a left-deep tree of base tables and joins.
    pub from: FromNode,
    /// WHERE predicate applied after the joins (residual). Base-table-only
    /// conjuncts are pushed down to their [`FromNode::Table`] during planning.
    pub selection: Option<QExpr>,
    /// SELECT list.
    pub projection: Vec<Projection>,
}

/// One SELECT-list item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Projection {
    /// `*` — every column of every relation, in FROM order.
    Wildcard,
    /// `t.*` — every column of relation `t`.
    QualifiedWildcard(String),
    /// A scalar expression, with an optional `AS alias`.
    Expr { expr: QExpr, alias: Option<String> },
}

/// FROM clause as a left-deep join tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FromNode {
    Table(TableRef),
    Join {
        left: Box<FromNode>,
        right: Box<FromNode>,
        join_type: JoinType,
        /// `None` for a `CROSS JOIN` / comma join.
        on: Option<QExpr>,
    },
}

/// A base-table reference with an optional alias. `alias` (or the table name
/// when absent) is the qualifier used to disambiguate columns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableRef {
    pub table: String,
    pub alias: Option<String>,
}

impl TableRef {
    /// The name by which columns of this relation are qualified.
    pub fn qualifier(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.table)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum JoinType {
    Inner,
    Left,
    Right,
    /// `CROSS JOIN` / comma join — Cartesian product, never carries an `on`.
    Cross,
}

/// Phase-4 scalar expression (see the module doc for why this is distinct from
/// [`Expr`]). Variants are added per checkpoint; P4.a covers the scalar set a
/// join `WHERE`/`ON` needs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum QExpr {
    /// A column reference, optionally qualified by a relation name/alias.
    Column {
        qualifier: Option<String>,
        name: String,
    },
    Literal(Literal),
    Compare {
        op: CmpOp,
        lhs: Box<QExpr>,
        rhs: Box<QExpr>,
    },
    And(Box<QExpr>, Box<QExpr>),
    Or(Box<QExpr>, Box<QExpr>),
    Not(Box<QExpr>),
    IsNull {
        expr: Box<QExpr>,
        negated: bool,
    },
}

impl QExpr {
    /// If this expression is exactly `Column(q) = Column(q')` (an equi-join
    /// key), return the two column references. Used by the planner to detect
    /// hash/index-nested-loop-joinable conditions.
    pub fn as_equi(&self) -> Option<(&QExpr, &QExpr)> {
        match self {
            QExpr::Compare {
                op: CmpOp::Eq,
                lhs,
                rhs,
            } if matches!(**lhs, QExpr::Column { .. }) && matches!(**rhs, QExpr::Column { .. }) => {
                Some((lhs, rhs))
            }
            _ => None,
        }
    }

    /// Split a conjunction into its top-level AND'd conjuncts.
    pub fn conjuncts(&self) -> Vec<&QExpr> {
        let mut out = Vec::new();
        self.collect_conjuncts(&mut out);
        out
    }

    fn collect_conjuncts<'a>(&'a self, out: &mut Vec<&'a QExpr>) {
        match self {
            QExpr::And(l, r) => {
                l.collect_conjuncts(out);
                r.collect_conjuncts(out);
            }
            other => out.push(other),
        }
    }

    /// Substitute `$n` bind parameters in place (P2.e parity for the Query
    /// path). Mirrors [`crate::sql::logical::bind_params`] for the flat plan.
    pub fn bind_params(&mut self, params: &[Literal]) -> crate::error::Result<()> {
        match self {
            QExpr::Literal(lit) => {
                if let Literal::Param(n) = *lit {
                    *lit = params.get(n - 1).cloned().ok_or_else(|| {
                        crate::error::DbError::SqlPlan(format!(
                            "bind parameter ${n} has no value ({} supplied)",
                            params.len()
                        ))
                    })?;
                }
                Ok(())
            }
            QExpr::Column { .. } => Ok(()),
            QExpr::Compare { lhs, rhs, .. } | QExpr::And(lhs, rhs) | QExpr::Or(lhs, rhs) => {
                lhs.bind_params(params)?;
                rhs.bind_params(params)
            }
            QExpr::Not(e) | QExpr::IsNull { expr: e, .. } => e.bind_params(params),
        }
    }
}

impl QuerySpec {
    /// Rewrite: AND `policy` (an RLS predicate, in the flat [`Expr`] form the
    /// catalog stores it) into the scan of every base relation named `table`.
    /// This keeps RLS a pure planner rewrite for joins too — the executor never
    /// learns RLS exists, exactly as in [`crate::sql::logical::apply_rls`].
    ///
    /// The policy is translated from `Expr` to [`QExpr`] and qualified to the
    /// relation's alias so it composes with the join's combined schema.
    pub fn apply_rls_from(&mut self, policy_for: &dyn Fn(&str) -> Option<Expr>) {
        // Collect (qualifier, policy) for each base relation, then AND each
        // policy into the query's residual selection qualified to that alias.
        let mut policies = Vec::new();
        collect_table_policies(&self.from, policy_for, &mut policies);
        for pol in policies {
            self.selection = Some(match self.selection.take() {
                Some(existing) => QExpr::And(Box::new(existing), Box::new(pol)),
                None => pol,
            });
        }
    }

    /// Bind `$n` parameters across the whole spec.
    pub fn bind_params(&mut self, params: &[Literal]) -> crate::error::Result<()> {
        bind_from(&mut self.from, params)?;
        if let Some(sel) = &mut self.selection {
            sel.bind_params(params)?;
        }
        for proj in &mut self.projection {
            if let Projection::Expr { expr, .. } = proj {
                expr.bind_params(params)?;
            }
        }
        Ok(())
    }
}

fn bind_from(node: &mut FromNode, params: &[Literal]) -> crate::error::Result<()> {
    match node {
        FromNode::Table(_) => Ok(()),
        FromNode::Join {
            left, right, on, ..
        } => {
            bind_from(left, params)?;
            bind_from(right, params)?;
            if let Some(on) = on {
                on.bind_params(params)?;
            }
            Ok(())
        }
    }
}

fn collect_table_policies(
    node: &FromNode,
    policy_for: &dyn Fn(&str) -> Option<Expr>,
    out: &mut Vec<QExpr>,
) {
    match node {
        FromNode::Table(tref) => {
            if let Some(pol) = policy_for(&tref.table) {
                out.push(qualify_policy(pol, tref.qualifier()));
            }
        }
        FromNode::Join { left, right, .. } => {
            collect_table_policies(left, policy_for, out);
            collect_table_policies(right, policy_for, out);
        }
    }
}

/// Translate a flat [`Expr`] RLS policy into a [`QExpr`], qualifying every
/// column with `qualifier` so it resolves against the correct relation in a
/// join. RLS policies are simple predicates (comparisons AND'd together), which
/// is exactly the subset [`QExpr`] covers; anything outside it (e.g. `NEAR`,
/// which is never a policy) degrades to a literal `true` and is a no-op filter.
fn qualify_policy(policy: Expr, qualifier: &str) -> QExpr {
    match policy {
        Expr::Column(name) => QExpr::Column {
            qualifier: Some(qualifier.to_string()),
            name,
        },
        Expr::Literal(l) => QExpr::Literal(l),
        Expr::BinOp { op, lhs, rhs } => QExpr::Compare {
            op,
            lhs: Box::new(qualify_policy(*lhs, qualifier)),
            rhs: Box::new(qualify_policy(*rhs, qualifier)),
        },
        Expr::And(lhs, rhs) => QExpr::And(
            Box::new(qualify_policy(*lhs, qualifier)),
            Box::new(qualify_policy(*rhs, qualifier)),
        ),
        // JSON extraction and NEAR are not valid RLS policy shapes; treat as a
        // permissive no-op rather than inventing semantics for them here.
        Expr::JsonExtract { .. } | Expr::JsonExtractText { .. } | Expr::Near { .. } => {
            QExpr::Literal(Literal::Bool(true))
        }
    }
}
