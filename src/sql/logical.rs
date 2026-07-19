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
use crate::error::{DbError, Result};
use crate::sql::query::QuerySpec;

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
    /// A `$n` bind-parameter placeholder (P2.e), 1-based. Produced by the
    /// parser; every occurrence is replaced with the caller-supplied value by
    /// [`bind_params`] *before* the plan reaches the executor — a `Param` never
    /// survives into encoding, comparison, or the wire. This is what makes
    /// parameterized queries injection-proof: the value is always data, never
    /// re-parsed as SQL.
    Param(usize),
    Null,
}

/// Substitute every `$n` placeholder in `plan` with the corresponding value
/// from `params` (1-based: `$1` -> `params[0]`) (P2.e). Errors on an
/// out-of-range index. After this runs, no [`Literal::Param`] remains, so the
/// executor only ever sees concrete values.
pub fn bind_params(plan: &mut LogicalPlan, params: &[Literal]) -> Result<()> {
    match plan {
        LogicalPlan::Insert { values, .. } => {
            for row in values {
                for lit in row {
                    bind_literal(lit, params)?;
                }
            }
        }
        LogicalPlan::Update {
            assignments,
            predicate,
            ..
        } => {
            for (_, expr) in assignments {
                bind_expr(expr, params)?;
            }
            if let Some(expr) = predicate {
                bind_expr(expr, params)?;
            }
        }
        LogicalPlan::Select { predicate, .. } | LogicalPlan::Delete { predicate, .. } => {
            if let Some(expr) = predicate {
                bind_expr(expr, params)?;
            }
        }
        LogicalPlan::Query(spec) => spec.bind_params(params)?,
        LogicalPlan::Explain { spec, .. } => spec.bind_params(params)?,
        // DDL / CREATE INDEX carry no bind parameters.
        LogicalPlan::CreateTable { .. }
        | LogicalPlan::CreateIndex { .. }
        | LogicalPlan::AlterTableAddColumn { .. }
        | LogicalPlan::AlterTableDropColumn { .. }
        | LogicalPlan::DropTable { .. }
        | LogicalPlan::Truncate { .. }
        | LogicalPlan::Analyze { .. } => {}
    }
    Ok(())
}

fn bind_literal(lit: &mut Literal, params: &[Literal]) -> Result<()> {
    if let Literal::Param(n) = *lit {
        *lit = params.get(n - 1).cloned().ok_or_else(|| {
            DbError::SqlPlan(format!(
                "bind parameter ${n} has no value ({} supplied)",
                params.len()
            ))
        })?;
    }
    Ok(())
}

fn bind_expr(expr: &mut Expr, params: &[Literal]) -> Result<()> {
    match expr {
        Expr::Literal(lit) => bind_literal(lit, params),
        Expr::BinOp { lhs, rhs, .. }
        | Expr::And(lhs, rhs)
        | Expr::Or(lhs, rhs)
        | Expr::Arith { lhs, rhs, .. } => {
            bind_expr(lhs, params)?;
            bind_expr(rhs, params)
        }
        Expr::JsonExtract { expr, .. } | Expr::JsonExtractText { expr, .. } => {
            bind_expr(expr, params)
        }
        Expr::Like { expr, pattern, .. } => {
            bind_expr(expr, params)?;
            bind_expr(pattern, params)
        }
        Expr::Match { query, .. } => bind_expr(query, params),
        Expr::IsNull { expr, .. } => bind_expr(expr, params),
        Expr::Column(_) | Expr::ColumnSlot(_) | Expr::Near { .. } | Expr::CurrentUser => Ok(()),
    }
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

/// Arithmetic operator for [`Expr::Arith`]. Used in UPDATE SET clauses
/// (`SET k = k + 1`) and any other expression context that needs integer or
/// floating-point arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Expr {
    Column(String),
    /// Pre-bound column index: replaces `Expr::Column` after the binding pass
    /// (`bind_predicate_columns` in executor.rs, item 59 Fix 2). Never appears
    /// in parsed SQL or stored predicates — only created during execution.
    /// Direct positional access eliminates the per-row linear `String` scan
    /// that `Expr::Column` requires in `eval_expr`.
    ColumnSlot(usize),
    Literal(Literal),
    BinOp {
        op: CmpOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    /// Arithmetic binary expression: `lhs op rhs` where `op` is `+`, `-`,
    /// `*`, `/`, or `%`. Primarily produced for UPDATE SET clauses
    /// (`SET k = k + 1`) and valid anywhere [`eval_expr`] runs. The result
    /// type matches the operands: `Int op Int → Int`, `Float op Float → Float`,
    /// mixed `Int`/`Float` coerce to `Float`.
    Arith {
        op: ArithOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    And(Box<Expr>, Box<Expr>),
    /// Logical OR: `lhs OR rhs`. Used by RLS to combine multiple permissive
    /// policies for the same operation (item-24 Z2: OR semantics). Not produced
    /// by the SQL WHERE-clause parser (which is AND-only), only constructed at
    /// policy-materialization time in `create_policy_inner`.
    Or(Box<Expr>, Box<Expr>),
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
    /// `expr [NOT] [I]LIKE pattern` — SQL-standard pattern matching (G9,
    /// item 30). `%` = any run of characters, `_` = exactly one character.
    /// `case_insensitive = true` for `ILIKE`. NULL on either operand → NULL
    /// (treated as false by the predicate evaluator, same as other NULLs).
    Like {
        expr: Box<Expr>,
        pattern: Box<Expr>,
        negated: bool,
        case_insensitive: bool,
    },
    /// `MATCH(column, 'query text')` — boolean full-text predicate (G11,
    /// item 30). In `exec_select` this routes to the FULLTEXT index
    /// (over-fetch-then-filter, same as `Near`); in `eval_expr` it returns
    /// `true` because the caller already filtered via the index.
    Match {
        column: String,
        query: Box<Expr>,
    },
    /// `expr IS [NOT] NULL` (G10, item 19). Works on both the simple row path
    /// (`eval_expr`) and under `NEAR`/`MATCH` predicate re-check. `negated =
    /// true` for `IS NOT NULL`.
    IsNull {
        expr: Box<Expr>,
        negated: bool,
    },
    /// `current_user()` / `CURRENT_USER` — evaluates to the identity of the
    /// executing user at query time. In RLS policies this lets you write
    /// per-user row isolation without hardcoding values:
    /// `USING (owner = current_user())`. Stored in catalog policies as-is;
    /// substituted with the actual username by `substitute_current_user_in_plan`
    /// in `execute_sql_inner_as` before execution. Falls back to `Literal::Null`
    /// in `eval_expr` (the substitution should always run first; `Null` means
    /// the embedded/superuser path correctly omits RLS when user is `None`).
    CurrentUser,
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
        /// G5 (item 19): `RETURNING col1, col2, …`. `None` → old count-only
        /// result; `Some(cols)` → `ExecResult::Rows` with the inserted rows.
        /// An empty list means `RETURNING *` (all columns).
        returning: Option<Vec<String>>,
    },
    Select {
        table: String,
        /// Empty means `SELECT *`.
        projection: Vec<String>,
        predicate: Option<Expr>,
    },
    /// A multi-relation / advanced query (Phase 4): joins, and in later
    /// checkpoints aggregates, grouping, sort, subqueries, CTEs. The parser
    /// routes here only when the query uses a Phase-4 construct; the trivial
    /// single-table filter/project stays a [`LogicalPlan::Select`] (preserving
    /// the concurrent-read fast path and every pre-P4 test).
    Query(QuerySpec),
    Update {
        table: String,
        assignments: Vec<(String, Expr)>,
        predicate: Option<Expr>,
        /// G5 (item 19): `RETURNING col1, col2, …`.
        returning: Option<Vec<String>>,
    },
    Delete {
        table: String,
        predicate: Option<Expr>,
        /// G5 (item 19): `RETURNING col1, col2, …`.
        returning: Option<Vec<String>>,
    },
    /// `CREATE INDEX ... ON table (column) USING HNSW|FULLTEXT` (M2.c). One
    /// column only in M2 — no composite secondary indexes.
    CreateIndex {
        table: String,
        column: String,
        kind: IndexKind,
    },
    /// `ALTER TABLE t ADD COLUMN c <type> [constraints]` (P2.c).
    AlterTableAddColumn { table: String, column: ColumnDef },
    /// `ALTER TABLE t DROP COLUMN [IF EXISTS] c` (P2.c).
    AlterTableDropColumn {
        table: String,
        column: String,
        if_exists: bool,
    },
    /// `DROP TABLE [IF EXISTS] t` (P2.c).
    DropTable { table: String, if_exists: bool },
    /// `TRUNCATE [TABLE] t` (P2.c).
    Truncate { table: String },
    /// `ANALYZE [TABLE] t` (P4.d): gather + persist optimizer statistics.
    Analyze { table: String },
    /// `EXPLAIN [ANALYZE] <query>` (P4.e): show the chosen plan tree with
    /// estimated rows/cost, and (with ANALYZE) the actual rows + execution time.
    Explain { analyze: bool, spec: QuerySpec },
}

// ─── current_user() substitution (item-24 Z6) ────────────────────────────────

/// Recursively replace every [`Expr::CurrentUser`] leaf in `expr` with
/// `Expr::Literal(Literal::Text(user.to_string()))`. Called by
/// `substitute_current_user_in_plan` before the executor sees the plan,
/// and by `exec_insert` for INSERT policy checks.
pub fn substitute_current_user_in_expr(expr: &mut Expr, user: &str) {
    match expr {
        Expr::CurrentUser => {
            *expr = Expr::Literal(Literal::Text(user.to_string()));
        }
        Expr::BinOp { lhs, rhs, .. } | Expr::Arith { lhs, rhs, .. } => {
            substitute_current_user_in_expr(lhs, user);
            substitute_current_user_in_expr(rhs, user);
        }
        Expr::And(lhs, rhs) | Expr::Or(lhs, rhs) => {
            substitute_current_user_in_expr(lhs, user);
            substitute_current_user_in_expr(rhs, user);
        }
        Expr::JsonExtract { expr, .. } | Expr::JsonExtractText { expr, .. } => {
            substitute_current_user_in_expr(expr, user);
        }
        Expr::Like { expr, pattern, .. } => {
            substitute_current_user_in_expr(expr, user);
            substitute_current_user_in_expr(pattern, user);
        }
        Expr::Match { query, .. } => substitute_current_user_in_expr(query, user),
        Expr::IsNull { expr, .. } => substitute_current_user_in_expr(expr, user),
        // Leaves with no sub-expressions — nothing to recurse into.
        Expr::Column(_) | Expr::ColumnSlot(_) | Expr::Literal(_) | Expr::Near { .. } => {}
    }
}

/// Walk the [`LogicalPlan`] and substitute every [`Expr::CurrentUser`] in
/// predicates with `Literal::Text(user)`. Called twice in
/// `execute_sql_inner_as`: once before `apply_rls` (for user-supplied SQL)
/// and once after (for injected RLS policy expressions that also contain
/// `CurrentUser`).
pub fn substitute_current_user_in_plan(plan: &mut LogicalPlan, user: &str) {
    let sub = |e: &mut Option<Expr>| {
        if let Some(expr) = e {
            substitute_current_user_in_expr(expr, user);
        }
    };
    match plan {
        LogicalPlan::Select { predicate, .. } => sub(predicate),
        LogicalPlan::Update { predicate, .. } => sub(predicate),
        LogicalPlan::Delete { predicate, .. } => sub(predicate),
        // INSERT carries no predicate (its policy is handled inline in
        // exec_insert via ExecCtx::current_user). DDL has no predicates.
        _ => {}
    }
}

/// Whether `expr` contains any [`Expr::CurrentUser`] node at any depth.
/// Used to detect policies that require a user identity to evaluate correctly.
pub fn expr_has_current_user_pub(expr: &Expr) -> bool {
    expr_has_current_user(expr)
}

fn expr_has_current_user(expr: &Expr) -> bool {
    match expr {
        Expr::CurrentUser => true,
        Expr::BinOp { lhs, rhs, .. } | Expr::Arith { lhs, rhs, .. } => {
            expr_has_current_user(lhs) || expr_has_current_user(rhs)
        }
        Expr::And(lhs, rhs) | Expr::Or(lhs, rhs) => {
            expr_has_current_user(lhs) || expr_has_current_user(rhs)
        }
        Expr::JsonExtract { expr, .. } | Expr::JsonExtractText { expr, .. } => {
            expr_has_current_user(expr)
        }
        Expr::Like { expr, pattern, .. } => {
            expr_has_current_user(expr) || expr_has_current_user(pattern)
        }
        Expr::Match { query, .. } => expr_has_current_user(query),
        Expr::IsNull { expr, .. } => expr_has_current_user(expr),
        Expr::Column(_) | Expr::ColumnSlot(_) | Expr::Literal(_) | Expr::Near { .. } => false,
    }
}

/// Like [`apply_rls`] but skips any policy that contains [`Expr::CurrentUser`].
/// Used by the embedded/superuser path (`execute_sql_inner`) which has no user
/// identity to substitute — a `CurrentUser` policy would evaluate to `Null`
/// (false) and silently hide all rows from the superuser, which is wrong.
/// Literal-value policies (no `CurrentUser`) are applied normally, preserving
/// the existing embedded-path RLS behavior.
pub fn apply_rls_skip_current_user(plan: LogicalPlan, catalog: &Catalog) -> LogicalPlan {
    match plan {
        LogicalPlan::Select {
            table,
            projection,
            predicate,
        } => {
            // SELECT context: use rls_policy (SELECT + ALL scoped).
            let policy = select_policy_for_skip_current_user(catalog, &table);
            let predicate = and_policy(predicate, policy);
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
            returning,
        } => {
            // UPDATE context: use update_policy (UPDATE + ALL scoped) — Z2.
            let policy = update_policy_for_skip_current_user(catalog, &table);
            let predicate = and_policy(predicate, policy);
            LogicalPlan::Update {
                table,
                assignments,
                predicate,
                returning,
            }
        }
        LogicalPlan::Delete {
            table,
            predicate,
            returning,
        } => {
            // DELETE context: use delete_policy (DELETE + ALL scoped) — Z2.
            let policy = delete_policy_for_skip_current_user(catalog, &table);
            let predicate = and_policy(predicate, policy);
            LogicalPlan::Delete {
                table,
                predicate,
                returning,
            }
        }
        LogicalPlan::Query(mut spec) => {
            // JOIN/query context is SELECT-context; use rls_policy.
            spec.apply_rls_from(&|table| select_policy_for_skip_current_user(catalog, table));
            LogicalPlan::Query(spec)
        }
        LogicalPlan::Explain { analyze, mut spec } => {
            spec.apply_rls_from(&|table| select_policy_for_skip_current_user(catalog, table));
            LogicalPlan::Explain { analyze, spec }
        }
        other @ (LogicalPlan::CreateTable { .. }
        | LogicalPlan::Insert { .. }
        | LogicalPlan::CreateIndex { .. }
        | LogicalPlan::AlterTableAddColumn { .. }
        | LogicalPlan::AlterTableDropColumn { .. }
        | LogicalPlan::DropTable { .. }
        | LogicalPlan::Truncate { .. }
        | LogicalPlan::Analyze { .. }) => other,
    }
}

fn select_policy_for_skip_current_user(catalog: &Catalog, table: &str) -> Option<Expr> {
    catalog
        .lookup(table)
        .ok()
        .and_then(|t| t.rls_policy.clone())
        .filter(|p| !expr_has_current_user(p))
}

fn update_policy_for_skip_current_user(catalog: &Catalog, table: &str) -> Option<Expr> {
    catalog
        .lookup(table)
        .ok()
        .and_then(|t| t.update_policy.clone())
        .filter(|p| !expr_has_current_user(p))
}

fn delete_policy_for_skip_current_user(catalog: &Catalog, table: &str) -> Option<Expr> {
    catalog
        .lookup(table)
        .ok()
        .and_then(|t| t.delete_policy.clone())
        .filter(|p| !expr_has_current_user(p))
}

/// AND the table's per-operation RLS policy (if any) into the plan's predicate.
/// This is the entire RLS mechanism — everything below the logical-plan layer is
/// unaware RLS exists.
///
/// Item-24 Z2: each plan variant applies its scoped policy field:
/// - SELECT / JOIN → `rls_policy` (SELECT + ALL)
/// - UPDATE        → `update_policy` (UPDATE + ALL)
/// - DELETE        → `delete_policy` (DELETE + ALL)
pub fn apply_rls(plan: LogicalPlan, catalog: &Catalog) -> LogicalPlan {
    match plan {
        LogicalPlan::Select {
            table,
            projection,
            predicate,
        } => {
            // SELECT context: use rls_policy (SELECT + ALL scoped).
            let predicate = and_policy(predicate, select_policy_for(catalog, &table));
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
            returning,
        } => {
            // UPDATE context: use update_policy (UPDATE + ALL scoped) — Z2.
            let predicate = and_policy(predicate, update_policy_for(catalog, &table));
            LogicalPlan::Update {
                table,
                assignments,
                predicate,
                returning,
            }
        }
        LogicalPlan::Delete {
            table,
            predicate,
            returning,
        } => {
            // DELETE context: use delete_policy (DELETE + ALL scoped) — Z2.
            let predicate = and_policy(predicate, delete_policy_for(catalog, &table));
            LogicalPlan::Delete {
                table,
                predicate,
                returning,
            }
        }
        LogicalPlan::Query(mut spec) => {
            // RLS for joins is SELECT-context: AND each base relation's
            // SELECT policy into the query's residual selection, qualified
            // to that relation. The executor never learns RLS exists.
            spec.apply_rls_from(&|table| select_policy_for(catalog, table));
            LogicalPlan::Query(spec)
        }
        LogicalPlan::Explain { analyze, mut spec } => {
            // EXPLAIN shows the RLS-rewritten plan the query would actually run.
            spec.apply_rls_from(&|table| select_policy_for(catalog, table));
            LogicalPlan::Explain { analyze, spec }
        }
        other @ (LogicalPlan::CreateTable { .. }
        | LogicalPlan::Insert { .. }
        | LogicalPlan::CreateIndex { .. }
        | LogicalPlan::AlterTableAddColumn { .. }
        | LogicalPlan::AlterTableDropColumn { .. }
        | LogicalPlan::DropTable { .. }
        | LogicalPlan::Truncate { .. }
        | LogicalPlan::Analyze { .. }) => other,
    }
}

/// SELECT-context policy: `rls_policy` (SELECT + ALL scoped).
fn select_policy_for(catalog: &Catalog, table: &str) -> Option<Expr> {
    catalog
        .lookup(table)
        .ok()
        .and_then(|t| t.rls_policy.clone())
}

/// UPDATE-context policy: `update_policy` (UPDATE + ALL scoped) — Z2.
fn update_policy_for(catalog: &Catalog, table: &str) -> Option<Expr> {
    catalog
        .lookup(table)
        .ok()
        .and_then(|t| t.update_policy.clone())
}

/// DELETE-context policy: `delete_policy` (DELETE + ALL scoped) — Z2.
fn delete_policy_for(catalog: &Catalog, table: &str) -> Option<Expr> {
    catalog
        .lookup(table)
        .ok()
        .and_then(|t| t.delete_policy.clone())
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
                index_root: None,
                unique_index_root: None,
                dropped: false,
                ty: ColumnType::Int64,
                constraints: Default::default(),
            }],
            pages: vec![],
            fsm_meta: None,
            rls_policy: policy,
            insert_policy: None,
            update_policy: None,
            delete_policy: None,
            policies: vec![],
            events_enabled: false,
            serial_next: Default::default(),
            constraints: Default::default(),
            generation: 0,
            row_count: 0,
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
        // Z2: rls_policy applies to SELECT; delete_policy applies to DELETE.
        // Test SELECT path: rls_policy is AND-ed in.
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
        let plan = LogicalPlan::Select {
            table: "t".to_string(),
            projection: vec![],
            predicate: Some(user_pred.clone()),
        };
        let rewritten = apply_rls(plan, &catalog);
        match rewritten {
            LogicalPlan::Select { predicate, .. } => {
                assert_eq!(
                    predicate,
                    Some(Expr::And(
                        Box::new(user_pred.clone()),
                        Box::new(policy.clone())
                    ))
                );
            }
            _ => panic!("expected Select"),
        }
        // Z2: DELETE with no delete_policy → predicate unchanged.
        let del_plan = LogicalPlan::Delete {
            table: "t".to_string(),
            predicate: Some(user_pred.clone()),
            returning: None,
        };
        let del_rewritten = apply_rls(del_plan, &catalog);
        match del_rewritten {
            LogicalPlan::Delete { predicate, .. } => {
                // delete_policy is None — only user_pred remains.
                assert_eq!(predicate, Some(user_pred));
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
    fn bind_params_substitutes_placeholders_in_predicate() {
        let mut plan = LogicalPlan::Select {
            table: "t".to_string(),
            projection: vec![],
            predicate: Some(Expr::BinOp {
                op: CmpOp::Eq,
                lhs: Box::new(Expr::Column("id".to_string())),
                rhs: Box::new(Expr::Literal(Literal::Param(1))),
            }),
        };
        bind_params(&mut plan, &[Literal::Int(7)]).unwrap();
        match plan {
            LogicalPlan::Select { predicate, .. } => match predicate {
                Some(Expr::BinOp { rhs, .. }) => {
                    assert_eq!(*rhs, Expr::Literal(Literal::Int(7)));
                }
                _ => panic!("expected BinOp"),
            },
            _ => panic!("expected Select"),
        }
    }

    #[test]
    fn bind_params_errors_on_missing_value() {
        let mut plan = LogicalPlan::Insert {
            table: "t".to_string(),
            columns: None,
            values: vec![vec![Literal::Param(2)]],
            returning: None,
        };
        assert!(bind_params(&mut plan, &[Literal::Int(1)]).is_err());
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
