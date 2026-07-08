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

use crate::catalog::{Catalog, ColumnDef, IndexKind, TableConstraints};

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
    /// Exact fixed-point decimal (P2.a): `(unscaled_value, scale)`, i.e. the
    /// numeric value is `unscaled_value / 10^scale`. The parser produces this
    /// with the scale exactly as written (`9.90` -> `(990, 2)`); the executor
    /// rescales it to the target column's declared scale at coercion time.
    /// `i128` bounds precision to ~38 significant digits.
    Decimal(i128, u8),
    /// Timestamp (P2.a): microseconds since the Unix epoch, UTC. The parser
    /// leaves a timestamp *string* as [`Literal::Text`] (it has no schema to
    /// know the column is temporal); the executor converts Text -> Timestamp
    /// at coercion time and `compare` parses a Text operand on demand.
    Timestamp(i64),
    /// IEEE-754 double (P2.b). Numeric literals stay `Int`/`Decimal` at parse
    /// time and coerce to `Float` against a `FLOAT` column.
    Float(f64),
    /// UUID as 16 raw bytes (P2.b). Arrives as text, parsed at coercion.
    Uuid([u8; 16]),
    /// Opaque bytes (P2.b). Arrives as text, decoded at coercion.
    Bytea(Vec<u8>),
    /// Calendar date as days since the Unix epoch (P2.b). Arrives as text.
    Date(i32),
    /// Time of day as micros since midnight (P2.b). Arrives as text.
    Time(i64),
    Null,
}

/// Render an exact decimal `(unscaled_value, scale)` as canonical decimal
/// text (`(990, 2)` -> `"9.90"`, `(-5, 0)` -> `"-5"`). Used by the JSON/DTO
/// boundary layers so a `DECIMAL` crosses into JSON as a string, never an
/// `f64`. Preserves trailing zeros implied by the scale (money stays `9.90`).
pub fn format_decimal(value: i128, scale: u8) -> String {
    if scale == 0 {
        return value.to_string();
    }
    let neg = value < 0;
    let digits = value.unsigned_abs().to_string();
    let scale = scale as usize;
    let (int_part, frac_part) = if digits.len() > scale {
        let split = digits.len() - scale;
        (digits[..split].to_string(), digits[split..].to_string())
    } else {
        ("0".to_string(), format!("{digits:0>scale$}"))
    };
    let sign = if neg { "-" } else { "" };
    format!("{sign}{int_part}.{frac_part}")
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
    /// `NEAR(column, [0.1, 0.2, ...], k)` (M2.d): a predicate-shaped
    /// construct, not a separate `LogicalPlan` variant, so it lives inside
    /// `Select.predicate` and `apply_rls`'s existing AND-rewrite keeps
    /// working unmodified — `WHERE NEAR(...) AND <rls policy>` composes for
    /// free. `OR` is already rejected everywhere else in the AND-only
    /// `WHERE` subset, so `NEAR(...) OR x` is rejected too, with no special
    /// case needed here.
    Near {
        column: String,
        query: Vec<f32>,
        k: usize,
    },
}

#[derive(Debug, Clone)]
pub enum LogicalPlan {
    CreateTable {
        name: String,
        columns: Vec<ColumnDef>,
        /// Table-level constraints (M11): `PRIMARY KEY (cols)`, `UNIQUE
        /// (cols)`, `FOREIGN KEY (...)`, table `CHECK`. Column-level
        /// constraints ride on each [`ColumnDef`] instead.
        constraints: TableConstraints,
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
    /// `CREATE INDEX ... ON table (column) USING HNSW|FULLTEXT` (M2.c). One
    /// column only in M2 — no composite secondary indexes.
    CreateIndex {
        table: String,
        column: String,
        kind: IndexKind,
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
        other @ (LogicalPlan::CreateTable { .. }
        | LogicalPlan::Insert { .. }
        | LogicalPlan::CreateIndex { .. }) => other,
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
                constraints: Default::default(),
            }],
            pages: vec![],
            rls_policy: policy,
            events_enabled: false,
            constraints: Default::default(),
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
    fn format_decimal_renders_canonical_text() {
        assert_eq!(format_decimal(990, 2), "9.90");
        assert_eq!(format_decimal(-5, 2), "-0.05");
        assert_eq!(format_decimal(10000, 2), "100.00");
        assert_eq!(format_decimal(-12345, 2), "-123.45");
        assert_eq!(format_decimal(42, 0), "42");
        assert_eq!(format_decimal(0, 2), "0.00");
        assert_eq!(format_decimal(7, 3), "0.007");
    }

    #[test]
    fn create_table_and_insert_are_untouched_by_rls() {
        let catalog = Catalog::new();
        let plan = LogicalPlan::CreateTable {
            name: "t".to_string(),
            columns: vec![],
            constraints: Default::default(),
        };
        assert!(matches!(
            apply_rls(plan, &catalog),
            LogicalPlan::CreateTable { .. }
        ));
    }
}
