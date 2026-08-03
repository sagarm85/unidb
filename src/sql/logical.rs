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

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::catalog::{
    Catalog, ColumnDef, IndexKind, NamedTypeDef, TableConstraints, TriggerEvent, TriggerTiming,
};
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
        LogicalPlan::Insert {
            values,
            on_conflict,
            ..
        } => {
            for row in values {
                for lit in row {
                    bind_literal(lit, params)?;
                }
            }
            // item 150: `$n` placeholders may also appear in the DO UPDATE
            // arm's SET exprs / WHERE (e.g. `... DO UPDATE SET n = $3`).
            if let Some(oc) = on_conflict {
                if let OnConflictAction::DoUpdate {
                    assignments,
                    predicate,
                } = &mut oc.action
                {
                    for (_, expr) in assignments {
                        bind_expr(expr, params)?;
                    }
                    if let Some(expr) = predicate {
                        bind_expr(expr, params)?;
                    }
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
        LogicalPlan::SetOp { left, right, .. } => {
            bind_params(left, params)?;
            bind_params(right, params)?;
        }
        // DDL / CREATE INDEX carry no bind parameters.
        LogicalPlan::CreateTable { .. }
        | LogicalPlan::CreateIndex { .. }
        | LogicalPlan::AlterTableAddColumn { .. }
        | LogicalPlan::AlterTableDropColumn { .. }
        | LogicalPlan::DropTable { .. }
        | LogicalPlan::Truncate { .. }
        | LogicalPlan::Analyze { .. }
        | LogicalPlan::CreateNamedType { .. }
        | LogicalPlan::DropNamedType { .. }
        | LogicalPlan::CreateTrigger { .. }
        | LogicalPlan::DropTrigger { .. } => {}
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
        Expr::Column(_)
        | Expr::ColumnSlot(_)
        | Expr::Near { .. }
        | Expr::CurrentUser
        | Expr::AuthUid
        | Expr::AuthClaim(_)
        | Expr::Excluded(_) => Ok(()),
    }
}

/// Collect every unqualified column name `expr` references (item 112) into
/// `out`, for the single-table `Select`/`Update`/`Delete`/`Insert…RETURNING`
/// plan shapes — every reference is against the plan's one `table`, so no
/// qualifier/attribution logic is needed here (contrast the multi-relation
/// [`crate::sql::query::collect_query_column_reads`], which must resolve
/// qualifiers against a join tree).
///
/// Called on the plan **as parsed from the caller's own SQL** (before RLS
/// injection — see `check_plan_privileges`'s doc comment), so a column
/// referenced only inside an RLS-injected policy predicate is never visited
/// here: the policy-column exemption (item 112 Step 0) falls out of *when*
/// this is called, not from any special-casing inside it.
///
/// `Expr::ColumnSlot` never appears in a plan this function is called on —
/// it is only ever created by `bind_predicate_columns` in the executor,
/// which runs strictly after privilege checking.
pub fn collect_expr_columns(expr: &Expr, out: &mut std::collections::BTreeSet<String>) {
    match expr {
        Expr::Column(name) => {
            out.insert(name.clone());
        }
        Expr::Near { column, .. } => {
            out.insert(column.clone());
        }
        Expr::Match { column, query } => {
            out.insert(column.clone());
            collect_expr_columns(query, out);
        }
        Expr::BinOp { lhs, rhs, .. }
        | Expr::Arith { lhs, rhs, .. }
        | Expr::And(lhs, rhs)
        | Expr::Or(lhs, rhs) => {
            collect_expr_columns(lhs, out);
            collect_expr_columns(rhs, out);
        }
        Expr::JsonExtract { expr, .. } | Expr::JsonExtractText { expr, .. } => {
            collect_expr_columns(expr, out);
        }
        Expr::Like { expr, pattern, .. } => {
            collect_expr_columns(expr, out);
            collect_expr_columns(pattern, out);
        }
        Expr::IsNull { expr, .. } => collect_expr_columns(expr, out),
        // item 150: `EXCLUDED.<col>` is a reference to the *proposed* row,
        // not a read of the existing table row — it must not be conflated
        // with a real column read (grant-checking would otherwise demand a
        // SELECT grant that INSERT's own column-grant check already covers).
        Expr::ColumnSlot(_)
        | Expr::Literal(_)
        | Expr::CurrentUser
        | Expr::AuthUid
        | Expr::AuthClaim(_)
        | Expr::Excluded(_) => {}
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
    /// `auth.uid()` (item 122, Workstream B1) — the authenticated principal's
    /// `sub` claim (a stable subject id, distinct from the display username
    /// `current_user()` resolves to, though both source from the same JWT
    /// `sub` today). Stored in catalog policies as-is; substituted with the
    /// principal's subject by `substitute_auth_context_in_plan`/`_in_expr` at
    /// policy-injection time, exactly like `CurrentUser`. **Fails closed**:
    /// when no subject is available the substitution replaces this with
    /// `Literal::Null` (never `Literal::Bool(true)` — item-110 lesson) and
    /// logs a `tracing::warn!`. Falls back to `Literal::Null` in `eval_expr`
    /// if somehow unsubstituted (should never happen — substitution always
    /// runs first).
    AuthUid,
    /// `auth.jwt() ->> '<claim>'` (item 122, Workstream B2) — the literal text
    /// value of the verified token's `<claim>` (e.g. `tenant_id`), letting
    /// policies key on arbitrary JWT claims: `USING (tenant_id = (auth.jwt()
    /// ->> 'tenant'))`. Claims *only* ever originate from `ExecCtx.auth_claims`,
    /// itself only ever populated from a verified token (never from user SQL) —
    /// see `AuthPrincipal`. Substituted with the claim's text value by
    /// `substitute_auth_context_in_plan`/`_in_expr` at policy-injection time.
    /// **Fails closed**: a missing claim (or no principal at all) substitutes
    /// `Literal::Null` (never `Literal::Bool(true)`) and logs a
    /// `tracing::warn!`. Falls back to `Literal::Null` in `eval_expr` if
    /// somehow unsubstituted.
    AuthClaim(String),
    /// `EXCLUDED.<col>` (item 150): only valid inside an `INSERT ... ON
    /// CONFLICT ... DO UPDATE`'s `SET`/`WHERE` exprs — refers to the value
    /// the proposed (would-have-been-inserted) row carries for `<col>`.
    /// Never appears anywhere else in a stored/parsed plan; `exec_insert`
    /// substitutes it with a `Literal` (the conflicting row's proposed
    /// value) before handing the assignments/predicate to the same
    /// single-row UPDATE machinery `exec_update` uses — see
    /// `docs/backlog/150_upsert_on_conflict.md`.
    Excluded(String),
}

/// `INSERT ... ON CONFLICT [(col)] DO NOTHING | DO UPDATE SET ...` (item
/// 150). Locked v1 grammar: at most one conflict-target column (composite
/// targets and `ON CONSTRAINT <name>` are non-goals — see
/// `docs/backlog/150_upsert_on_conflict.md`); the target is optional for
/// `DoNothing` (then any unique/PK violation on the row is absorbed) and
/// required for `DoUpdate` (validated at parse time, not the executor).
#[derive(Debug, Clone, PartialEq)]
pub struct OnConflict {
    /// The single named conflict-target column, when specified. Must name
    /// the table's PRIMARY KEY or a UNIQUE column — validated by the
    /// executor against the live catalog (`InvalidConflictTarget` on
    /// mismatch), not here (this layer has no catalog access).
    pub target: Option<String>,
    pub action: OnConflictAction,
}

#[derive(Debug, Clone, PartialEq)]
pub enum OnConflictAction {
    /// Conflicting rows are silently skipped; the statement's row count
    /// reflects only rows actually inserted (Postgres semantics).
    DoNothing,
    /// Conflicting rows are updated instead, through the same single-row
    /// UPDATE machinery `exec_update` uses. `assignments`' RHS and
    /// `predicate` may reference `Expr::Excluded(col)` (the proposed row).
    DoUpdate {
        assignments: Vec<(String, Expr)>,
        predicate: Option<Expr>,
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
        /// Item 69: fill-factor reservation (10–100, default 100).
        fill_factor: u8,
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
        /// `ON CONFLICT ...` (item 150). `None` → byte-identical pre-150
        /// behavior (a unique violation always errors).
        on_conflict: Option<OnConflict>,
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
    /// `include_cols` carries `INCLUDE (col, ...)` columns for a covering B-tree
    /// index (item 102-B). Empty for non-covering indexes.
    CreateIndex {
        table: String,
        column: String,
        kind: IndexKind,
        /// INCLUDE columns for covering B-tree index (item 102-B). Empty if
        /// `INCLUDE (...)` was not specified. Only meaningful for BTree indexes.
        #[allow(dead_code)] // used in executor
        include_cols: Vec<String>,
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
    /// `SELECT … UNION [ALL] SELECT …` / `INTERSECT` / `EXCEPT` (G3, item 19).
    /// Concatenates two query results; `all = false` deduplicates the rows.
    SetOp {
        op: SetOpKind,
        /// `ALL` quantifier: when true, keep duplicates (UNION ALL). When false,
        /// deduplicate the combined result (UNION / INTERSECT / EXCEPT distinct).
        all: bool,
        /// Left branch — either a `Query` spec or a nested `SetOp` (for
        /// chained set operations like `A UNION B UNION C`).
        left: Box<LogicalPlan>,
        /// Right branch — same flexibility as `left`.
        right: Box<LogicalPlan>,
    },
    /// `CREATE TYPE name AS ENUM (...)` / `CREATE DOMAIN name AS base [CHECK
    /// (...)]` (item 148). One variant covers both — `def.kind` distinguishes
    /// Enum vs Domain. Name-rule and built-in-shadow validation already
    /// happened at parse time; label/base-type validity is checked by the
    /// executor before persisting.
    CreateNamedType { def: NamedTypeDef },
    /// `DROP TYPE [IF EXISTS] name` / `DROP DOMAIN [IF EXISTS] name` (item
    /// 148). Both keywords resolve to this same variant — enums and domains
    /// share one namespace, so either drops whichever kind `name` actually
    /// is; there is no PostgreSQL-style "DROP TYPE can't drop a domain"
    /// restriction in this engine (a deliberate v1 simplification).
    DropNamedType { name: String, if_exists: bool },
    /// `CREATE TRIGGER <name> {BEFORE|AFTER} {INSERT|UPDATE|DELETE} ON
    /// <table> [FOR EACH ROW] EXECUTE FUNCTION <fn_name>` (item 149).
    /// `FOR EACH ROW` is optional and implied (v1 has no `FOR EACH
    /// STATEMENT`) — parsed and validated, not carried here.
    CreateTrigger {
        name: String,
        table: String,
        timing: TriggerTiming,
        event: TriggerEvent,
        function: String,
    },
    /// `DROP TRIGGER [IF EXISTS] <name> ON <table>` (item 149).
    DropTrigger {
        name: String,
        table: String,
        if_exists: bool,
    },
}

/// Set-operation kind for [`LogicalPlan::SetOp`] (G3, item 19).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetOpKind {
    Union,
    Intersect,
    Except,
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
        // Leaves with no sub-expressions — nothing to recurse into. AuthUid /
        // AuthClaim are also leaves this pass doesn't touch — they are
        // substituted separately by `substitute_auth_context_in_expr` (item 122).
        Expr::Column(_)
        | Expr::ColumnSlot(_)
        | Expr::Literal(_)
        | Expr::Near { .. }
        | Expr::AuthUid
        | Expr::AuthClaim(_)
        | Expr::Excluded(_) => {}
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
        // item 150: an `ON CONFLICT ... DO UPDATE` arm's SET/WHERE is
        // user-authored SQL exactly like a plain UPDATE's — a caller writing
        // `DO UPDATE SET owner = current_user()` needs the same substitution
        // a plain `UPDATE ... SET owner = current_user()` gets, or it would
        // silently fall back to `Literal::Null` in `eval_expr`.
        LogicalPlan::Insert {
            on_conflict: Some(oc),
            ..
        } => {
            if let OnConflictAction::DoUpdate {
                assignments,
                predicate,
            } = &mut oc.action
            {
                for (_, expr) in assignments {
                    substitute_current_user_in_expr(expr, user);
                }
                sub(predicate);
            }
        }
        // Bare INSERT (no ON CONFLICT) carries no predicate (its policy is
        // handled inline in exec_insert via ExecCtx::current_user). DDL has
        // no predicates.
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
        Expr::Column(_)
        | Expr::ColumnSlot(_)
        | Expr::Literal(_)
        | Expr::Near { .. }
        | Expr::AuthUid
        | Expr::AuthClaim(_)
        | Expr::Excluded(_) => false,
    }
}

// ─── auth.uid() / auth.jwt() ->> 'claim' substitution (item 122, B1/B2) ───────

/// Whether `expr` contains an [`Expr::AuthUid`] node at any depth.
fn expr_has_auth_uid(expr: &Expr) -> bool {
    match expr {
        Expr::AuthUid => true,
        Expr::BinOp { lhs, rhs, .. } | Expr::Arith { lhs, rhs, .. } => {
            expr_has_auth_uid(lhs) || expr_has_auth_uid(rhs)
        }
        Expr::And(lhs, rhs) | Expr::Or(lhs, rhs) => {
            expr_has_auth_uid(lhs) || expr_has_auth_uid(rhs)
        }
        Expr::JsonExtract { expr, .. } | Expr::JsonExtractText { expr, .. } => {
            expr_has_auth_uid(expr)
        }
        Expr::Like { expr, pattern, .. } => expr_has_auth_uid(expr) || expr_has_auth_uid(pattern),
        Expr::Match { query, .. } => expr_has_auth_uid(query),
        Expr::IsNull { expr, .. } => expr_has_auth_uid(expr),
        Expr::Column(_)
        | Expr::ColumnSlot(_)
        | Expr::Literal(_)
        | Expr::Near { .. }
        | Expr::CurrentUser
        | Expr::AuthClaim(_)
        | Expr::Excluded(_) => false,
    }
}

/// Whether `expr` contains an [`Expr::AuthClaim`] node at any depth.
fn expr_has_auth_claim(expr: &Expr) -> bool {
    match expr {
        Expr::AuthClaim(_) => true,
        Expr::BinOp { lhs, rhs, .. } | Expr::Arith { lhs, rhs, .. } => {
            expr_has_auth_claim(lhs) || expr_has_auth_claim(rhs)
        }
        Expr::And(lhs, rhs) | Expr::Or(lhs, rhs) => {
            expr_has_auth_claim(lhs) || expr_has_auth_claim(rhs)
        }
        Expr::JsonExtract { expr, .. } | Expr::JsonExtractText { expr, .. } => {
            expr_has_auth_claim(expr)
        }
        Expr::Like { expr, pattern, .. } => {
            expr_has_auth_claim(expr) || expr_has_auth_claim(pattern)
        }
        Expr::Match { query, .. } => expr_has_auth_claim(query),
        Expr::IsNull { expr, .. } => expr_has_auth_claim(expr),
        Expr::Column(_)
        | Expr::ColumnSlot(_)
        | Expr::Literal(_)
        | Expr::Near { .. }
        | Expr::CurrentUser
        | Expr::AuthUid
        | Expr::Excluded(_) => false,
    }
}

/// Whether `expr` requires *some* verified-identity substitution
/// (`current_user()`, `auth.uid()`, or `auth.jwt() ->> '...'`) to evaluate
/// correctly. Used by the embedded/superuser bypass paths
/// (`apply_rls_skip_current_user` and the INSERT-policy check in
/// `exec_insert`) to decide whether a policy containing any of these must be
/// skipped rather than evaluated (an unresolvable identity reference would
/// otherwise fail closed to `Null`/false and silently block the superuser —
/// the same item-103/item-110 reasoning `expr_has_current_user_pub` already
/// documents, now widened to cover B1/B2).
pub fn expr_requires_auth_context_pub(expr: &Expr) -> bool {
    expr_has_current_user(expr) || expr_has_auth_uid(expr) || expr_has_auth_claim(expr)
}

/// Render a JSON claim value as `->>`-style text: a JSON string contributes
/// its bare contents (no surrounding quotes); every other JSON shape
/// (number/bool/object/array/null) is rendered via its compact JSON text —
/// mirrors Postgres's `->>` "always text" contract.
fn claim_value_as_text(v: &JsonValue) -> String {
    match v {
        JsonValue::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Recursively replace every [`Expr::AuthUid`]/[`Expr::AuthClaim`] leaf in
/// `expr` with its resolved literal value — `subject` for `AuthUid`, the
/// matching entry of `claims` (rendered as text) for `AuthClaim`. **Fails
/// closed**: a missing subject or claim substitutes `Expr::Literal(Literal::
/// Null)` — never `Literal::Bool(true)` (the item-110 regression this
/// mirrors) — and logs a `tracing::warn!` so a silently-over-restrictive
/// policy is at least visible in logs. `claims` only ever originates from
/// [`crate::AuthPrincipal`], itself only ever populated from a verified
/// token — never from user-supplied SQL.
pub fn substitute_auth_context_in_expr(
    expr: &mut Expr,
    subject: Option<&str>,
    claims: &BTreeMap<String, JsonValue>,
) {
    match expr {
        Expr::AuthUid => {
            *expr = match subject {
                Some(s) => Expr::Literal(Literal::Text(s.to_string())),
                None => {
                    tracing::warn!(
                        "auth.uid() evaluated with no authenticated subject — \
                         failing closed (NULL), never Bool(true)"
                    );
                    Expr::Literal(Literal::Null)
                }
            };
        }
        Expr::AuthClaim(key) => {
            *expr = match claims.get(key) {
                Some(v) => Expr::Literal(Literal::Text(claim_value_as_text(v))),
                None => {
                    tracing::warn!(
                        claim = %key,
                        "auth.jwt() claim missing from verified token — failing \
                         closed (NULL), never Bool(true)"
                    );
                    Expr::Literal(Literal::Null)
                }
            };
        }
        Expr::BinOp { lhs, rhs, .. } | Expr::Arith { lhs, rhs, .. } => {
            substitute_auth_context_in_expr(lhs, subject, claims);
            substitute_auth_context_in_expr(rhs, subject, claims);
        }
        Expr::And(lhs, rhs) | Expr::Or(lhs, rhs) => {
            substitute_auth_context_in_expr(lhs, subject, claims);
            substitute_auth_context_in_expr(rhs, subject, claims);
        }
        Expr::JsonExtract { expr, .. } | Expr::JsonExtractText { expr, .. } => {
            substitute_auth_context_in_expr(expr, subject, claims);
        }
        Expr::Like { expr, pattern, .. } => {
            substitute_auth_context_in_expr(expr, subject, claims);
            substitute_auth_context_in_expr(pattern, subject, claims);
        }
        Expr::Match { query, .. } => substitute_auth_context_in_expr(query, subject, claims),
        Expr::IsNull { expr, .. } => substitute_auth_context_in_expr(expr, subject, claims),
        // Leaves with no sub-expressions — nothing to recurse into. CurrentUser
        // is a leaf this pass doesn't touch — it is substituted separately by
        // `substitute_current_user_in_expr`.
        Expr::Column(_)
        | Expr::ColumnSlot(_)
        | Expr::Literal(_)
        | Expr::Near { .. }
        | Expr::CurrentUser
        | Expr::Excluded(_) => {}
    }
}

/// Walk the [`LogicalPlan`] and substitute every [`Expr::AuthUid`]/
/// [`Expr::AuthClaim`] in predicates via [`substitute_auth_context_in_expr`].
/// Called unconditionally (regardless of whether `subject`/`claims` are
/// present) — a missing subject/claim must still resolve to a typed `Null`
/// (fail closed), not be left unsubstituted. Called twice in
/// `execute_sql_inner_as_principal`: once before `apply_rls` (for
/// user-supplied SQL) and once after (for injected RLS policy expressions
/// that also reference `auth.uid()`/`auth.jwt()`).
pub fn substitute_auth_context_in_plan(
    plan: &mut LogicalPlan,
    subject: Option<&str>,
    claims: &BTreeMap<String, JsonValue>,
) {
    let sub = |e: &mut Option<Expr>| {
        if let Some(expr) = e {
            substitute_auth_context_in_expr(expr, subject, claims);
        }
    };
    match plan {
        LogicalPlan::Select { predicate, .. } => sub(predicate),
        LogicalPlan::Update { predicate, .. } => sub(predicate),
        LogicalPlan::Delete { predicate, .. } => sub(predicate),
        // item 150: see the identical rationale in
        // `substitute_current_user_in_plan` just above.
        LogicalPlan::Insert {
            on_conflict: Some(oc),
            ..
        } => {
            if let OnConflictAction::DoUpdate {
                assignments,
                predicate,
            } = &mut oc.action
            {
                for (_, expr) in assignments {
                    substitute_auth_context_in_expr(expr, subject, claims);
                }
                sub(predicate);
            }
        }
        // Bare INSERT (no ON CONFLICT) carries no predicate (its policy is
        // handled inline in exec_insert via ExecCtx::auth_claims/
        // current_user). DDL has no predicates.
        _ => {}
    }
}

/// Like [`apply_rls`] but skips any policy that contains [`Expr::CurrentUser`]
/// (or, item 122, `Expr::AuthUid`/`Expr::AuthClaim`). Used by the
/// embedded/superuser path (`execute_sql_inner`) which has no user identity or
/// verified claims to substitute — such a policy would evaluate to `Null`
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
        LogicalPlan::SetOp {
            op,
            all,
            left,
            right,
        } => {
            let left = Box::new(apply_rls_skip_current_user(*left, catalog));
            let right = Box::new(apply_rls_skip_current_user(*right, catalog));
            LogicalPlan::SetOp {
                op,
                all,
                left,
                right,
            }
        }
        other @ (LogicalPlan::CreateTable { .. }
        | LogicalPlan::Insert { .. }
        | LogicalPlan::CreateIndex { .. }
        | LogicalPlan::AlterTableAddColumn { .. }
        | LogicalPlan::AlterTableDropColumn { .. }
        | LogicalPlan::DropTable { .. }
        | LogicalPlan::Truncate { .. }
        | LogicalPlan::Analyze { .. }
        | LogicalPlan::CreateNamedType { .. }
        | LogicalPlan::DropNamedType { .. }
        | LogicalPlan::CreateTrigger { .. }
        | LogicalPlan::DropTrigger { .. }) => other,
    }
}

fn select_policy_for_skip_current_user(catalog: &Catalog, table: &str) -> Option<Expr> {
    catalog
        .lookup(table)
        .ok()
        .and_then(|t| t.rls_policy.clone())
        .filter(|p| !expr_requires_auth_context_pub(p))
}

fn update_policy_for_skip_current_user(catalog: &Catalog, table: &str) -> Option<Expr> {
    catalog
        .lookup(table)
        .ok()
        .and_then(|t| t.update_policy.clone())
        .filter(|p| !expr_requires_auth_context_pub(p))
}

fn delete_policy_for_skip_current_user(catalog: &Catalog, table: &str) -> Option<Expr> {
    catalog
        .lookup(table)
        .ok()
        .and_then(|t| t.delete_policy.clone())
        .filter(|p| !expr_requires_auth_context_pub(p))
}

/// AND the table's per-operation RLS policy (if any) into the plan's predicate.
/// This is the entire RLS mechanism — everything below the logical-plan layer is
/// unaware RLS exists.
///
/// Item-24 Z2: each plan variant applies its scoped policy field:
/// - SELECT / JOIN → `rls_policy` (SELECT + ALL)
/// - UPDATE        → `update_policy` (UPDATE + ALL)
/// - DELETE        → `delete_policy` (DELETE + ALL)
///
/// `user` is the caller's identity (item 110): policy expressions can
/// reference `current_user`, and for `LogicalPlan::Query` (any SELECT with
/// LIMIT/JOIN/GROUP BY/…) the policy `Expr` is eagerly converted to `QExpr`
/// at injection — so `current_user` must be substituted HERE, before that
/// conversion, or it is destroyed (the pre-item-110 bug: the conversion's
/// fallback turned it into `Bool(true)`, making `owner = current_user` into
/// `owner = TRUE` → a coercion error on every RLS+LIMIT query, and a silent
/// policy weakening in shapes where `Bool` type-checks).
///
/// `roles` is the caller's *effective* roles (item 122, B3/B4 — see
/// [`crate::authz::RoleStore::effective_roles`]), used to gate **role-scoped**
/// named policies (`CREATE POLICY … TO <role,...>`): such a policy is only
/// OR-combined into the applicable predicate when `roles` intersects its
/// target-role set (checked by [`role_scoped_policy_for`]). A named policy
/// with no `TO` clause has an empty target-role set and is folded into the
/// unfiltered `rls_policy`/`update_policy`/`delete_policy` catalog field at
/// `CREATE POLICY` time (`Engine::create_policy_inner`) exactly as before B4
/// shipped — **back-compat**: every no-`TO` policy still applies to every
/// caller, unconditionally, regardless of `roles`. An empty `roles` slice
/// (the back-compat shim [`apply_rls`] passes, and what a caller who somehow
/// resolves to no roles would carry) matches no `TO`-scoped policy at all —
/// "no role filtering" here means exactly "only the always-on no-`TO`
/// policies apply," never a widening.
pub fn apply_rls_with_auth(
    plan: LogicalPlan,
    catalog: &Catalog,
    user: Option<&str>,
    claims: &BTreeMap<String, JsonValue>,
    roles: &[String],
) -> LogicalPlan {
    use crate::authz::PolicyOp;
    // Policy fetcher with current_user AND auth.uid()/auth.jwt() resolved at
    // fetch time — BEFORE the Expr→QExpr conversion (the Query/Explain path)
    // destroys them. Item 110 established this for current_user; item 122
    // extends it to the auth context, or a policy like
    // `tenant_id = (auth.jwt() ->> 'tenant')` reaches QExpr unsubstituted and
    // fails closed to NULL → 0 rows on the happy (LIMIT/JOIN/GROUP BY) path.
    let policy_sub = |pol: Option<Expr>| -> Option<Expr> {
        pol.map(|mut e| {
            if let Some(u) = user {
                substitute_current_user_in_expr(&mut e, u);
            }
            // Fail closed on a missing subject/claim (Null), never Bool(true).
            substitute_auth_context_in_expr(&mut e, user, claims);
            e
        })
    };
    // The applicable predicate for `op_matches` is the OR of: (a) the
    // unfiltered merged field (no-`TO` policies, applies to everyone —
    // untouched by B4), and (b) whichever `TO`-scoped named policies match
    // `roles` (item 122, B4). If NEITHER contributes anything AND at least
    // one `TO`-scoped policy exists for this table+operation (it just didn't
    // match `roles`), the result is a hard deny (`Bool(false)`), not "no
    // restriction" — mirroring Postgres RLS semantics ("some permissive
    // policy exists for this command; a caller matching none of them sees
    // nothing"). See `combine_role_gate`'s doc comment for why this matters
    // and why it can't regress a table with zero named policies.
    let select_for = |table: &str| {
        combine_role_gate(
            select_policy_for(catalog, table),
            role_scoped_policy_for(catalog, table, roles, |op| {
                matches!(op, PolicyOp::Select | PolicyOp::All)
            }),
        )
    };
    let update_for = |table: &str| {
        combine_role_gate(
            update_policy_for(catalog, table),
            role_scoped_policy_for(catalog, table, roles, |op| {
                matches!(op, PolicyOp::Update | PolicyOp::All)
            }),
        )
    };
    let delete_for = |table: &str| {
        combine_role_gate(
            delete_policy_for(catalog, table),
            role_scoped_policy_for(catalog, table, roles, |op| {
                matches!(op, PolicyOp::Delete | PolicyOp::All)
            }),
        )
    };
    match plan {
        LogicalPlan::Select {
            table,
            projection,
            predicate,
        } => {
            // SELECT context: use rls_policy (SELECT + ALL scoped).
            let predicate = and_policy(predicate, select_for(&table));
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
            let predicate = and_policy(predicate, update_for(&table));
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
            let predicate = and_policy(predicate, delete_for(&table));
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
            // Item 110: substitute current_user BEFORE the Expr→QExpr
            // conversion inside apply_rls_from destroys it.
            spec.apply_rls_from(&|table| policy_sub(select_for(table)));
            LogicalPlan::Query(spec)
        }
        LogicalPlan::Explain { analyze, mut spec } => {
            // EXPLAIN shows the RLS-rewritten plan the query would actually run.
            spec.apply_rls_from(&|table| policy_sub(select_for(table)));
            LogicalPlan::Explain { analyze, spec }
        }
        LogicalPlan::SetOp {
            op,
            all,
            left,
            right,
        } => {
            // Apply RLS to both branches of a set operation recursively.
            let left = Box::new(apply_rls_with_auth(*left, catalog, user, claims, roles));
            let right = Box::new(apply_rls_with_auth(*right, catalog, user, claims, roles));
            LogicalPlan::SetOp {
                op,
                all,
                left,
                right,
            }
        }
        other @ (LogicalPlan::CreateTable { .. }
        | LogicalPlan::Insert { .. }
        | LogicalPlan::CreateIndex { .. }
        | LogicalPlan::AlterTableAddColumn { .. }
        | LogicalPlan::AlterTableDropColumn { .. }
        | LogicalPlan::DropTable { .. }
        | LogicalPlan::Truncate { .. }
        | LogicalPlan::Analyze { .. }
        | LogicalPlan::CreateNamedType { .. }
        | LogicalPlan::DropNamedType { .. }
        | LogicalPlan::CreateTrigger { .. }
        | LogicalPlan::DropTrigger { .. }) => other,
    }
}

/// Back-compat shim for callers with no verified auth claims and no resolved
/// roles (the embedded/params path and unit tests that don't exercise
/// `auth.jwt()`/role-scoped policies): applies RLS with an empty claim set
/// and an empty role list, so any `auth.jwt()`/`auth.uid()` in a policy fails
/// closed to NULL exactly as it would for an unauthenticated caller, and any
/// `TO`-scoped named policy is skipped (only no-`TO` policies apply) — see
/// [`apply_rls_with_auth`]'s doc comment for the exact empty-`roles`
/// semantics.
pub fn apply_rls(plan: LogicalPlan, catalog: &Catalog, user: Option<&str>) -> LogicalPlan {
    apply_rls_with_auth(plan, catalog, user, &BTreeMap::new(), &[])
}

/// Resolve the fully-substituted SELECT-context RLS predicate for `table`
/// under one caller identity, ready to evaluate directly against a
/// materialized row via [`crate::sql::executor::predicate_matches`]/
/// [`crate::sql::executor::eval_expr`] — item E1 (Workstream E): per-
/// subscriber realtime filtering on `GET /events/subscribe` needs "would this
/// caller's SELECT policy admit this row" without running a query at all
/// (the row already came from the event queue, not a scan).
///
/// This is **not** a second policy engine: it composes exactly the same
/// `select_policy_for`/`role_scoped_policy_for`/`combine_role_gate`/
/// `substitute_current_user_in_expr`/`substitute_auth_context_in_expr`
/// helpers [`apply_rls_with_auth`]'s `select_for`/`policy_sub` closures use
/// for a real `SELECT`'s WHERE-clause injection — just packaged as a
/// standalone entry point, since those closures are private to
/// `apply_rls_with_auth` and there is no `LogicalPlan` here to inject into.
/// The substitution is applied eagerly (mirroring how the `Query`/`Explain`
/// branches above substitute before the `Expr`→`QExpr` conversion) since
/// there is no second pass over a plan to catch it later.
///
/// `None` means "no policy restricts this table's SELECT visibility for this
/// caller" (deliver unconditionally, exactly as an unrestricted `SELECT`
/// would); `Some(expr)` may itself resolve to a hard `Bool(false)` (item 122
/// B4's deny-by-default when `TO`-scoped policies exist but none match the
/// caller's roles) — evaluating it is the caller's job, not this function's.
pub(crate) fn resolve_select_policy_for_subscriber(
    catalog: &Catalog,
    table: &str,
    user: Option<&str>,
    claims: &BTreeMap<String, JsonValue>,
    roles: &[String],
) -> Option<Expr> {
    use crate::authz::PolicyOp;
    let policy = combine_role_gate(
        select_policy_for(catalog, table),
        role_scoped_policy_for(catalog, table, roles, |op| {
            matches!(op, PolicyOp::Select | PolicyOp::All)
        }),
    );
    policy.map(|mut e| {
        if let Some(u) = user {
            substitute_current_user_in_expr(&mut e, u);
        }
        substitute_auth_context_in_expr(&mut e, user, claims);
        e
    })
}

/// OR-merge the USING predicates of every **role-scoped** (`TO`-having)
/// named policy on `table` whose operation matches `op_matches` and whose
/// target-role set intersects `roles` (item 122, B4), returning `(merged
/// predicate, whether at least one TO-scoped policy exists for this op at
/// all)`. The second value is what lets [`combine_role_gate`] distinguish
/// "no TO-scoped policy was ever defined for this operation" (leave
/// unrestricted — back-compat) from "TO-scoped policies exist, but none
/// matched this caller's roles" (deny — see that function's doc comment).
///
/// No-`TO` policies are deliberately excluded from the merge here — they
/// were already folded into the unfiltered `rls_policy`/`update_policy`/
/// `delete_policy` catalog field by `Engine::create_policy_inner`, and are
/// picked up by `select_policy_for`/`update_policy_for`/`delete_policy_for`
/// exactly as they were before B4 shipped. An empty `roles` slice can never
/// match any TO-scoped policy's target set (by construction — see
/// `apply_rls`'s doc comment on the empty-roles back-compat contract), but
/// still needs to report `any_defined` correctly so a table that *does* have
/// TO-scoped policies denies an empty-roles caller rather than silently
/// leaving it unrestricted.
fn role_scoped_policy_for(
    catalog: &Catalog,
    table: &str,
    roles: &[String],
    op_matches: impl Fn(crate::authz::PolicyOp) -> bool,
) -> (Option<Expr>, bool) {
    let Ok(t) = catalog.lookup(table) else {
        return (None, false);
    };
    let mut merged: Option<Expr> = None;
    let mut any_defined = false;
    for p in &t.policies {
        if p.target_roles.is_empty() || !op_matches(p.op) {
            continue;
        }
        any_defined = true;
        if !p.target_roles.iter().any(|r| roles.contains(r)) {
            continue;
        }
        let Ok(expr) = parse_policy_predicate(table, &p.using_expr) else {
            // A stored named policy that fails to re-parse is an impossible
            // state in practice (`CREATE POLICY` validates it up front by
            // parsing it the same way) — fail closed by skipping just this
            // policy rather than trusting on-disk data blindly or panicking
            // in a read path.
            continue;
        };
        merged = Some(match merged {
            Some(old) => Expr::Or(Box::new(old), Box::new(expr)),
            None => expr,
        });
    }
    (merged, any_defined)
}

/// Combine the unfiltered (no-`TO`) merged predicate `base` with the
/// role-gated result of [`role_scoped_policy_for`] (`(scoped, any_defined)`)
/// into the final applicable predicate for one operation (item 122, B4).
///
/// - Either or both present → OR them (a row is visible if it satisfies any
///   applicable permissive policy — Postgres semantics, unchanged from
///   pre-B4).
/// - Neither present, and `any_defined` is `false` (no `TO`-scoped policy
///   was ever created for this table+operation) → `None`, i.e. no
///   restriction at all. This is the **back-compat** case: a table with only
///   no-`TO` policies (or none at all) behaves bit-for-bit as it did before
///   B4 shipped.
/// - Neither present, but `any_defined` is `true` (one or more `TO`-scoped
///   policies exist for this operation, none matched the caller's roles) →
///   `Some(Bool(false))`, a hard deny. This mirrors Postgres: once a
///   permissive policy exists for a command, a caller satisfying none of
///   them sees zero rows, not every row — the deny-by-default is what makes
///   `TO authenticated`/`TO <role>` actually *restrictive* (test case: an
///   `anon`/no-matching-role caller must see 0 rows under a `TO
///   authenticated`-only policy, not all of them).
fn combine_role_gate(base: Option<Expr>, scoped: (Option<Expr>, bool)) -> Option<Expr> {
    let (scoped, any_defined) = scoped;
    match (base, scoped) {
        (Some(x), Some(y)) => Some(Expr::Or(Box::new(x), Box::new(y))),
        (Some(x), None) => Some(x),
        (None, Some(y)) => Some(y),
        (None, None) if any_defined => Some(Expr::Literal(Literal::Bool(false))),
        (None, None) => None,
    }
}

/// Parse a bare predicate string into an [`Expr`] by wrapping it in a dummy
/// `SELECT * FROM <table> WHERE <predicate>` and reusing the ordinary SQL
/// parser — the one-grammar policy language (item-24 Z1, item 122 B4).
/// Shared by `Engine::create_policy_inner`/`drop_policy_inner` (lib.rs) and
/// [`role_scoped_policy_for`] above, so there is exactly one place that
/// re-parses a stored policy's `using_expr` back into an `Expr`.
pub(crate) fn parse_policy_predicate(table: &str, using_expr: &str) -> Result<Expr> {
    let sql = format!("SELECT * FROM {table} WHERE {using_expr}");
    let plans = crate::sql::parser::parse_sql(&sql)?;
    match plans.as_slice() {
        [LogicalPlan::Select {
            predicate: Some(expr),
            ..
        }] => Ok(expr.clone()),
        _ => Err(DbError::SqlUnsupported(
            "a policy USING predicate must be a single AND-only comparison predicate".into(),
        )),
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
                include_cols: Vec::new(),
                type_name: None,
            }],
            pages: vec![],
            fsm_meta: None,
            rls_policy: policy,
            insert_policy: None,
            update_policy: None,
            delete_policy: None,
            update_with_check: None,
            policies: vec![],
            events_enabled: false,
            serial_next: Default::default(),
            constraints: Default::default(),
            generation: 0,
            row_count: 0,
            fill_factor: 100,
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
        let rewritten = apply_rls(plan, &catalog, Some("test_user"));
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
        let rewritten = apply_rls(plan, &catalog, Some("test_user"));
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
        let del_rewritten = apply_rls(del_plan, &catalog, Some("test_user"));
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
        let rewritten = apply_rls(plan, &catalog, Some("test_user"));
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
            on_conflict: None,
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
            fill_factor: 100,
        };
        assert!(matches!(
            apply_rls(plan, &catalog, Some("test_user")),
            LogicalPlan::CreateTable { .. }
        ));
    }

    // ─── item 122 (B1/B2): auth.uid() / auth.jwt() substitution ────────────────

    #[test]
    fn auth_uid_substitutes_subject_when_present() {
        let mut expr = Expr::AuthUid;
        substitute_auth_context_in_expr(&mut expr, Some("alice"), &BTreeMap::new());
        assert_eq!(expr, Expr::Literal(Literal::Text("alice".to_string())));
    }

    /// The literal B1 acceptance requirement: "when subject is absent ->
    /// substitute a typed Null and warn — FAIL CLOSED", never `Bool(true)`.
    #[test]
    fn auth_uid_fails_closed_to_null_when_subject_absent() {
        let mut expr = Expr::AuthUid;
        substitute_auth_context_in_expr(&mut expr, None, &BTreeMap::new());
        assert_eq!(
            expr,
            Expr::Literal(Literal::Null),
            "absent subject must substitute typed Null, never Bool(true)"
        );
    }

    #[test]
    fn auth_claim_substitutes_text_value_when_present() {
        let mut claims = BTreeMap::new();
        claims.insert("tenant".to_string(), JsonValue::String("acme".to_string()));
        let mut expr = Expr::AuthClaim("tenant".to_string());
        substitute_auth_context_in_expr(&mut expr, Some("svc"), &claims);
        assert_eq!(expr, Expr::Literal(Literal::Text("acme".to_string())));
    }

    /// Non-string JSON claim values render as their compact JSON text (the
    /// Postgres `->>` "always text" contract) rather than erroring.
    #[test]
    fn auth_claim_renders_non_string_json_as_text() {
        let mut claims = BTreeMap::new();
        claims.insert("level".to_string(), JsonValue::Number(42.into()));
        let mut expr = Expr::AuthClaim("level".to_string());
        substitute_auth_context_in_expr(&mut expr, Some("svc"), &claims);
        assert_eq!(expr, Expr::Literal(Literal::Text("42".to_string())));
    }

    /// The literal B2 acceptance requirement: a missing claim key fails
    /// CLOSED to typed Null, never `Bool(true)` — even with a present subject
    /// and a non-empty (but non-matching) claim set.
    #[test]
    fn auth_claim_fails_closed_to_null_when_claim_missing() {
        let mut claims = BTreeMap::new();
        claims.insert("other".to_string(), JsonValue::String("x".to_string()));
        let mut expr = Expr::AuthClaim("tenant".to_string());
        substitute_auth_context_in_expr(&mut expr, Some("svc"), &claims);
        assert_eq!(
            expr,
            Expr::Literal(Literal::Null),
            "missing claim must substitute typed Null, never Bool(true)"
        );
    }

    /// No principal at all (subject absent AND claims empty — the pure
    /// embedded path calling the substitution unconditionally) fails closed
    /// for both leaves simultaneously inside one predicate.
    #[test]
    fn auth_context_fails_closed_with_no_principal_at_all() {
        let mut expr = Expr::And(
            Box::new(Expr::BinOp {
                op: CmpOp::Eq,
                lhs: Box::new(Expr::Column("owner".to_string())),
                rhs: Box::new(Expr::AuthUid),
            }),
            Box::new(Expr::BinOp {
                op: CmpOp::Eq,
                lhs: Box::new(Expr::Column("tenant_id".to_string())),
                rhs: Box::new(Expr::AuthClaim("tenant".to_string())),
            }),
        );
        substitute_auth_context_in_expr(&mut expr, None, &BTreeMap::new());
        assert!(
            !expr_has_auth_uid(&expr) && !expr_has_auth_claim(&expr),
            "both leaves must be substituted away"
        );
        let expected = Expr::And(
            Box::new(Expr::BinOp {
                op: CmpOp::Eq,
                lhs: Box::new(Expr::Column("owner".to_string())),
                rhs: Box::new(Expr::Literal(Literal::Null)),
            }),
            Box::new(Expr::BinOp {
                op: CmpOp::Eq,
                lhs: Box::new(Expr::Column("tenant_id".to_string())),
                rhs: Box::new(Expr::Literal(Literal::Null)),
            }),
        );
        assert_eq!(expr, expected, "both must fail closed to Literal::Null");
    }

    #[test]
    fn expr_requires_auth_context_pub_detects_all_three_leaves() {
        assert!(expr_requires_auth_context_pub(&Expr::CurrentUser));
        assert!(expr_requires_auth_context_pub(&Expr::AuthUid));
        assert!(expr_requires_auth_context_pub(&Expr::AuthClaim(
            "x".to_string()
        )));
        assert!(!expr_requires_auth_context_pub(&Expr::Literal(
            Literal::Bool(true)
        )));
    }

    /// `apply_rls_skip_current_user` (the embedded/superuser bypass) must
    /// widen its skip to cover `AuthUid`/`AuthClaim`-referencing policies too
    /// — not just `CurrentUser` — for the same item-103 reasoning: an
    /// unresolvable identity reference must never silently block the
    /// superuser by evaluating to Null.
    #[test]
    fn apply_rls_skip_current_user_also_skips_auth_uid_and_auth_claim_policies() {
        let uid_policy = Expr::BinOp {
            op: CmpOp::Eq,
            lhs: Box::new(Expr::Column("owner".to_string())),
            rhs: Box::new(Expr::AuthUid),
        };
        let catalog = catalog_with_policy("t", Some(uid_policy));
        let plan = LogicalPlan::Select {
            table: "t".to_string(),
            projection: vec![],
            predicate: None,
        };
        let rewritten = apply_rls_skip_current_user(plan, &catalog);
        match rewritten {
            LogicalPlan::Select { predicate, .. } => {
                assert_eq!(
                    predicate, None,
                    "an AuthUid policy must be skipped for the superuser bypass, not injected"
                );
            }
            _ => panic!("expected Select"),
        }

        let claim_policy = Expr::BinOp {
            op: CmpOp::Eq,
            lhs: Box::new(Expr::Column("tenant_id".to_string())),
            rhs: Box::new(Expr::AuthClaim("tenant".to_string())),
        };
        let catalog2 = catalog_with_policy("t2", Some(claim_policy));
        let plan2 = LogicalPlan::Select {
            table: "t2".to_string(),
            projection: vec![],
            predicate: None,
        };
        let rewritten2 = apply_rls_skip_current_user(plan2, &catalog2);
        match rewritten2 {
            LogicalPlan::Select { predicate, .. } => {
                assert_eq!(
                    predicate, None,
                    "an AuthClaim policy must be skipped for the superuser bypass, not injected"
                );
            }
            _ => panic!("expected Select"),
        }
    }

    // ── resolve_select_policy_for_subscriber (item E1) ──────────────────────

    #[test]
    fn resolve_select_policy_for_subscriber_no_policy_is_unrestricted() {
        let catalog = catalog_with_policy("t", None);
        assert_eq!(
            resolve_select_policy_for_subscriber(
                &catalog,
                "t",
                Some("alice"),
                &BTreeMap::new(),
                &[]
            ),
            None
        );
    }

    #[test]
    fn resolve_select_policy_for_subscriber_substitutes_current_user() {
        let policy = Expr::BinOp {
            op: CmpOp::Eq,
            lhs: Box::new(Expr::Column("owner".to_string())),
            rhs: Box::new(Expr::CurrentUser),
        };
        let catalog = catalog_with_policy("t", Some(policy));
        let resolved = resolve_select_policy_for_subscriber(
            &catalog,
            "t",
            Some("alice"),
            &BTreeMap::new(),
            &[],
        )
        .expect("policy expected");
        assert_eq!(
            resolved,
            Expr::BinOp {
                op: CmpOp::Eq,
                lhs: Box::new(Expr::Column("owner".to_string())),
                rhs: Box::new(Expr::Literal(Literal::Text("alice".to_string()))),
            }
        );
    }

    #[test]
    fn resolve_select_policy_for_subscriber_substitutes_auth_jwt_claim() {
        let policy = Expr::BinOp {
            op: CmpOp::Eq,
            lhs: Box::new(Expr::Column("tenant_id".to_string())),
            rhs: Box::new(Expr::AuthClaim("tenant".to_string())),
        };
        let catalog = catalog_with_policy("t", Some(policy));
        let mut claims = BTreeMap::new();
        claims.insert("tenant".to_string(), serde_json::json!("acme"));
        let resolved =
            resolve_select_policy_for_subscriber(&catalog, "t", Some("alice"), &claims, &[])
                .expect("policy expected");
        assert_eq!(
            resolved,
            Expr::BinOp {
                op: CmpOp::Eq,
                lhs: Box::new(Expr::Column("tenant_id".to_string())),
                rhs: Box::new(Expr::Literal(Literal::Text("acme".to_string()))),
            }
        );
    }

    #[test]
    fn resolve_select_policy_for_subscriber_missing_claim_fails_closed_to_null() {
        let policy = Expr::BinOp {
            op: CmpOp::Eq,
            lhs: Box::new(Expr::Column("tenant_id".to_string())),
            rhs: Box::new(Expr::AuthClaim("tenant".to_string())),
        };
        let catalog = catalog_with_policy("t", Some(policy));
        // No "tenant" claim present — must resolve to a typed Null, not `true`.
        let resolved = resolve_select_policy_for_subscriber(
            &catalog,
            "t",
            Some("alice"),
            &BTreeMap::new(),
            &[],
        )
        .expect("policy expected");
        assert_eq!(
            resolved,
            Expr::BinOp {
                op: CmpOp::Eq,
                lhs: Box::new(Expr::Column("tenant_id".to_string())),
                rhs: Box::new(Expr::Literal(Literal::Null)),
            }
        );
    }
}
