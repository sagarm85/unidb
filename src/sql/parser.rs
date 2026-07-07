// SQL parser (M1.c): wraps `sqlparser`'s AST and converts it to our
// LogicalPlan. Grammar subset: CREATE TABLE, INSERT, SELECT with AND-only
// WHERE, UPDATE, DELETE — no joins/aggregates/subqueries/ORDER BY. Using
// `sqlparser` (CLAUDE.md's own deferred-crate note for M1) rather than
// hand-rolling a parser spends M1's budget on the executor/MVCC work that
// is the actual point of this milestone, not parser plumbing.

use sqlparser::ast::{
    self, Array as SqlArray, BinaryOperator, DataType, Expr as SqlExpr, FromTable, IndexType,
    SelectItem, SetExpr, Statement, TableFactor, TableObject, Value,
};
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::Parser as SqlParser;

use crate::{
    catalog::{ColumnDef, ColumnType, IndexKind},
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
        Statement::CreateIndex(ci) => convert_create_index(ci),
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
                index: None,
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
        // `VECTOR(n)` has no built-in sqlparser type; it falls through to
        // `DataType::Custom(name, modifiers)` (confirmed against sqlparser
        // 0.62.0's own AST — see the M2 plan's checkpoint M2.a notes).
        DataType::Custom(name, modifiers) if name.to_string().eq_ignore_ascii_case("vector") => {
            let dim = modifiers
                .first()
                .ok_or_else(|| DbError::SqlUnsupported("VECTOR requires a dimension".into()))?
                .parse::<u32>()
                .map_err(|_| {
                    DbError::SqlUnsupported("VECTOR dimension must be a positive integer".into())
                })?;
            if dim == 0 {
                return Err(DbError::SqlUnsupported(
                    "VECTOR dimension must be greater than 0".into(),
                ));
            }
            Ok(ColumnType::Vector(dim))
        }
        other => Err(DbError::SqlUnsupported(format!(
            "unsupported column type: {other}"
        ))),
    }
}

/// `CREATE INDEX ... ON table USING HNSW|FULLTEXT|BTREE (column)`. Note
/// `USING` comes *before* the column list — confirmed against `sqlparser`'s
/// own `parse_create_index` (it only looks for an optional `USING` clause
/// immediately after the table name, not after the column list; a
/// trailing-`USING` MySQL variant exists but isn't what this matches
/// against). None of these are built-in `sqlparser` index types, so they
/// arrive as `IndexType::Custom` — matched case-insensitively, same pattern
/// as `VECTOR(n)`'s `DataType::Custom` fallback. Exactly one column,
/// matching M2/M6's "no composite secondary indexes" scope.
fn convert_create_index(ci: ast::CreateIndex) -> Result<LogicalPlan> {
    let table = ci.table_name.to_string();
    let kind = match &ci.using {
        Some(IndexType::Custom(ident)) if ident.value.eq_ignore_ascii_case("hnsw") => {
            IndexKind::Hnsw
        }
        Some(IndexType::Custom(ident)) if ident.value.eq_ignore_ascii_case("fulltext") => {
            IndexKind::FullText
        }
        // Unlike HNSW/FULLTEXT, `BTREE` is one of `sqlparser`'s own built-in
        // `IndexType` variants (it's a real, common index type name across
        // Postgres/MySQL) — it arrives as `IndexType::BTree` directly, not
        // `IndexType::Custom`.
        Some(IndexType::BTree) => IndexKind::BTree,
        other => {
            return Err(DbError::SqlUnsupported(format!(
                "unsupported index type: {other:?} (expected USING HNSW, FULLTEXT, or BTREE)"
            )))
        }
    };
    if ci.columns.len() != 1 {
        return Err(DbError::SqlUnsupported(
            "CREATE INDEX supports exactly one column in M2".into(),
        ));
    }
    let column = match &ci.columns[0].column.expr {
        SqlExpr::Identifier(ident) => ident.value.clone(),
        other => {
            return Err(DbError::SqlUnsupported(format!(
                "unsupported index column expression: {other:?}"
            )))
        }
    };
    Ok(LogicalPlan::CreateIndex {
        table,
        column,
        kind,
    })
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
        SqlExpr::Array(arr) => convert_array_literal(arr),
        other => Err(DbError::SqlUnsupported(format!(
            "unsupported literal in VALUES: {other:?}"
        ))),
    }
}

/// `[0.1, 0.2, ...]` array literals are only meaningful as `VECTOR(n)`
/// values in M2 — parse every element as `f32`. This is a narrow
/// float-parsing fallback scoped to array-literal elements only;
/// `convert_value`'s general numeric path stays `i64`-only.
fn convert_array_literal(arr: &SqlArray) -> Result<Literal> {
    let values = arr
        .elem
        .iter()
        .map(|e| match e {
            SqlExpr::Value(vws) => match &vws.value {
                Value::Number(s, _) => s
                    .parse::<f32>()
                    .map_err(|_| DbError::SqlUnsupported(format!("invalid vector element: {s}"))),
                other => Err(DbError::SqlUnsupported(format!(
                    "unsupported vector element: {other:?}"
                ))),
            },
            SqlExpr::UnaryOp {
                op: ast::UnaryOperator::Minus,
                expr,
            } => match expr.as_ref() {
                SqlExpr::Value(vws) => match &vws.value {
                    Value::Number(s, _) => s.parse::<f32>().map(|v| -v).map_err(|_| {
                        DbError::SqlUnsupported(format!("invalid vector element: {s}"))
                    }),
                    other => Err(DbError::SqlUnsupported(format!(
                        "unsupported vector element: {other:?}"
                    ))),
                },
                other => Err(DbError::SqlUnsupported(format!(
                    "unsupported vector element: {other:?}"
                ))),
            },
            other => Err(DbError::SqlUnsupported(format!(
                "unsupported vector element: {other:?}"
            ))),
        })
        .collect::<Result<Vec<f32>>>()?;
    Ok(Literal::Vector(values))
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
        SqlExpr::Function(func) if func.name.to_string().eq_ignore_ascii_case("near") => {
            convert_near(func)
        }
        other => Err(DbError::SqlUnsupported(format!(
            "unsupported expression: {other:?}"
        ))),
    }
}

/// `NEAR(column, [0.1, 0.2, ...], k)` parses today, unmodified, as an
/// ordinary `SqlExpr::Function` — no grammar changes needed (same "spend
/// the budget on the executor, not the parser" logic that motivated using
/// `sqlparser` in the first place).
fn convert_near(func: &ast::Function) -> Result<Expr> {
    let args = match &func.args {
        ast::FunctionArguments::List(list) => &list.args,
        _ => {
            return Err(DbError::SqlUnsupported(
                "NEAR requires (column, vector, k) arguments".into(),
            ))
        }
    };
    if args.len() != 3 {
        return Err(DbError::SqlUnsupported(
            "NEAR requires exactly 3 arguments: (column, vector, k)".into(),
        ));
    }

    let column = match &args[0] {
        ast::FunctionArg::Unnamed(ast::FunctionArgExpr::Expr(SqlExpr::Identifier(ident))) => {
            ident.value.clone()
        }
        other => {
            return Err(DbError::SqlUnsupported(format!(
                "NEAR's first argument must be a column name, got {other:?}"
            )))
        }
    };

    let query = match &args[1] {
        ast::FunctionArg::Unnamed(ast::FunctionArgExpr::Expr(SqlExpr::Array(arr))) => {
            match convert_array_literal(arr)? {
                Literal::Vector(v) => v,
                _ => unreachable!("convert_array_literal always returns Literal::Vector"),
            }
        }
        other => {
            return Err(DbError::SqlUnsupported(format!(
                "NEAR's second argument must be a vector literal, got {other:?}"
            )))
        }
    };

    let k = match &args[2] {
        ast::FunctionArg::Unnamed(ast::FunctionArgExpr::Expr(SqlExpr::Value(vws))) => {
            match &vws.value {
                Value::Number(s, _) => s.parse::<usize>().map_err(|_| {
                    DbError::SqlUnsupported(format!("NEAR's k must be a non-negative integer: {s}"))
                })?,
                other => {
                    return Err(DbError::SqlUnsupported(format!(
                        "NEAR's third argument must be an integer k, got {other:?}"
                    )))
                }
            }
        }
        other => {
            return Err(DbError::SqlUnsupported(format!(
                "NEAR's third argument must be an integer k, got {other:?}"
            )))
        }
    };

    Ok(Expr::Near { column, query, k })
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
    fn parses_create_table_with_vector_column() {
        let plan = parse_one("CREATE TABLE t (id INT, embedding VECTOR(4))");
        match plan {
            LogicalPlan::CreateTable { columns, .. } => {
                assert_eq!(columns[1].ty, ColumnType::Vector(4));
            }
            _ => panic!("expected CreateTable"),
        }
    }

    #[test]
    fn rejects_zero_dimension_vector() {
        let err = parse_sql("CREATE TABLE t (embedding VECTOR(0))");
        assert!(matches!(err, Err(DbError::SqlUnsupported(_))));
    }

    #[test]
    fn parses_insert_with_vector_literal() {
        let plan = parse_one("INSERT INTO t VALUES (1, [0.1, 0.2, -0.3, 0.4])");
        match plan {
            LogicalPlan::Insert { values, .. } => {
                assert_eq!(
                    values,
                    vec![vec![
                        Literal::Int(1),
                        Literal::Vector(vec![0.1, 0.2, -0.3, 0.4])
                    ]]
                );
            }
            _ => panic!("expected Insert"),
        }
    }

    #[test]
    fn parses_create_index_hnsw() {
        let plan = parse_one("CREATE INDEX idx ON t USING HNSW (embedding)");
        match plan {
            LogicalPlan::CreateIndex {
                table,
                column,
                kind,
            } => {
                assert_eq!(table, "t");
                assert_eq!(column, "embedding");
                assert_eq!(kind, IndexKind::Hnsw);
            }
            _ => panic!("expected CreateIndex"),
        }
    }

    #[test]
    fn parses_create_index_fulltext_case_insensitive() {
        let plan = parse_one("CREATE INDEX idx ON t USING fulltext (body)");
        match plan {
            LogicalPlan::CreateIndex { column, kind, .. } => {
                assert_eq!(column, "body");
                assert_eq!(kind, IndexKind::FullText);
            }
            _ => panic!("expected CreateIndex"),
        }
    }

    #[test]
    fn parses_create_index_btree_case_insensitive() {
        let plan = parse_one("CREATE INDEX idx ON t USING btree (id)");
        match plan {
            LogicalPlan::CreateIndex { column, kind, .. } => {
                assert_eq!(column, "id");
                assert_eq!(kind, IndexKind::BTree);
            }
            _ => panic!("expected CreateIndex"),
        }
    }

    #[test]
    fn rejects_create_index_with_unsupported_using() {
        let err = parse_sql("CREATE INDEX idx ON t USING GIST (id)");
        assert!(matches!(err, Err(DbError::SqlUnsupported(_))));
    }

    #[test]
    fn parses_near_in_where_clause() {
        let plan = parse_one("SELECT * FROM t WHERE NEAR(embedding, [0.1, 0.2], 5)");
        match plan {
            LogicalPlan::Select { predicate, .. } => {
                assert_eq!(
                    predicate,
                    Some(Expr::Near {
                        column: "embedding".to_string(),
                        query: vec![0.1, 0.2],
                        k: 5,
                    })
                );
            }
            _ => panic!("expected Select"),
        }
    }

    #[test]
    fn parses_near_anded_with_other_predicate() {
        let plan = parse_one("SELECT * FROM t WHERE NEAR(embedding, [1.0], 3) AND active = true");
        match plan {
            LogicalPlan::Select { predicate, .. } => {
                assert!(matches!(predicate, Some(Expr::And(_, _))));
            }
            _ => panic!("expected Select"),
        }
    }

    #[test]
    fn rejects_near_with_wrong_arg_count() {
        let err = parse_sql("SELECT * FROM t WHERE NEAR(embedding, [1.0])");
        assert!(matches!(err, Err(DbError::SqlUnsupported(_))));
    }

    #[test]
    fn rejects_near_with_non_column_first_arg() {
        let err = parse_sql("SELECT * FROM t WHERE NEAR('x', [1.0], 3)");
        assert!(matches!(err, Err(DbError::SqlUnsupported(_))));
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
