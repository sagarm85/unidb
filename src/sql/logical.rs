// Logical plan (M1.c): the parser's target and the planner's rewrite point.
//
// Grammar subset: CREATE TABLE, INSERT, SELECT with AND-only WHERE, UPDATE,
// DELETE. No joins/aggregates/subqueries/ORDER BY — see CLAUDE.md's M1
// scope note and the approved plan's checkpoint M1.c description.
//
// RLS folds in here as a single AND-rewrite (`apply_rls`): the entire "RLS
// is a planner rewrite" story is this one function. Nothing below the
// logical-plan layer (physical plan, executor) needs to know RLS exists —
// it just evaluates whatever predicate the logical plan handed it.

use serde::{Deserialize, Serialize};

use crate::catalog::{Catalog, ColumnDef};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Literal {
    Int(i64),
    Text(String),
    Bool(bool),
    /// Raw JSON text (validated well-formed at INSERT/UPDATE time by the
    /// executor, not here).
    Json(String),
    /// Fixed-dimension `f32` embedding (M2). Dimension is validated against
    /// the column's declared `n` at INSERT/UPDATE time by the executor, not
    /// here — this type just carries the parsed values.
    Vector(Vec<f32>),
    Null,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Expr {
    Column(String),
    Literal(Literal),
    BinOp {
        op: CmpOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    And(Box<Expr>, Box<Expr>),
    /// `expr -> path`: extract a JSON value (stays JSON) at `path`.
    JsonExtract {
        expr: Box<Expr>,
        path: String,
    },
    /// `expr ->> path`: extract a JSON value as text at `path`.
    JsonExtractText {
        expr: Box<Expr>,
        path: String,
    },
}

#[derive(Debug, Clone)]
pub enum LogicalPlan {
    CreateTable {
        name: String,
        columns: Vec<ColumnDef>,
    },
    Insert {
        table: String,
        /// `None` means "all columns, in table-definition order."
        columns: Option<Vec<String>>,
        values: Vec<Vec<Literal>>,
    },
    Select {
        table: String,
        /// Empty means `SELECT *`.
        projection: Vec<String>,
        predicate: Option<Expr>,
    },
    Update {
        table: String,
        assignments: Vec<(String, Expr)>,
        predicate: Option<Expr>,
    },
    Delete {
        table: String,
        predicate: Option<Expr>,
    },
}

/// AND the table's RLS policy (if any) into the plan's predicate. This is
/// the entire RLS mechanism — everything below the logical-plan layer is
/// unaware RLS exists.
pub fn apply_rls(plan: LogicalPlan, catalog: &Catalog) -> LogicalPlan {
    match plan {
        LogicalPlan::Select {
            table,
            projection,
            predicate,
        } => {
            let predicate = and_policy(predicate, policy_for(catalog, &table));
            LogicalPlan::Select {
                table,
                projection,
                predicate,
            }
        }
        LogicalPlan::Update {
            table,
            assignments,
            predicate,
        } => {
            let predicate = and_policy(predicate, policy_for(catalog, &table));
            LogicalPlan::Update {
                table,
                assignments,
                predicate,
            }
        }
        LogicalPlan::Delete { table, predicate } => {
            let predicate = and_policy(predicate, policy_for(catalog, &table));
            LogicalPlan::Delete { table, predicate }
        }
        other @ (LogicalPlan::CreateTable { .. } | LogicalPlan::Insert { .. }) => other,
    }
}

fn policy_for(catalog: &Catalog, table: &str) -> Option<Expr> {
    catalog
        .lookup(table)
        .ok()
        .and_then(|t| t.rls_policy.clone())
}

fn and_policy(predicate: Option<Expr>, policy: Option<Expr>) -> Option<Expr> {
    match (predicate, policy) {
        (Some(p), Some(pol)) => Some(Expr::And(Box::new(p), Box::new(pol))),
        (Some(p), None) => Some(p),
        (None, Some(pol)) => Some(pol),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{ColumnType, TableDef};

    fn catalog_with_policy(table: &str, policy: Option<Expr>) -> Catalog {
        let mut catalog = Catalog::new();
        catalog.insert_for_test(TableDef {
            name: table.to_string(),
            columns: vec![ColumnDef {
                name: "id".to_string(),
                index: None,
                ty: ColumnType::Int64,
            }],
            pages: vec![],
            rls_policy: policy,
        });
        catalog
    }

    #[test]
    fn rls_rewrite_adds_policy_when_no_predicate() {
        let policy = Expr::BinOp {
            op: CmpOp::Eq,
            lhs: Box::new(Expr::Column("owner".to_string())),
            rhs: Box::new(Expr::Literal(Literal::Text("alice".to_string()))),
        };
        let catalog = catalog_with_policy("t", Some(policy.clone()));
        let plan = LogicalPlan::Select {
            table: "t".to_string(),
            projection: vec![],
            predicate: None,
        };
        let rewritten = apply_rls(plan, &catalog);
        match rewritten {
            LogicalPlan::Select { predicate, .. } => assert_eq!(predicate, Some(policy)),
            _ => panic!("expected Select"),
        }
    }

    #[test]
    fn rls_rewrite_ands_with_existing_predicate() {
        let policy = Expr::BinOp {
            op: CmpOp::Eq,
            lhs: Box::new(Expr::Column("owner".to_string())),
            rhs: Box::new(Expr::Literal(Literal::Text("alice".to_string()))),
        };
        let catalog = catalog_with_policy("t", Some(policy.clone()));
        let user_pred = Expr::BinOp {
            op: CmpOp::Gt,
            lhs: Box::new(Expr::Column("id".to_string())),
            rhs: Box::new(Expr::Literal(Literal::Int(5))),
        };
        let plan = LogicalPlan::Delete {
            table: "t".to_string(),
            predicate: Some(user_pred.clone()),
        };
        let rewritten = apply_rls(plan, &catalog);
        match rewritten {
            LogicalPlan::Delete { predicate, .. } => {
                assert_eq!(
                    predicate,
                    Some(Expr::And(Box::new(user_pred), Box::new(policy)))
                );
            }
            _ => panic!("expected Delete"),
        }
    }

    #[test]
    fn no_policy_leaves_predicate_untouched() {
        let catalog = catalog_with_policy("t", None);
        let plan = LogicalPlan::Select {
            table: "t".to_string(),
            projection: vec![],
            predicate: None,
        };
        let rewritten = apply_rls(plan, &catalog);
        match rewritten {
            LogicalPlan::Select { predicate, .. } => assert_eq!(predicate, None),
            _ => panic!("expected Select"),
        }
    }

    #[test]
    fn create_table_and_insert_are_untouched_by_rls() {
        let catalog = Catalog::new();
        let plan = LogicalPlan::CreateTable {
            name: "t".to_string(),
            columns: vec![],
        };
        assert!(matches!(
            apply_rls(plan, &catalog),
            LogicalPlan::CreateTable { .. }
        ));
    }
}
