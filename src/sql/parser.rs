// SQL parser (M1.c): wraps `sqlparser`'s AST and converts it to our
// LogicalPlan. Grammar subset: CREATE TABLE, INSERT, SELECT with AND-only
// WHERE, UPDATE, DELETE — no joins/aggregates/subqueries/ORDER BY. Using
// `sqlparser` (CLAUDE.md's own deferred-crate note for M1) rather than
// hand-rolling a parser spends M1's budget on the executor/MVCC work that
// is the actual point of this milestone, not parser plumbing.

use sqlparser::ast::{
    self, AlterTableOperation, Array as SqlArray, BinaryOperator, DataType, ExactNumberInfo,
    Expr as SqlExpr, FromTable, IndexType, JoinConstraint, JoinOperator, ObjectType, SelectItem,
    SetExpr, Statement, TableFactor, TableObject, TableWithJoins, UnaryOperator, Value,
};
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::Parser as SqlParser;

use crate::{
    catalog::{
        ColumnConstraints, ColumnDef, ColumnType, ForeignKey, ForeignKeyRef, IndexKind,
        TableConstraints,
    },
    error::{DbError, Result},
};

use super::logical::{ArithOp, CmpOp, Expr, Literal, LogicalPlan};
use super::query::{AggFunc, FromNode, JoinType, OrderKey, Projection, QExpr, QuerySpec, TableRef};

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
        Statement::AlterTable(at) => convert_alter_table(at),
        Statement::Drop {
            object_type,
            names,
            if_exists,
            ..
        } => convert_drop(object_type, names, if_exists),
        Statement::Truncate(t) => convert_truncate(t),
        Statement::Explain {
            analyze, statement, ..
        } => match *statement {
            Statement::Query(q) => Ok(LogicalPlan::Explain {
                analyze,
                spec: query_to_spec(*q)?,
            }),
            other => Err(DbError::SqlUnsupported(format!(
                "EXPLAIN is only supported for SELECT queries in v1, got: {other}"
            ))),
        },
        Statement::Analyze(a) => {
            let table = a
                .table_name
                .ok_or_else(|| {
                    DbError::SqlUnsupported("ANALYZE requires a table name in v1".into())
                })?
                .to_string();
            Ok(LogicalPlan::Analyze { table })
        }
        other => Err(DbError::SqlUnsupported(format!(
            "unsupported statement: {other}"
        ))),
    }
}

/// `ALTER TABLE t <op>` (P2.c). Exactly one operation per statement in v1;
/// only `ADD COLUMN` and `DROP COLUMN` are supported.
fn convert_alter_table(at: ast::AlterTable) -> Result<LogicalPlan> {
    let table = at.name.to_string();
    if at.operations.len() != 1 {
        return Err(DbError::SqlUnsupported(
            "ALTER TABLE supports exactly one operation per statement".into(),
        ));
    }
    match at.operations.into_iter().next().expect("len checked") {
        AlterTableOperation::AddColumn { column_def, .. } => {
            let column = convert_column_def(column_def)?;
            Ok(LogicalPlan::AlterTableAddColumn { table, column })
        }
        AlterTableOperation::DropColumn {
            column_names,
            if_exists,
            ..
        } => {
            if column_names.len() != 1 {
                return Err(DbError::SqlUnsupported(
                    "DROP COLUMN supports exactly one column per statement".into(),
                ));
            }
            Ok(LogicalPlan::AlterTableDropColumn {
                table,
                column: column_names[0].value.clone(),
                if_exists,
            })
        }
        other => Err(DbError::SqlUnsupported(format!(
            "unsupported ALTER TABLE operation: {other:?}"
        ))),
    }
}

/// `DROP TABLE [IF EXISTS] t` (P2.c). Only `TABLE`, exactly one name.
fn convert_drop(
    object_type: ObjectType,
    names: Vec<ast::ObjectName>,
    if_exists: bool,
) -> Result<LogicalPlan> {
    if object_type != ObjectType::Table {
        return Err(DbError::SqlUnsupported(format!(
            "DROP {object_type:?} is not supported (only DROP TABLE)"
        )));
    }
    if names.len() != 1 {
        return Err(DbError::SqlUnsupported(
            "DROP TABLE supports exactly one table per statement".into(),
        ));
    }
    Ok(LogicalPlan::DropTable {
        table: names[0].to_string(),
        if_exists,
    })
}

/// `TRUNCATE [TABLE] t` (P2.c). Exactly one table.
fn convert_truncate(t: ast::Truncate) -> Result<LogicalPlan> {
    if t.table_names.len() != 1 {
        return Err(DbError::SqlUnsupported(
            "TRUNCATE supports exactly one table per statement".into(),
        ));
    }
    Ok(LogicalPlan::Truncate {
        table: t.table_names[0].name.to_string(),
    })
}

fn convert_create_table(ct: ast::CreateTable) -> Result<LogicalPlan> {
    let name = ct.name.to_string();
    let mut columns = ct
        .columns
        .into_iter()
        .map(convert_column_def)
        .collect::<Result<Vec<_>>>()?;
    let constraints = convert_table_constraints(&ct.constraints)?;
    // A table-level `PRIMARY KEY (a, b)` makes each named column `NOT NULL`
    // (SQL requires PK columns to be non-null); fold that into the column's
    // own constraint flags so NOT-NULL enforcement has a single source.
    for pk_col in &constraints.primary_key {
        if let Some(c) = columns.iter_mut().find(|c| &c.name == pk_col) {
            c.constraints.not_null = true;
        }
    }
    Ok(LogicalPlan::CreateTable {
        name,
        columns,
        constraints,
    })
}

/// Map one `sqlparser` `ColumnDef` — name, data type, and the per-column
/// `options` list that `convert_create_table` used to drop entirely — into
/// our [`ColumnDef`] with its [`ColumnConstraints`] populated (M11).
fn convert_column_def(c: ast::ColumnDef) -> Result<ColumnDef> {
    let mut cons = ColumnConstraints::default();
    for opt in &c.options {
        match &opt.option {
            ast::ColumnOption::NotNull => cons.not_null = true,
            // An explicit `NULL` marker is the default; nothing to record.
            ast::ColumnOption::Null => {}
            ast::ColumnOption::Default(expr) => cons.default = Some(convert_value_expr(expr)?),
            ast::ColumnOption::Unique(_) => cons.unique = true,
            ast::ColumnOption::PrimaryKey(_) => cons.primary_key = true,
            ast::ColumnOption::ForeignKey(fk) => {
                cons.references = Some(ForeignKeyRef {
                    table: fk.foreign_table.to_string(),
                    column: fk.referred_columns.first().map(|i| i.value.clone()),
                });
            }
            ast::ColumnOption::Check(cc) => cons.check = Some(convert_expr(&cc.expr)?),
            // `GENERATED ... AS IDENTITY` (P2.d): auto-fill from the table's
            // serial counter, same mechanism as `SERIAL`.
            ast::ColumnOption::Generated { .. } => cons.identity = true,
            other => {
                return Err(DbError::SqlUnsupported(format!(
                    "unsupported column option: {other:?}"
                )))
            }
        }
    }
    // `SERIAL`/`BIGSERIAL`/`SMALLSERIAL` (P2.d) parse as a custom type name; map
    // them to an `Int64` identity column (auto-filled from the table counter).
    let ty = if is_serial_type(&c.data_type) {
        cons.identity = true;
        ColumnType::Int64
    } else {
        convert_data_type(&c.data_type)?
    };
    Ok(ColumnDef {
        name: c.name.value,
        ty,
        index: None,
        index_root: None,
        unique_index_root: None,
        dropped: false,
        constraints: cons,
    })
}

/// Whether a data type is a `SERIAL` pseudo-type (P2.d). These have no
/// built-in `sqlparser` variant, so they arrive as `DataType::Custom`.
fn is_serial_type(dt: &DataType) -> bool {
    matches!(dt, DataType::Custom(name, _)
    if matches!(
        name.to_string().to_ascii_lowercase().as_str(),
        "serial" | "bigserial" | "smallserial" | "serial2" | "serial4" | "serial8"
    ))
}

/// Map the table-level `constraints` list (`PRIMARY KEY (..)`, `UNIQUE (..)`,
/// `FOREIGN KEY (..) REFERENCES ..`, table `CHECK (..)`) into
/// [`TableConstraints`] (M11).
fn convert_table_constraints(constraints: &[ast::TableConstraint]) -> Result<TableConstraints> {
    let mut tc = TableConstraints::default();
    for c in constraints {
        match c {
            ast::TableConstraint::PrimaryKey(pk) => {
                tc.primary_key = index_columns_to_names(&pk.columns)?;
            }
            ast::TableConstraint::Unique(u) => {
                tc.unique.push(index_columns_to_names(&u.columns)?);
            }
            ast::TableConstraint::ForeignKey(fk) => {
                tc.foreign_keys.push(ForeignKey {
                    columns: fk.columns.iter().map(|i| i.value.clone()).collect(),
                    ref_table: fk.foreign_table.to_string(),
                    ref_columns: fk
                        .referred_columns
                        .iter()
                        .map(|i| i.value.clone())
                        .collect(),
                });
            }
            ast::TableConstraint::Check(cc) => {
                tc.checks.push(convert_expr(&cc.expr)?);
            }
            other => {
                return Err(DbError::SqlUnsupported(format!(
                    "unsupported table constraint: {other:?}"
                )))
            }
        }
    }
    Ok(tc)
}

/// Extract plain column names from a constraint's `IndexColumn` list, which
/// wraps each column in an `OrderByExpr`; only bare identifiers are
/// supported (no expressions / ordering in a constraint column list).
fn index_columns_to_names(cols: &[ast::IndexColumn]) -> Result<Vec<String>> {
    cols.iter()
        .map(|ic| match &ic.column.expr {
            SqlExpr::Identifier(ident) => Ok(ident.value.clone()),
            other => Err(DbError::SqlUnsupported(format!(
                "unsupported constraint column expression: {other:?}"
            ))),
        })
        .collect()
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
        // Exact fixed-point (P2.a). `DECIMAL`/`NUMERIC`/`DEC` are synonyms.
        DataType::Decimal(info)
        | DataType::Numeric(info)
        | DataType::Dec(info)
        | DataType::BigDecimal(info)
        | DataType::BigNumeric(info) => convert_decimal_type(info),
        // Timestamp (P2.a): all zone variants store UTC micros in v1; the
        // precision hint is ignored (we always keep microsecond resolution).
        DataType::Timestamp(_, _) | DataType::TimestampNtz(_) => Ok(ColumnType::Timestamp),
        // Floating point (P2.b): every spelling collapses to f64.
        DataType::Float(_)
        | DataType::FloatUnsigned(_)
        | DataType::Real
        | DataType::RealUnsigned
        | DataType::Float4
        | DataType::Float8
        | DataType::Float32
        | DataType::Float64
        | DataType::Double(_)
        | DataType::DoubleUnsigned(_)
        | DataType::DoublePrecision => Ok(ColumnType::Float),
        DataType::Uuid => Ok(ColumnType::Uuid),
        // Opaque binary (P2.b): every blob/binary spelling maps to BYTEA.
        DataType::Bytea
        | DataType::Blob(_)
        | DataType::TinyBlob
        | DataType::MediumBlob
        | DataType::LongBlob
        | DataType::Binary(_)
        | DataType::Varbinary(_) => Ok(ColumnType::Bytea),
        DataType::Date => Ok(ColumnType::Date),
        DataType::Time(_, _) => Ok(ColumnType::Time),
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

/// Maximum DECIMAL precision — bounded by the `i128` backing store (`i128`
/// holds 39 decimal digits, but 38 is the largest that fits *every* value at
/// that width, matching SQL Server / common `DECIMAL(38, s)` practice).
pub(crate) const MAX_DECIMAL_PRECISION: u8 = 38;

/// Map sqlparser's `ExactNumberInfo` to `ColumnType::Decimal(precision,
/// scale)`. A bare `DECIMAL` defaults to `(38, 0)` (integer-valued exact
/// numeric); `DECIMAL(p)` defaults scale to 0. Validates `1 <= p <= 38` and
/// `0 <= s <= p`, so a bad type is rejected at `CREATE TABLE` rather than at
/// first insert.
fn convert_decimal_type(info: &ExactNumberInfo) -> Result<ColumnType> {
    let (precision, scale) = match info {
        ExactNumberInfo::None => (MAX_DECIMAL_PRECISION as u64, 0i64),
        ExactNumberInfo::Precision(p) => (*p, 0),
        ExactNumberInfo::PrecisionAndScale(p, s) => (*p, *s),
    };
    if precision == 0 || precision > MAX_DECIMAL_PRECISION as u64 {
        return Err(DbError::SqlUnsupported(format!(
            "DECIMAL precision must be between 1 and {MAX_DECIMAL_PRECISION}, got {precision}"
        )));
    }
    if scale < 0 || scale as u64 > precision {
        return Err(DbError::SqlUnsupported(format!(
            "DECIMAL scale must be between 0 and the precision ({precision}), got {scale}"
        )));
    }
    Ok(ColumnType::Decimal(precision as u8, scale as u8))
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
            Literal::Decimal(v, scale) => Ok(Literal::Decimal(-v, scale)),
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
        Value::Number(s, _) => convert_number_literal(s),
        Value::SingleQuotedString(s) => Ok(Literal::Text(s.clone())),
        Value::Boolean(b) => Ok(Literal::Bool(*b)),
        Value::Null => Ok(Literal::Null),
        // `$n` bind parameter (P2.e): carried through as a placeholder and
        // substituted by `bind_params` before execution.
        Value::Placeholder(p) => parse_placeholder(p),
        other => Err(DbError::SqlUnsupported(format!(
            "unsupported literal: {other:?}"
        ))),
    }
}

/// Parse a `$n` placeholder into a 1-based [`Literal::Param`] (P2.e). Only the
/// `$n` form is supported (not `?` positional or `:name`).
fn parse_placeholder(p: &str) -> Result<Literal> {
    let n = p
        .strip_prefix('$')
        .and_then(|d| d.parse::<usize>().ok())
        .filter(|&n| n >= 1)
        .ok_or_else(|| {
            DbError::SqlUnsupported(format!(
                "unsupported bind parameter '{p}' (use $1, $2, ...)"
            ))
        })?;
    Ok(Literal::Param(n))
}

/// A bare numeric literal. Integers stay [`Literal::Int`]; anything with a
/// fractional point becomes an exact [`Literal::Decimal`] carrying the scale
/// exactly as written (`9.90` -> `(990, 2)`), which the executor then rescales
/// to the target column. This keeps money literals exact end-to-end — never
/// routed through `f64` — even before the column type is known. A `DECIMAL`
/// value can still land in a `FLOAT` column: the executor's `coerce_value`
/// converts it there (P2.b).
fn convert_number_literal(s: &str) -> Result<Literal> {
    if s.contains('.') {
        parse_decimal_literal(s)
    } else {
        s.parse::<i64>()
            .map(Literal::Int)
            .map_err(|_| DbError::SqlUnsupported(format!("unsupported numeric literal: {s}")))
    }
}

/// Parse a fixed-point decimal string (`"-12.340"`) into `(unscaled i128,
/// scale)`. Exponent forms (`1e3`) are rejected — SQL numeric literals in this
/// subset are plain fixed-point.
fn parse_decimal_literal(s: &str) -> Result<Literal> {
    let invalid = || DbError::SqlUnsupported(format!("unsupported numeric literal: {s}"));
    let (neg, digits) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let (int_part, frac_part) = digits.split_once('.').ok_or_else(invalid)?;
    // Reject a second dot or any non-digit (e.g. an exponent marker).
    if !int_part.bytes().all(|b| b.is_ascii_digit())
        || !frac_part.bytes().all(|b| b.is_ascii_digit())
        || (int_part.is_empty() && frac_part.is_empty())
    {
        return Err(invalid());
    }
    let scale = u8::try_from(frac_part.len())
        .map_err(|_| DbError::SqlUnsupported(format!("decimal scale too large: {s}")))?;
    let combined = format!("{int_part}{frac_part}");
    let magnitude = if combined.is_empty() {
        0i128
    } else {
        combined.parse::<i128>().map_err(|_| invalid())?
    };
    let value = if neg { -magnitude } else { magnitude };
    Ok(Literal::Decimal(value, scale))
}

fn convert_query(q: ast::Query) -> Result<LogicalPlan> {
    // Destructure up front so `select` (moved out of `*body`) doesn't leave `q`
    // partially moved when the Phase-4 path needs the clause flags.
    let ast::Query {
        with,
        body,
        order_by,
        limit_clause,
        ..
    } = q;
    let select = match *body {
        SetExpr::Select(s) => *s,
        other => {
            return Err(DbError::SqlUnsupported(format!(
                "unsupported query body: {other:?}"
            )))
        }
    };

    // Route to the Phase-4 query path when the query uses any construct beyond a
    // trivial single-table filter/project: a join, GROUP BY / HAVING, DISTINCT,
    // ORDER BY, LIMIT/OFFSET, or an aggregate in the SELECT list. Everything
    // else stays a `LogicalPlan::Select` (its concurrent-read fast path + every
    // pre-P4 test are unchanged).
    let has_join =
        select.from.len() > 1 || select.from.first().is_some_and(|t| !t.joins.is_empty());
    let group_by_present = match &select.group_by {
        ast::GroupByExpr::Expressions(exprs, _) => !exprs.is_empty(),
        ast::GroupByExpr::All(_) => true,
    };
    let projection_has_agg = select.projection.iter().any(select_item_has_aggregate);
    let has_subquery = select.selection.as_ref().is_some_and(expr_has_subquery)
        || select.projection.iter().any(select_item_has_subquery);
    // Milestone 18, Epic C: a SELECT over an `information_schema.*` /
    // `unidb_catalog.*` virtual relation is routed through the Phase-4 query
    // path so a single, virtual-scan-aware executor serves it — the row-at-a-time
    // `LogicalPlan::Select` path has no notion of synthesized relations.
    let from_is_introspection = select.from.first().is_some_and(|t| {
        table_name_from_relation(&t.relation)
            .map(|n| crate::sql::information_schema::is_virtual_relation(&n))
            .unwrap_or(false)
    });
    let needs_query = has_join
        || group_by_present
        || select.having.is_some()
        || select.distinct.is_some()
        || order_by.is_some()
        || limit_clause.is_some()
        || with.is_some()
        || projection_has_agg
        || has_subquery
        || from_is_introspection;
    if needs_query {
        return convert_query_spec(select, with, order_by, limit_clause);
    }

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

/// Build a [`LogicalPlan::Query`] for a Phase-4 SELECT: joins, aggregates,
/// grouping, sort, distinct, limit, subqueries, CTEs.
fn convert_query_spec(
    select: ast::Select,
    with: Option<ast::With>,
    order_by: Option<ast::OrderBy>,
    limit_clause: Option<ast::LimitClause>,
) -> Result<LogicalPlan> {
    Ok(LogicalPlan::Query(build_query_spec(
        select,
        with,
        order_by,
        limit_clause,
    )?))
}

/// Convert a whole `sqlparser` [`ast::Query`] into a [`QuerySpec`] (always the
/// Phase-4 form). Used for CTE bodies and scalar/IN/EXISTS subqueries.
fn query_to_spec(q: ast::Query) -> Result<QuerySpec> {
    let ast::Query {
        with,
        body,
        order_by,
        limit_clause,
        ..
    } = q;
    let select = match *body {
        SetExpr::Select(s) => *s,
        other => {
            return Err(DbError::SqlUnsupported(format!(
                "unsupported subquery body: {other:?}"
            )))
        }
    };
    build_query_spec(select, with, order_by, limit_clause)
}

fn build_query_spec(
    select: ast::Select,
    with: Option<ast::With>,
    order_by: Option<ast::OrderBy>,
    limit_clause: Option<ast::LimitClause>,
) -> Result<QuerySpec> {
    let with_ctes = match with {
        None => Vec::new(),
        Some(w) => {
            if w.recursive {
                return Err(DbError::SqlUnsupported(
                    "recursive CTEs (WITH RECURSIVE) are not supported in v1".into(),
                ));
            }
            w.cte_tables
                .into_iter()
                .map(|cte| Ok((cte.alias.name.value.clone(), query_to_spec(*cte.query)?)))
                .collect::<Result<Vec<_>>>()?
        }
    };

    let group_by = match select.group_by {
        ast::GroupByExpr::Expressions(exprs, _) => exprs
            .iter()
            .map(convert_qexpr)
            .collect::<Result<Vec<_>>>()?,
        ast::GroupByExpr::All(_) => {
            return Err(DbError::SqlUnsupported(
                "GROUP BY ALL is not supported".into(),
            ))
        }
    };
    let having = select.having.as_ref().map(convert_qexpr).transpose()?;
    let distinct = match &select.distinct {
        None => false,
        Some(ast::Distinct::Distinct) => true,
        Some(other) => {
            return Err(DbError::SqlUnsupported(format!(
                "unsupported DISTINCT form: {other:?}"
            )))
        }
    };
    let order_by = convert_order_by(order_by)?;
    let (limit, offset) = convert_limit(limit_clause)?;

    let from = convert_from(&select.from)?;
    let selection = select.selection.as_ref().map(convert_qexpr).transpose()?;
    let projection = convert_query_projection(select.projection)?;
    Ok(QuerySpec {
        with: with_ctes,
        from,
        selection,
        projection,
        group_by,
        having,
        distinct,
        order_by,
        limit,
        offset,
    })
}

/// Map an aggregate function name to its [`AggFunc`], or `None` if it isn't one.
fn agg_func_from_name(name: &str) -> Option<AggFunc> {
    match name.to_ascii_uppercase().as_str() {
        "COUNT" => Some(AggFunc::Count),
        "SUM" => Some(AggFunc::Sum),
        "AVG" => Some(AggFunc::Avg),
        "MIN" => Some(AggFunc::Min),
        "MAX" => Some(AggFunc::Max),
        _ => None,
    }
}

fn select_item_has_aggregate(item: &SelectItem) -> bool {
    match item {
        SelectItem::UnnamedExpr(e) => expr_has_aggregate(e),
        SelectItem::ExprWithAlias { expr, .. } => expr_has_aggregate(expr),
        _ => false,
    }
}

fn select_item_has_subquery(item: &SelectItem) -> bool {
    match item {
        SelectItem::UnnamedExpr(e) => expr_has_subquery(e),
        SelectItem::ExprWithAlias { expr, .. } => expr_has_subquery(expr),
        _ => false,
    }
}

/// Whether a `sqlparser` expression needs the Phase-4 query path — a subquery
/// (scalar/IN/EXISTS) or an IN-list (the single-table Select path handles
/// neither).
fn expr_has_subquery(e: &SqlExpr) -> bool {
    match e {
        SqlExpr::Subquery(_)
        | SqlExpr::Exists { .. }
        | SqlExpr::InSubquery { .. }
        | SqlExpr::InList { .. } => true,
        SqlExpr::BinaryOp { left, right, .. } => {
            expr_has_subquery(left) || expr_has_subquery(right)
        }
        SqlExpr::UnaryOp { expr, .. } | SqlExpr::Nested(expr) => expr_has_subquery(expr),
        SqlExpr::IsNull(e) | SqlExpr::IsNotNull(e) => expr_has_subquery(e),
        _ => false,
    }
}

/// Whether a `sqlparser` expression contains an aggregate function call.
fn expr_has_aggregate(e: &SqlExpr) -> bool {
    match e {
        SqlExpr::Function(f) => agg_func_from_name(&f.name.to_string()).is_some(),
        SqlExpr::BinaryOp { left, right, .. } => {
            expr_has_aggregate(left) || expr_has_aggregate(right)
        }
        SqlExpr::UnaryOp { expr, .. } | SqlExpr::Nested(expr) => expr_has_aggregate(expr),
        SqlExpr::IsNull(e) | SqlExpr::IsNotNull(e) => expr_has_aggregate(e),
        _ => false,
    }
}

fn convert_order_by(order_by: Option<ast::OrderBy>) -> Result<Vec<OrderKey>> {
    let Some(ob) = order_by else {
        return Ok(Vec::new());
    };
    let exprs = match ob.kind {
        ast::OrderByKind::Expressions(v) => v,
        ast::OrderByKind::All(_) => {
            return Err(DbError::SqlUnsupported(
                "ORDER BY ALL is not supported".into(),
            ))
        }
    };
    exprs
        .iter()
        .map(|e| {
            Ok(OrderKey {
                expr: convert_qexpr(&e.expr)?,
                asc: e.options.asc.unwrap_or(true),
            })
        })
        .collect()
}

fn convert_limit(limit_clause: Option<ast::LimitClause>) -> Result<(Option<usize>, usize)> {
    match limit_clause {
        None => Ok((None, 0)),
        Some(ast::LimitClause::LimitOffset {
            limit,
            offset,
            limit_by,
        }) => {
            if !limit_by.is_empty() {
                return Err(DbError::SqlUnsupported(
                    "LIMIT ... BY is not supported".into(),
                ));
            }
            let limit = limit.as_ref().map(expr_to_usize).transpose()?;
            let offset = offset
                .as_ref()
                .map(|o| expr_to_usize(&o.value))
                .transpose()?
                .unwrap_or(0);
            Ok((limit, offset))
        }
        Some(ast::LimitClause::OffsetCommaLimit { offset, limit }) => {
            Ok((Some(expr_to_usize(&limit)?), expr_to_usize(&offset)?))
        }
    }
}

fn expr_to_usize(e: &SqlExpr) -> Result<usize> {
    match e {
        SqlExpr::Value(vws) => match &vws.value {
            Value::Number(s, _) => s.parse::<usize>().map_err(|_| {
                DbError::SqlUnsupported(format!("expected a non-negative integer: {s}"))
            }),
            other => Err(DbError::SqlUnsupported(format!(
                "LIMIT/OFFSET must be an integer, got {other:?}"
            ))),
        },
        other => Err(DbError::SqlUnsupported(format!(
            "LIMIT/OFFSET must be an integer literal, got {other:?}"
        ))),
    }
}

/// Fold the FROM list into a left-deep [`FromNode`] tree. Comma-separated FROM
/// items become `CROSS JOIN`s; each item's own `joins` are folded left-deep.
fn convert_from(items: &[TableWithJoins]) -> Result<FromNode> {
    let mut iter = items.iter();
    let first = iter
        .next()
        .ok_or_else(|| DbError::SqlUnsupported("SELECT without FROM is not supported".into()))?;
    let mut node = convert_table_with_joins(first)?;
    for twj in iter {
        let right = convert_table_with_joins(twj)?;
        node = FromNode::Join {
            left: Box::new(node),
            right: Box::new(right),
            join_type: JoinType::Cross,
            on: None,
            using: Vec::new(),
        };
    }
    Ok(node)
}

fn convert_table_with_joins(twj: &TableWithJoins) -> Result<FromNode> {
    let mut node = FromNode::Table(table_ref_from_factor(&twj.relation)?);
    for join in &twj.joins {
        let right = FromNode::Table(table_ref_from_factor(&join.relation)?);
        let (join_type, on, using) = convert_join_operator(&join.join_operator)?;
        node = FromNode::Join {
            left: Box::new(node),
            right: Box::new(right),
            join_type,
            on,
            using,
        };
    }
    Ok(node)
}

fn table_ref_from_factor(rel: &TableFactor) -> Result<TableRef> {
    match rel {
        TableFactor::Table { name, alias, .. } => Ok(TableRef {
            table: name.to_string(),
            alias: alias.as_ref().map(|a| a.name.value.clone()),
        }),
        other => Err(DbError::SqlUnsupported(format!(
            "unsupported table reference in join: {other:?}"
        ))),
    }
}

/// Returns `(join_type, ON expr, USING columns)`. `USING (c1, …)` yields the
/// column names (desugared to an equi-`ON` at plan time, see `plan::plan_join`);
/// `ON`/cross joins yield an empty `using`.
fn convert_join_operator(op: &JoinOperator) -> Result<(JoinType, Option<QExpr>, Vec<String>)> {
    let (ty, constraint) = match op {
        JoinOperator::Inner(c) | JoinOperator::Join(c) => (JoinType::Inner, Some(c)),
        JoinOperator::LeftOuter(c) | JoinOperator::Left(c) => (JoinType::Left, Some(c)),
        JoinOperator::RightOuter(c) | JoinOperator::Right(c) => (JoinType::Right, Some(c)),
        JoinOperator::CrossJoin(_) => (JoinType::Cross, None),
        other => {
            return Err(DbError::SqlUnsupported(format!(
                "unsupported join type: {other:?} (FULL OUTER / NATURAL arrive later)"
            )))
        }
    };
    let (on, using) = match constraint {
        None => (None, Vec::new()),
        Some(JoinConstraint::On(expr)) => (Some(convert_qexpr(expr)?), Vec::new()),
        Some(JoinConstraint::Using(cols)) => {
            // `USING (a, b)` — each entry is a bare column name.
            let names = cols.iter().map(|c| c.to_string()).collect::<Vec<_>>();
            (None, names)
        }
        Some(JoinConstraint::None) => (None, Vec::new()),
        Some(other) => {
            return Err(DbError::SqlUnsupported(format!(
                "unsupported join constraint: {other:?} (only ON <expr> / USING (cols))"
            )))
        }
    };
    Ok((ty, on, using))
}

fn convert_query_projection(items: Vec<SelectItem>) -> Result<Vec<Projection>> {
    let mut out = Vec::new();
    for item in items {
        match item {
            SelectItem::Wildcard(_) => out.push(Projection::Wildcard),
            SelectItem::QualifiedWildcard(kind, _) => {
                let name = match kind {
                    ast::SelectItemQualifiedWildcardKind::ObjectName(n) => n.to_string(),
                    other => {
                        return Err(DbError::SqlUnsupported(format!(
                            "unsupported qualified wildcard: {other:?}"
                        )))
                    }
                };
                out.push(Projection::QualifiedWildcard(name));
            }
            SelectItem::UnnamedExpr(e) => out.push(Projection::Expr {
                expr: convert_qexpr(&e)?,
                alias: None,
            }),
            SelectItem::ExprWithAlias { expr, alias } => out.push(Projection::Expr {
                expr: convert_qexpr(&expr)?,
                alias: Some(alias.value),
            }),
            other => {
                return Err(DbError::SqlUnsupported(format!(
                    "unsupported SELECT item: {other:?}"
                )))
            }
        }
    }
    Ok(out)
}

/// Convert a `sqlparser` expression into a [`QExpr`] (the Phase-4 query
/// expression, which keeps column qualifiers and supports `OR`/`NOT`/`IS NULL`).
fn convert_qexpr(e: &SqlExpr) -> Result<QExpr> {
    match e {
        SqlExpr::Identifier(ident) => Ok(QExpr::Column {
            qualifier: None,
            name: ident.value.clone(),
        }),
        SqlExpr::CompoundIdentifier(parts) => {
            let name = parts.last().map(|i| i.value.clone()).unwrap_or_default();
            let qualifier = if parts.len() >= 2 {
                Some(parts[parts.len() - 2].value.clone())
            } else {
                None
            };
            Ok(QExpr::Column { qualifier, name })
        }
        SqlExpr::Value(vws) => convert_value(&vws.value).map(QExpr::Literal),
        SqlExpr::Nested(inner) => convert_qexpr(inner),
        SqlExpr::IsNull(inner) => Ok(QExpr::IsNull {
            expr: Box::new(convert_qexpr(inner)?),
            negated: false,
        }),
        SqlExpr::IsNotNull(inner) => Ok(QExpr::IsNull {
            expr: Box::new(convert_qexpr(inner)?),
            negated: true,
        }),
        SqlExpr::UnaryOp {
            op: UnaryOperator::Not,
            expr,
        } => Ok(QExpr::Not(Box::new(convert_qexpr(expr)?))),
        SqlExpr::BinaryOp { left, op, right } => convert_qbinary_op(left, op, right),
        SqlExpr::Function(func) if func.name.to_string().eq_ignore_ascii_case("match") => {
            convert_match_qexpr(func)
        }
        SqlExpr::Function(f) => convert_aggregate(f),
        SqlExpr::Exists { subquery, negated } => Ok(QExpr::Exists {
            subquery: Box::new(query_to_spec((**subquery).clone())?),
            negated: *negated,
        }),
        SqlExpr::Subquery(q) => Ok(QExpr::ScalarSubquery(Box::new(query_to_spec(
            (**q).clone(),
        )?))),
        SqlExpr::InSubquery {
            expr,
            subquery,
            negated,
        } => Ok(QExpr::InSubquery {
            expr: Box::new(convert_qexpr(expr)?),
            subquery: Box::new(query_to_spec((**subquery).clone())?),
            negated: *negated,
        }),
        SqlExpr::InList {
            expr,
            list,
            negated,
        } => Ok(QExpr::InList {
            expr: Box::new(convert_qexpr(expr)?),
            list: list.iter().map(convert_qexpr).collect::<Result<Vec<_>>>()?,
            negated: *negated,
        }),
        SqlExpr::Like {
            negated,
            any,
            expr,
            pattern,
            ..
        } => {
            if *any {
                return Err(DbError::SqlUnsupported(
                    "LIKE ANY is not supported in v1".into(),
                ));
            }
            Ok(QExpr::Like {
                expr: Box::new(convert_qexpr(expr)?),
                pattern: Box::new(convert_qexpr(pattern)?),
                negated: *negated,
                case_insensitive: false,
            })
        }
        SqlExpr::ILike {
            negated,
            any,
            expr,
            pattern,
            ..
        } => {
            if *any {
                return Err(DbError::SqlUnsupported(
                    "ILIKE ANY is not supported in v1".into(),
                ));
            }
            Ok(QExpr::Like {
                expr: Box::new(convert_qexpr(expr)?),
                pattern: Box::new(convert_qexpr(pattern)?),
                negated: *negated,
                case_insensitive: true,
            })
        }
        other => Err(DbError::SqlUnsupported(format!(
            "unsupported expression in query: {other:?}"
        ))),
    }
}

/// `MATCH(column, query)` in the planner `QExpr` representation (G11, item 30).
fn convert_match_qexpr(func: &ast::Function) -> Result<QExpr> {
    let args = match &func.args {
        ast::FunctionArguments::List(list) => &list.args,
        _ => {
            return Err(DbError::SqlUnsupported(
                "MATCH requires (column, query) arguments".into(),
            ))
        }
    };
    if args.len() != 2 {
        return Err(DbError::SqlUnsupported(
            "MATCH requires exactly 2 arguments: MATCH(column, 'query text')".into(),
        ));
    }
    let column = match &args[0] {
        ast::FunctionArg::Unnamed(ast::FunctionArgExpr::Expr(e)) => convert_qexpr(e)?,
        other => {
            return Err(DbError::SqlUnsupported(format!(
                "MATCH's first argument must be a column name, got {other:?}"
            )))
        }
    };
    let query = match &args[1] {
        ast::FunctionArg::Unnamed(ast::FunctionArgExpr::Expr(e)) => convert_qexpr(e)?,
        other => {
            return Err(DbError::SqlUnsupported(format!(
                "MATCH's second argument must be a query string or $n, got {other:?}"
            )))
        }
    };
    Ok(QExpr::Match {
        column: Box::new(column),
        query: Box::new(query),
    })
}

/// Convert an aggregate function call (`COUNT(*)`, `SUM(x)`, `AVG(DISTINCT x)`,
/// ...) into a [`QExpr::Aggregate`]. Only the five standard aggregates are
/// supported; window/filter clauses are rejected.
fn convert_aggregate(f: &ast::Function) -> Result<QExpr> {
    let func = agg_func_from_name(&f.name.to_string()).ok_or_else(|| {
        DbError::SqlUnsupported(format!("unsupported function in query: {}", f.name))
    })?;
    if f.over.is_some() || f.filter.is_some() || !f.within_group.is_empty() {
        return Err(DbError::SqlUnsupported(
            "window / FILTER / WITHIN GROUP aggregates are not supported".into(),
        ));
    }
    let list = match &f.args {
        ast::FunctionArguments::List(list) => list,
        ast::FunctionArguments::None => {
            return Err(DbError::SqlUnsupported(format!(
                "{} requires arguments",
                f.name
            )))
        }
        other => {
            return Err(DbError::SqlUnsupported(format!(
                "unsupported aggregate arguments: {other:?}"
            )))
        }
    };
    let distinct = matches!(
        list.duplicate_treatment,
        Some(ast::DuplicateTreatment::Distinct)
    );
    // `COUNT(*)` has a single wildcard arg -> no argument expression.
    if list.args.len() == 1 {
        if let ast::FunctionArg::Unnamed(ast::FunctionArgExpr::Wildcard) = &list.args[0] {
            if func != AggFunc::Count {
                return Err(DbError::SqlUnsupported(format!(
                    "{} does not support the * argument",
                    f.name
                )));
            }
            return Ok(QExpr::Aggregate {
                func,
                arg: None,
                distinct,
            });
        }
    }
    if list.args.len() != 1 {
        return Err(DbError::SqlUnsupported(format!(
            "{} takes exactly one argument",
            f.name
        )));
    }
    let arg = match &list.args[0] {
        ast::FunctionArg::Unnamed(ast::FunctionArgExpr::Expr(e)) => convert_qexpr(e)?,
        other => {
            return Err(DbError::SqlUnsupported(format!(
                "unsupported aggregate argument: {other:?}"
            )))
        }
    };
    Ok(QExpr::Aggregate {
        func,
        arg: Some(Box::new(arg)),
        distinct,
    })
}

fn convert_qbinary_op(left: &SqlExpr, op: &BinaryOperator, right: &SqlExpr) -> Result<QExpr> {
    let lhs = Box::new(convert_qexpr(left)?);
    let rhs = Box::new(convert_qexpr(right)?);
    let cmp = |op| {
        Ok(QExpr::Compare {
            op,
            lhs: lhs.clone(),
            rhs: rhs.clone(),
        })
    };
    match op {
        BinaryOperator::And => Ok(QExpr::And(lhs, rhs)),
        BinaryOperator::Or => Ok(QExpr::Or(lhs, rhs)),
        BinaryOperator::Eq => cmp(CmpOp::Eq),
        BinaryOperator::NotEq => cmp(CmpOp::Ne),
        BinaryOperator::Lt => cmp(CmpOp::Lt),
        BinaryOperator::Gt => cmp(CmpOp::Gt),
        BinaryOperator::LtEq => cmp(CmpOp::Le),
        BinaryOperator::GtEq => cmp(CmpOp::Ge),
        other => Err(DbError::SqlUnsupported(format!(
            "unsupported operator in join query: {other}"
        ))),
    }
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
        SqlExpr::Function(func) if func.name.to_string().eq_ignore_ascii_case("match") => {
            convert_match_expr(func)
        }
        SqlExpr::Like {
            negated,
            any,
            expr,
            pattern,
            ..
        } => {
            if *any {
                return Err(DbError::SqlUnsupported(
                    "LIKE ANY is not supported in v1".into(),
                ));
            }
            Ok(Expr::Like {
                expr: Box::new(convert_expr(expr)?),
                pattern: Box::new(convert_expr(pattern)?),
                negated: *negated,
                case_insensitive: false,
            })
        }
        SqlExpr::ILike {
            negated,
            any,
            expr,
            pattern,
            ..
        } => {
            if *any {
                return Err(DbError::SqlUnsupported(
                    "ILIKE ANY is not supported in v1".into(),
                ));
            }
            Ok(Expr::Like {
                expr: Box::new(convert_expr(expr)?),
                pattern: Box::new(convert_expr(pattern)?),
                negated: *negated,
                case_insensitive: true,
            })
        }
        other => Err(DbError::SqlUnsupported(format!(
            "unsupported expression: {other:?}"
        ))),
    }
}

/// `MATCH(column, query)` in the row-path `Expr` representation (G11,
/// item 30). Column must be a bare identifier; query may be a literal or `$n`.
fn convert_match_expr(func: &ast::Function) -> Result<Expr> {
    let args = match &func.args {
        ast::FunctionArguments::List(list) => &list.args,
        _ => {
            return Err(DbError::SqlUnsupported(
                "MATCH requires (column, query) arguments".into(),
            ))
        }
    };
    if args.len() != 2 {
        return Err(DbError::SqlUnsupported(
            "MATCH requires exactly 2 arguments: MATCH(column, 'query text')".into(),
        ));
    }
    let column = match &args[0] {
        ast::FunctionArg::Unnamed(ast::FunctionArgExpr::Expr(SqlExpr::Identifier(ident))) => {
            ident.value.clone()
        }
        other => {
            return Err(DbError::SqlUnsupported(format!(
                "MATCH's first argument must be a column name, got {other:?}"
            )))
        }
    };
    let query = match &args[1] {
        ast::FunctionArg::Unnamed(ast::FunctionArgExpr::Expr(e)) => convert_expr(e)?,
        other => {
            return Err(DbError::SqlUnsupported(format!(
                "MATCH's second argument must be a query string or $n, got {other:?}"
            )))
        }
    };
    Ok(Expr::Match {
        column,
        query: Box::new(query),
    })
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
        // Arithmetic operators — used primarily in UPDATE SET clauses
        // (e.g. `SET k = k + 1`) and valid anywhere `eval_expr` runs.
        BinaryOperator::Plus => Ok(Expr::Arith {
            op: ArithOp::Add,
            lhs,
            rhs,
        }),
        BinaryOperator::Minus => Ok(Expr::Arith {
            op: ArithOp::Sub,
            lhs,
            rhs,
        }),
        BinaryOperator::Multiply => Ok(Expr::Arith {
            op: ArithOp::Mul,
            lhs,
            rhs,
        }),
        BinaryOperator::Divide => Ok(Expr::Arith {
            op: ArithOp::Div,
            lhs,
            rhs,
        }),
        BinaryOperator::Modulo => Ok(Expr::Arith {
            op: ArithOp::Mod,
            lhs,
            rhs,
        }),
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
            LogicalPlan::CreateTable { name, columns, .. } => {
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
    fn parses_decimal_and_numeric_columns() {
        let plan = parse_one("CREATE TABLE t (a DECIMAL(10, 2), b NUMERIC(5), c DECIMAL)");
        match plan {
            LogicalPlan::CreateTable { columns, .. } => {
                assert_eq!(columns[0].ty, ColumnType::Decimal(10, 2));
                assert_eq!(columns[1].ty, ColumnType::Decimal(5, 0));
                assert_eq!(columns[2].ty, ColumnType::Decimal(38, 0));
            }
            _ => panic!("expected CreateTable"),
        }
    }

    #[test]
    fn parses_timestamp_column() {
        let plan = parse_one("CREATE TABLE t (created TIMESTAMP)");
        match plan {
            LogicalPlan::CreateTable { columns, .. } => {
                assert_eq!(columns[0].ty, ColumnType::Timestamp);
            }
            _ => panic!("expected CreateTable"),
        }
    }

    #[test]
    fn parses_bind_placeholders() {
        match parse_one("SELECT * FROM t WHERE id = $1") {
            LogicalPlan::Select { predicate, .. } => match predicate {
                Some(Expr::BinOp { rhs, .. }) => {
                    assert_eq!(*rhs, Expr::Literal(Literal::Param(1)));
                }
                other => panic!("expected BinOp, got {other:?}"),
            },
            other => panic!("expected Select, got {other:?}"),
        }
        match parse_one("INSERT INTO t (a, b) VALUES ($1, $2)") {
            LogicalPlan::Insert { values, .. } => {
                assert_eq!(values, vec![vec![Literal::Param(1), Literal::Param(2)]]);
            }
            other => panic!("expected Insert, got {other:?}"),
        }
    }

    #[test]
    fn parses_serial_and_generated_identity() {
        match parse_one(
            "CREATE TABLE t (id SERIAL, big BIGSERIAL, g INT GENERATED ALWAYS AS IDENTITY)",
        ) {
            LogicalPlan::CreateTable { columns, .. } => {
                assert_eq!(columns[0].ty, ColumnType::Int64);
                assert!(columns[0].constraints.identity);
                assert!(columns[1].constraints.identity);
                assert!(columns[2].constraints.identity);
            }
            other => panic!("expected CreateTable, got {other:?}"),
        }
    }

    #[test]
    fn parses_alter_add_and_drop_column() {
        match parse_one("ALTER TABLE t ADD COLUMN c TEXT DEFAULT 'x'") {
            LogicalPlan::AlterTableAddColumn { table, column } => {
                assert_eq!(table, "t");
                assert_eq!(column.name, "c");
                assert_eq!(column.ty, ColumnType::Text);
                assert_eq!(column.constraints.default, Some(Literal::Text("x".into())));
            }
            other => panic!("expected AlterTableAddColumn, got {other:?}"),
        }
        match parse_one("ALTER TABLE t DROP COLUMN IF EXISTS c") {
            LogicalPlan::AlterTableDropColumn {
                table,
                column,
                if_exists,
            } => {
                assert_eq!(table, "t");
                assert_eq!(column, "c");
                assert!(if_exists);
            }
            other => panic!("expected AlterTableDropColumn, got {other:?}"),
        }
    }

    #[test]
    fn parses_drop_table_and_truncate() {
        match parse_one("DROP TABLE IF EXISTS t") {
            LogicalPlan::DropTable { table, if_exists } => {
                assert_eq!(table, "t");
                assert!(if_exists);
            }
            other => panic!("expected DropTable, got {other:?}"),
        }
        match parse_one("TRUNCATE TABLE t") {
            LogicalPlan::Truncate { table } => assert_eq!(table, "t"),
            other => panic!("expected Truncate, got {other:?}"),
        }
    }

    #[test]
    fn rejects_drop_non_table_object() {
        assert!(parse_sql("DROP SCHEMA s").is_err());
    }

    #[test]
    fn parses_p2b_scalar_columns() {
        let plan = parse_one(
            "CREATE TABLE t (a FLOAT, b DOUBLE PRECISION, c REAL, d UUID, e BYTEA, f DATE, g TIME)",
        );
        match plan {
            LogicalPlan::CreateTable { columns, .. } => {
                assert_eq!(columns[0].ty, ColumnType::Float);
                assert_eq!(columns[1].ty, ColumnType::Float);
                assert_eq!(columns[2].ty, ColumnType::Float);
                assert_eq!(columns[3].ty, ColumnType::Uuid);
                assert_eq!(columns[4].ty, ColumnType::Bytea);
                assert_eq!(columns[5].ty, ColumnType::Date);
                assert_eq!(columns[6].ty, ColumnType::Time);
            }
            _ => panic!("expected CreateTable"),
        }
    }

    #[test]
    fn rejects_decimal_with_bad_precision_or_scale() {
        assert!(parse_sql("CREATE TABLE t (a DECIMAL(50, 2))").is_err());
        assert!(parse_sql("CREATE TABLE t (a DECIMAL(4, 6))").is_err());
    }

    #[test]
    fn parses_decimal_literal_with_scale_as_written() {
        let plan = parse_one("INSERT INTO t (price) VALUES (9.90)");
        match plan {
            LogicalPlan::Insert { values, .. } => {
                assert_eq!(values, vec![vec![Literal::Decimal(990, 2)]]);
            }
            _ => panic!("expected Insert"),
        }
    }

    #[test]
    fn parses_negative_decimal_literal() {
        let plan = parse_one("INSERT INTO t (x) VALUES (-0.05)");
        match plan {
            LogicalPlan::Insert { values, .. } => {
                assert_eq!(values, vec![vec![Literal::Decimal(-5, 2)]]);
            }
            _ => panic!("expected Insert"),
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
