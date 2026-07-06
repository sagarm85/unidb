// SQL parser (M1.c): wraps `sqlparser`'s AST and converts it to our
// LogicalPlan. Grammar subset: CREATE TABLE, INSERT, SELECT with AND-only
// WHERE, UPDATE, DELETE — no joins/aggregates/subqueries/ORDER BY. Using
// `sqlparser` (CLAUDE.md's own deferred-crate note for M1) rather than
// hand-rolling a parser spends M1's budget on the executor/MVCC work that
// is the actual point of this milestone, not parser plumbing.

use sqlparser::ast::{
    self, BinaryOperator, DataType, Expr as SqlExpr, FromTable, SelectItem, SetExpr, Statement,
    TableFactor, TableObject, Value,
};
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::Parser as SqlParser;

use crate::{
    catalog::{ColumnDef, ColumnType},
    error::{DbError, Result},
};

use super::logical::{CmpOp, Expr, Literal, LogicalPlan};

/// Parse a SQL string (possibly multiple `;`-separated statements) into
/// logical plans, one per statement.
pub fn parse_sql(sql: &str) -> Result<Vec<LogicalPlan>> {
    let dialect = GenericDialect {};
    let statements =
        SqlParser::parse_sql(&dialect, sql).map_err(|e| DbError::SqlParse(e.to_string()))?;
    statements.into_iter().map(convert_statement).collect()
}

fn convert_statement(stmt: Statement) -> Result<LogicalPlan> {
    match stmt {
        Statement::CreateTable(ct) => convert_create_table(ct),
        Statement::Insert(ins) => convert_insert(ins),
        Statement::Query(q) => convert_query(*q),
        Statement::Update(u) => convert_update(u),
        Statement::Delete(d) => convert_delete(d),
        other => Err(DbError::SqlUnsupported(format!(
            "unsupported statement: {other}"
        ))),
    }
}

fn convert_create_table(ct: ast::CreateTable) -> Result<LogicalPlan> {
    let name = ct.name.to_string();
    let columns = ct
        .columns
        .into_iter()
        .map(|c| {
            Ok(ColumnDef {
                name: c.name.value,
                ty: convert_data_type(&c.data_type)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(LogicalPlan::CreateTable { name, columns })
}

fn convert_data_type(dt: &DataType) -> Result<ColumnType> {
    match dt {
        DataType::Int(_) | DataType::Integer(_) | DataType::SmallInt(_) | DataType::BigInt(_) => {
            Ok(ColumnType::Int64)
        }
        DataType::Text | DataType::Varchar(_) | DataType::Char(_) | DataType::String(_) => {
            Ok(ColumnType::Text)
        }
        DataType::Bool | DataType::Boolean => Ok(ColumnType::Bool),
        DataType::JSON => Ok(ColumnType::Json),
        other => Err(DbError::SqlUnsupported(format!(
            "unsupported column type: {other}"
        ))),
    }
}

fn convert_insert(ins: ast::Insert) -> Result<LogicalPlan> {
    let table = match ins.table {
        TableObject::TableName(name) => name.to_string(),
        other => {
            return Err(DbError::SqlUnsupported(format!(
                "unsupported INSERT target: {other:?}"
            )))
        }
    };
    let columns = if ins.columns.is_empty() {
        None
    } else {
        Some(ins.columns.iter().map(|c| c.to_string()).collect())
    };
    let source = ins
        .source
        .ok_or_else(|| DbError::SqlUnsupported("INSERT without VALUES is not supported".into()))?;
    let rows = match *source.body {
        SetExpr::Values(values) => values
            .rows
            .into_iter()
            .map(|row| {
                row.content
                    .into_iter()
                    .map(|e| convert_value_expr(&e))
                    .collect()
            })
            .collect::<Result<Vec<Vec<Literal>>>>()?,
        other => {
            return Err(DbError::SqlUnsupported(format!(
                "unsupported INSERT source: {other:?}"
            )))
        }
    };
    Ok(LogicalPlan::Insert {
        table,
        columns,
        values: rows,
    })
}

/// INSERT VALUES entries must be literals (no sub-expressions) in M1.
fn convert_value_expr(e: &SqlExpr) -> Result<Literal> {
    match e {
        SqlExpr::Value(vws) => convert_value(&vws.value),
        SqlExpr::UnaryOp {
            op: ast::UnaryOperator::Minus,
            expr,
        } => match convert_value_expr(expr)? {
            Literal::Int(n) => Ok(Literal::Int(-n)),
            other => Err(DbError::SqlUnsupported(format!(
                "unary minus not supported on {other:?}"
            ))),
        },
        other => Err(DbError::SqlUnsupported(format!(
            "unsupported literal in VALUES: {other:?}"
        ))),
    }
}

fn convert_value(v: &Value) -> Result<Literal> {
    match v {
        Value::Number(s, _) => s
            .parse::<i64>()
            .map(Literal::Int)
            .map_err(|_| DbError::SqlUnsupported(format!("unsupported numeric literal: {s}"))),
        Value::SingleQuotedString(s) => Ok(Literal::Text(s.clone())),
        Value::Boolean(b) => Ok(Literal::Bool(*b)),
        Value::Null => Ok(Literal::Null),
        other => Err(DbError::SqlUnsupported(format!(
            "unsupported literal: {other:?}"
        ))),
    }
}

fn convert_query(q: ast::Query) -> Result<LogicalPlan> {
    let select = match *q.body {
        SetExpr::Select(s) => *s,
        other => {
            return Err(DbError::SqlUnsupported(format!(
                "unsupported query body: {other:?}"
            )))
        }
    };
    let table = select
        .from
        .first()
        .ok_or_else(|| DbError::SqlUnsupported("SELECT without FROM is not supported".into()))?;
    let table_name = table_name_from_relation(&table.relation)?;
    let projection = convert_projection(select.projection)?;
    let predicate = select.selection.as_ref().map(convert_expr).transpose()?;
    Ok(LogicalPlan::Select {
        table: table_name,
        projection,
        predicate,
    })
}

fn convert_projection(items: Vec<SelectItem>) -> Result<Vec<String>> {
    let mut cols = Vec::new();
    for item in items {
        match item {
            SelectItem::Wildcard(_) => return Ok(Vec::new()),
            SelectItem::UnnamedExpr(SqlExpr::Identifier(ident)) => cols.push(ident.value),
            SelectItem::UnnamedExpr(SqlExpr::CompoundIdentifier(parts)) => {
                cols.push(column_name_from_parts(&parts));
            }
            other => {
                return Err(DbError::SqlUnsupported(format!(
                    "unsupported SELECT item: {other:?}"
                )))
            }
        }
    }
    Ok(cols)
}

fn convert_update(u: ast::Update) -> Result<LogicalPlan> {
    let table = table_name_from_relation(&u.table.relation)?;
    let assignments = u
        .assignments
        .into_iter()
        .map(|a| {
            let name = match a.target {
                ast::AssignmentTarget::ColumnName(name) => name.to_string(),
                ast::AssignmentTarget::Tuple(_) => {
                    return Err(DbError::SqlUnsupported(
                        "tuple assignment targets are not supported".into(),
                    ))
                }
            };
            Ok((name, convert_expr(&a.value)?))
        })
        .collect::<Result<Vec<_>>>()?;
    let predicate = u.selection.as_ref().map(convert_expr).transpose()?;
    Ok(LogicalPlan::Update {
        table,
        assignments,
        predicate,
    })
}

fn convert_delete(d: ast::Delete) -> Result<LogicalPlan> {
    let tables = match &d.from {
        FromTable::WithFromKeyword(t) | FromTable::WithoutKeyword(t) => t,
    };
    let table = tables
        .first()
        .ok_or_else(|| DbError::SqlUnsupported("DELETE without a table is not supported".into()))?;
    let table_name = table_name_from_relation(&table.relation)?;
    let predicate = d.selection.as_ref().map(convert_expr).transpose()?;
    Ok(LogicalPlan::Delete {
        table: table_name,
        predicate,
    })
}

fn table_name_from_relation(rel: &TableFactor) -> Result<String> {
    match rel {
        TableFactor::Table { name, .. } => Ok(name.to_string()),
        other => Err(DbError::SqlUnsupported(format!(
            "unsupported table reference: {other:?}"
        ))),
    }
}

fn column_name_from_parts(parts: &[ast::Ident]) -> String {
    parts.last().map(|i| i.value.clone()).unwrap_or_default()
}

fn convert_expr(e: &SqlExpr) -> Result<Expr> {
    match e {
        SqlExpr::Identifier(ident) => Ok(Expr::Column(ident.value.clone())),
        SqlExpr::CompoundIdentifier(parts) => Ok(Expr::Column(column_name_from_parts(parts))),
        SqlExpr::Value(vws) => convert_value(&vws.value).map(Expr::Literal),
        SqlExpr::BinaryOp { left, op, right } => convert_binary_op(left, op, right),
        SqlExpr::Nested(inner) => convert_expr(inner),
        other => Err(DbError::SqlUnsupported(format!(
            "unsupported expression: {other:?}"
        ))),
    }
}

fn convert_binary_op(left: &SqlExpr, op: &BinaryOperator, right: &SqlExpr) -> Result<Expr> {
    if matches!(op, BinaryOperator::Arrow | BinaryOperator::LongArrow) {
        let expr = Box::new(convert_expr(left)?);
        let path = match convert_expr(right)? {
            Expr::Literal(Literal::Text(s)) => s,
            _ => {
                return Err(DbError::SqlUnsupported(
                    "->/->> path must be a string literal".into(),
                ))
            }
        };
        return Ok(if matches!(op, BinaryOperator::Arrow) {
            Expr::JsonExtract { expr, path }
        } else {
            Expr::JsonExtractText { expr, path }
        });
    }

    let lhs = Box::new(convert_expr(left)?);
    let rhs = Box::new(convert_expr(right)?);
    match op {
        BinaryOperator::And => Ok(Expr::And(lhs, rhs)),
        BinaryOperator::Eq => Ok(Expr::BinOp {
            op: CmpOp::Eq,
            lhs,
            rhs,
        }),
        BinaryOperator::NotEq => Ok(Expr::BinOp {
            op: CmpOp::Ne,
            lhs,
            rhs,
        }),
        BinaryOperator::Lt => Ok(Expr::BinOp {
            op: CmpOp::Lt,
            lhs,
            rhs,
        }),
        BinaryOperator::Gt => Ok(Expr::BinOp {
            op: CmpOp::Gt,
            lhs,
            rhs,
        }),
        BinaryOperator::LtEq => Ok(Expr::BinOp {
            op: CmpOp::Le,
            lhs,
            rhs,
        }),
        BinaryOperator::GtEq => Ok(Expr::BinOp {
            op: CmpOp::Ge,
            lhs,
            rhs,
        }),
        BinaryOperator::Or => Err(DbError::SqlUnsupported(
            "OR is not supported in M1's WHERE subset (AND-only predicates)".into(),
        )),
        other => Err(DbError::SqlUnsupported(format!(
            "unsupported operator: {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_one(sql: &str) -> LogicalPlan {
        let mut plans = parse_sql(sql).unwrap();
        assert_eq!(plans.len(), 1);
        plans.remove(0)
    }

    #[test]
    fn parses_create_table() {
        let plan = parse_one("CREATE TABLE accounts (id INT, name TEXT, active BOOLEAN)");
        match plan {
            LogicalPlan::CreateTable { name, columns } => {
                assert_eq!(name, "accounts");
                assert_eq!(columns.len(), 3);
                assert_eq!(columns[0].ty, ColumnType::Int64);
                assert_eq!(columns[1].ty, ColumnType::Text);
                assert_eq!(columns[2].ty, ColumnType::Bool);
            }
            _ => panic!("expected CreateTable"),
        }
    }

    #[test]
    fn parses_create_table_with_json_column() {
        let plan = parse_one("CREATE TABLE t (id INT, data JSON)");
        match plan {
            LogicalPlan::CreateTable { columns, .. } => {
                assert_eq!(columns[1].ty, ColumnType::Json);
            }
            _ => panic!("expected CreateTable"),
        }
    }

    #[test]
    fn parses_insert_with_columns() {
        let plan = parse_one("INSERT INTO accounts (id, name) VALUES (1, 'alice')");
        match plan {
            LogicalPlan::Insert {
                table,
                columns,
                values,
            } => {
                assert_eq!(table, "accounts");
                assert_eq!(columns, Some(vec!["id".to_string(), "name".to_string()]));
                assert_eq!(
                    values,
                    vec![vec![Literal::Int(1), Literal::Text("alice".to_string())]]
                );
            }
            _ => panic!("expected Insert"),
        }
    }

    #[test]
    fn parses_insert_without_columns() {
        let plan = parse_one("INSERT INTO t VALUES (1, 'x')");
        match plan {
            LogicalPlan::Insert { columns, .. } => assert_eq!(columns, None),
            _ => panic!("expected Insert"),
        }
    }

    #[test]
    fn parses_select_star_with_and_predicate() {
        let plan = parse_one("SELECT * FROM accounts WHERE id = 1 AND name = 'alice'");
        match plan {
            LogicalPlan::Select {
                table,
                projection,
                predicate,
            } => {
                assert_eq!(table, "accounts");
                assert!(projection.is_empty());
                assert!(matches!(predicate, Some(Expr::And(_, _))));
            }
            _ => panic!("expected Select"),
        }
    }

    #[test]
    fn parses_select_with_projection() {
        let plan = parse_one("SELECT id, name FROM accounts");
        match plan {
            LogicalPlan::Select { projection, .. } => {
                assert_eq!(projection, vec!["id".to_string(), "name".to_string()]);
            }
            _ => panic!("expected Select"),
        }
    }

    #[test]
    fn rejects_or_predicate() {
        let err = parse_sql("SELECT * FROM t WHERE a = 1 OR b = 2");
        assert!(matches!(err, Err(DbError::SqlUnsupported(_))));
    }

    #[test]
    fn parses_update() {
        let plan = parse_one("UPDATE accounts SET balance = 100 WHERE id = 1");
        match plan {
            LogicalPlan::Update {
                table,
                assignments,
                predicate,
            } => {
                assert_eq!(table, "accounts");
                assert_eq!(assignments.len(), 1);
                assert_eq!(assignments[0].0, "balance");
                assert!(predicate.is_some());
            }
            _ => panic!("expected Update"),
        }
    }

    #[test]
    fn parses_delete() {
        let plan = parse_one("DELETE FROM accounts WHERE id = 1");
        match plan {
            LogicalPlan::Delete { table, predicate } => {
                assert_eq!(table, "accounts");
                assert!(predicate.is_some());
            }
            _ => panic!("expected Delete"),
        }
    }

    #[test]
    fn parses_json_extract_operators() {
        // `->`/`->>` bind looser than `=` under GenericDialect's precedence
        // table, so `data -> 'status' = 'active'` parses as
        // `data -> ('status' = 'active')` — explicit parens needed.
        let plan = parse_one("SELECT * FROM t WHERE (data -> 'status') = 'active'");
        match plan {
            LogicalPlan::Select { predicate, .. } => match predicate {
                Some(Expr::BinOp { lhs, .. }) => {
                    assert!(matches!(*lhs, Expr::JsonExtract { .. }));
                }
                _ => panic!("expected BinOp with JsonExtract lhs"),
            },
            _ => panic!("expected Select"),
        }
    }

    #[test]
    fn parses_json_extract_text_operator() {
        let plan = parse_one("SELECT * FROM t WHERE (data ->> 'name') = 'bob'");
        match plan {
            LogicalPlan::Select { predicate, .. } => match predicate {
                Some(Expr::BinOp { lhs, .. }) => {
                    assert!(matches!(*lhs, Expr::JsonExtractText { .. }));
                }
                _ => panic!("expected BinOp with JsonExtractText lhs"),
            },
            _ => panic!("expected Select"),
        }
    }
}
