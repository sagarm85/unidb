//! Item 123 (Workstream C1): a PostgREST-style auto REST API —
//! `/rest/v1/<table>?col=eq.val&select=...&order=...` — derived from the
//! catalog, so a client gets resource-oriented CRUD without composing SQL.
//!
//! **The single design rule that makes this safe:** every request is
//! translated into a **parameterized SQL statement** and executed through
//! [`crate::Engine::execute_sql_params_as_principal`] — the exact same
//! enforced path `POST /sql` uses (`authorize_sql_as_principal` for the
//! table+column grant pre-check, then bind + RLS + execute under the
//! caller's [`crate::AuthPrincipal`]). This module never runs an unenforced
//! executor path and never string-concatenates request data into SQL text:
//!
//! - **Values are always `$n` bind parameters.** A filter/assignment value
//!   is pushed onto a `Vec<Literal>` and only ever appears in the generated
//!   SQL text as `$1`, `$2`, ... — see [`render_filter`]/[`build_assignments`]/
//!   [`build_insert`]. The engine's own implicit `Literal::Text` ↔
//!   column-type coercion (item 38, `sql/executor.rs::compare`) means a bound
//!   raw query-string value compares correctly against any column type
//!   without this layer needing to parse UUIDs/dates/decimals itself.
//! - **Identifiers (table + column names) are always validated against the
//!   catalog** ([`lookup_table`]/[`validate_column`]) before they are
//!   interpolated into the generated SQL text — an unknown table/column is
//!   rejected (404) before any query is built, let alone run. Column names
//!   are additionally quoted ([`quote_ident`]); table names are not — see
//!   [`table_ident`]'s doc comment for the parser quirk that makes quoting
//!   them actively wrong, and why catalog validation alone is what actually
//!   carries the safety property either way.
//! - **Operators come from a fixed allow-list** ([`ParsedOp`]/[`parse_op`]) —
//!   a filter's `<op>` segment is matched against exactly
//!   `eq,neq,gt,gte,lt,lte,like,ilike,in,is`; anything else is a 400.
//!
//! RLS and table/column grants are therefore inherited from the engine's
//! existing enforcement, not re-implemented here — this module is only a
//! plan/SQL-text builder + result shaper, mirroring the boundary the backlog
//! doc (`docs/backlog/123_auto_rest_api.md`) draws.
//!
//! **`IN` on `UPDATE`/`DELETE`:** this engine's simple (non-`Query`) WHERE
//! grammar — which is what `UPDATE`/`DELETE` always use, see
//! `sql/parser.rs::convert_update`/`convert_delete` — has no `IN`/`OR`
//! support (only the `SELECT`-only `Query`/`QuerySpec` path promotes to that
//! on an `IN`-list). Rather than grow the engine's SQL grammar for this
//! server-layer feature, an `in.(...)` filter on `PATCH`/`DELETE` is expanded
//! into one statement per value (`col = $n` each), all executed inside the
//! same one-shot transaction and their affected-row counts summed — see
//! [`extract_single_in`]/[`run_stmts`]. `GET` uses the real `IN (...)` SQL
//! form instead (promotes to the `Query` path, which supports it natively).
//! At most one `in.(...)` filter is accepted per `PATCH`/`DELETE` request
//! (more would require a combinatorial cross-product of per-value
//! statements); a second one is a 400.
//!
//! **C2 — embedded resource expansion** (`GET` only): `select=id,total,
//! customer(id,name)` nests the related row(s) into each base row, e.g.
//! `{"id":1,"total":10,"customer":{"id":7,"name":"acme"}}`. The relationship
//! is resolved purely from catalog FK metadata
//! ([`resolve_relation`]) — never hand-written per-table logic:
//! - **Forward (many-to-one):** the embed name matches one of the base
//!   table's own FK columns — either the referenced table's name, the FK
//!   column's own name, or the FK column with a trailing `_id` stripped
//!   (`customer_id` -> `customer`, [`strip_id_suffix`]). Embeds a single
//!   object, or `null` when the FK value is `NULL` or the referenced row is
//!   not visible to the caller (RLS/grants — see below).
//! - **Reverse (one-to-many):** the embed name matches some other table's
//!   name, where that table carries an FK column targeting the base table.
//!   Embeds an array (possibly empty).
//! - **Ambiguity is a 400**, not a silent first-match: a name that matches
//!   more than one FK relationship (e.g. two FK columns on the same table
//!   both referencing the target) is rejected as `AMBIGUOUS_RELATIONSHIP`,
//!   matching this module's existing posture of preferring an explicit error
//!   over guessing (mirrors [`extract_single_in`]'s multi-`in.()` rejection).
//! - **Enforcement:** the embedded table's data is fetched via a *second*
//!   parameterized query run through the exact same [`run_stmt`] path as the
//!   base query — same `authorize_sql_as_principal` table/column-grant
//!   pre-check, same RLS-applying `execute_sql_params_as_principal`, same
//!   caller [`AuthPrincipal`]. A restricted caller who can `SELECT` the base
//!   row but not (all of) the embedded table simply gets `null`/`[]`/a
//!   grant-denied error exactly as a direct `GET` on the embedded table
//!   would — this module never reaches past the enforcement layer to fetch
//!   a row the caller couldn't otherwise read. Stitching (join-key value ->
//!   embedded JSON) happens entirely in Rust after both enforced queries
//!   return; no unenforced executor path is ever used.
//! - Composite (multi-column) FKs are out of scope for v1 embedding (single-
//!   column FKs only, both column-level `REFERENCES` and single-column
//!   table-level `FOREIGN KEY`) — a modest, catalog-derived addition rather
//!   than a general multi-column join planner.

use std::collections::{HashMap, HashSet};

use axum::{
    body::Bytes,
    extract::{Path, RawQuery, State},
    Extension, Json,
};
use serde_json::{Map as JsonMap, Value as JsonValue};

use crate::{
    catalog::{ColumnType, TableDef},
    error::DbError,
    sql::{executor::ExecResult, logical::Literal},
    AuthPrincipal,
};

use super::{
    dto::{self, exec_result_to_json, is_internal_table, json_to_literal},
    error::ApiError,
    handlers::finish,
    AppState,
};

const RESERVED_PARAMS: [&str; 4] = ["select", "order", "limit", "offset"];

// ── catalog lookups (identifier validation) ─────────────────────────────────

/// Resolve `table` against the catalog, rejecting unknown or internal
/// (`__events__`/etc.) tables as `404 TABLE_NOT_FOUND` — the same status
/// `POST /sql` gets for `SELECT * FROM nonexistent`. Never used to build SQL
/// itself: only the already-validated [`TableDef::name`] and its columns are
/// interpolated (always quoted) from here on.
pub(super) async fn lookup_table(state: &AppState, table: &str) -> Result<TableDef, ApiError> {
    if is_internal_table(table) {
        return Err(DbError::TableNotFound(table.to_string()).into());
    }
    let defs = state.engine.table_defs().await?;
    defs.into_iter()
        .find(|d| d.name == table)
        .ok_or_else(|| DbError::TableNotFound(table.to_string()).into())
}

/// Validate that `column` exists (and isn't dropped) on `def` — `404
/// COLUMN_NOT_FOUND` otherwise. Every column name this module ever formats
/// into SQL text passes through here (or through `lookup_table` for the
/// table name) first.
pub(super) fn validate_column(def: &TableDef, column: &str) -> Result<(), ApiError> {
    if def.columns.iter().any(|c| c.name == column && !c.dropped) {
        Ok(())
    } else {
        Err(DbError::ColumnNotFound {
            table: def.name.clone(),
            column: column.to_string(),
        }
        .into())
    }
}

/// Double-quote a validated **column** identifier for safe interpolation.
/// Every caller has already checked the identifier against the catalog
/// (`validate_column`) — this only guards against a column name that itself
/// contains a `"` (SQL's own escaping: double it). Column references decode
/// a quoted identifier back to its bare `.value` in every path this module's
/// generated SQL travels (`sql/parser.rs::convert_expr`'s
/// `SqlExpr::Identifier(ident) => Expr::Column(ident.value.clone())`, and
/// `convert_projection`/`column_name_from_parts` likewise), so quoting is
/// safe here.
pub(super) fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// The **table** identifier, deliberately **not** quoted — unlike column
/// references, `sql/parser.rs::table_name_from_relation` resolves a table
/// name via `ObjectName::to_string()`, whose `Display` impl re-includes the
/// surrounding quote characters for a quoted identifier (sqlparser's
/// `Ident::fmt`) instead of stripping them the way column identifiers are
/// (`.value`). Quoting the table name here would therefore make the
/// generated SQL's table reference literally `"items"` (11 bytes, quotes
/// included) instead of `items`, which the catalog then fails to resolve —
/// this was caught by this module's own integration tests
/// (`tests/server_rest.rs`), not by inspection. Safety doesn't depend on
/// quoting here anyway: `table` is always [`TableDef::name`] from
/// [`lookup_table`]'s exact catalog match — never raw URL input — so an
/// un-quoted but catalog-validated name carries no injection surface.
pub(super) fn table_ident(name: &str) -> &str {
    name
}

// ── query-string parsing ────────────────────────────────────────────────────

/// Parse a raw query string into ordered `(key, value)` pairs, percent-
/// decoded, **preserving repeated keys** (`age=gte.18&age=lte.30`) — which is
/// exactly why this doesn't use axum's `Query<HashMap<_, _>>` (a `HashMap`
/// would silently drop one of the two `age` entries).
fn parse_query_pairs(raw: Option<&str>) -> Vec<(String, String)> {
    match raw {
        Some(s) if !s.is_empty() => form_urlencoded::parse(s.as_bytes()).into_owned().collect(),
        _ => Vec::new(),
    }
}

/// `order=col.asc,col2.desc` (or repeated `order=` keys, each itself
/// comma-separated) -> `[(col, desc)]`. No suffix defaults to ascending,
/// matching PostgREST.
pub(super) fn parse_order_value(raw: &str) -> Vec<(String, bool)> {
    raw.split(',')
        .filter(|s| !s.is_empty())
        .map(|part| {
            let lower = part.to_ascii_lowercase();
            if let Some(stripped) = lower.strip_suffix(".asc") {
                (part[..stripped.len()].to_string(), false)
            } else if let Some(stripped) = lower.strip_suffix(".desc") {
                (part[..stripped.len()].to_string(), true)
            } else {
                (part.to_string(), false)
            }
        })
        .collect()
}

/// Parse `limit`/`offset` as a plain non-negative integer. `LIMIT`/`OFFSET`
/// cannot be a `$n` bind in this engine's grammar (`sql/parser.rs::
/// expr_to_usize` requires an integer literal) — the injection-safety
/// argument here is that `str::parse::<i64>` only ever accepts an optional
/// sign followed by ASCII digits, so a value that isn't a clean integer
/// (e.g. `1) OR (1=1`) is rejected with 400 before it ever reaches SQL text,
/// rather than being formatted in.
fn parse_nonneg_int(raw: &str, field: &'static str) -> Result<i64, ApiError> {
    raw.trim()
        .parse::<i64>()
        .ok()
        .filter(|n| *n >= 0)
        .ok_or_else(|| {
            ApiError::bad_request(
                "INVALID_QUERY_PARAM",
                format!("`{field}` must be a non-negative integer, got {raw:?}"),
            )
        })
}

// ── filter operators (fixed allow-list) ─────────────────────────────────────

#[derive(Clone, Debug)]
pub(super) enum ParsedOp {
    Eq(String),
    Neq(String),
    Gt(String),
    Gte(String),
    Lt(String),
    Lte(String),
    Like(String),
    Ilike(String),
    In(Vec<String>),
    IsNull,
    IsTrue,
    IsFalse,
}

#[derive(Clone, Debug)]
pub(super) struct Filter {
    pub(super) column: String,
    pub(super) op: ParsedOp,
}

fn bad_filter(raw: &str) -> ApiError {
    ApiError::bad_request(
        "INVALID_FILTER",
        format!(
            "unsupported filter '{raw}': expected `<op>.<value>` with op one of \
             eq,neq,gt,gte,lt,lte,like,ilike,in,is"
        ),
    )
}

/// `<op>.<value>` -> a [`ParsedOp`]. `op` is matched against the fixed
/// allow-list only — never used to build SQL directly (each variant maps to
/// one hardcoded SQL fragment in [`render_filter`]).
fn parse_op(raw: &str) -> Result<ParsedOp, ApiError> {
    let (op, rest) = raw.split_once('.').ok_or_else(|| bad_filter(raw))?;
    match op.to_ascii_lowercase().as_str() {
        "eq" => Ok(ParsedOp::Eq(rest.to_string())),
        "neq" => Ok(ParsedOp::Neq(rest.to_string())),
        "gt" => Ok(ParsedOp::Gt(rest.to_string())),
        "gte" => Ok(ParsedOp::Gte(rest.to_string())),
        "lt" => Ok(ParsedOp::Lt(rest.to_string())),
        "lte" => Ok(ParsedOp::Lte(rest.to_string())),
        "like" => Ok(ParsedOp::Like(rest.to_string())),
        "ilike" => Ok(ParsedOp::Ilike(rest.to_string())),
        "in" => parse_in(rest),
        "is" => parse_is(rest),
        _ => Err(bad_filter(raw)),
    }
}

fn parse_in(rest: &str) -> Result<ParsedOp, ApiError> {
    let inner = rest
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))
        .ok_or_else(|| {
            ApiError::bad_request(
                "INVALID_FILTER",
                "`in` filter requires a parenthesized list, e.g. in.(1,2,3)",
            )
        })?;
    let values: Vec<String> = inner
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if values.is_empty() {
        return Err(ApiError::bad_request(
            "INVALID_FILTER",
            "`in` filter list must not be empty",
        ));
    }
    Ok(ParsedOp::In(values))
}

fn parse_is(rest: &str) -> Result<ParsedOp, ApiError> {
    match rest.to_ascii_lowercase().as_str() {
        "null" => Ok(ParsedOp::IsNull),
        "true" => Ok(ParsedOp::IsTrue),
        "false" => Ok(ParsedOp::IsFalse),
        other => Err(ApiError::bad_request(
            "INVALID_FILTER",
            format!("`is` filter accepts null/true/false, got '{other}'"),
        )),
    }
}

/// Every non-reserved query param is a filter: validate its column against
/// the catalog, then parse its `<op>.<value>`.
fn parse_filters(def: &TableDef, pairs: &[(String, String)]) -> Result<Vec<Filter>, ApiError> {
    let mut out = Vec::with_capacity(pairs.len());
    for (column, raw_op) in pairs {
        if RESERVED_PARAMS.contains(&column.as_str()) {
            continue;
        }
        validate_column(def, column)?;
        out.push(Filter {
            column: column.clone(),
            op: parse_op(raw_op)?,
        });
    }
    Ok(out)
}

/// Render one filter's SQL fragment, pushing its value(s) onto `binds` as
/// `Literal::Text` (or `Literal::Bool` for `is.true`/`is.false`) — **always**
/// data, never text interpolated into `sql`. `col_sql` is already a quoted,
/// catalog-validated identifier (never raw user input).
fn render_filter(binds: &mut Vec<Literal>, col_sql: &str, op: &ParsedOp) -> String {
    let bind = |lit: Literal, binds: &mut Vec<Literal>| -> String {
        binds.push(lit);
        format!("${}", binds.len())
    };
    match op {
        ParsedOp::Eq(v) => format!("{col_sql} = {}", bind(Literal::Text(v.clone()), binds)),
        ParsedOp::Neq(v) => format!("{col_sql} <> {}", bind(Literal::Text(v.clone()), binds)),
        ParsedOp::Gt(v) => format!("{col_sql} > {}", bind(Literal::Text(v.clone()), binds)),
        ParsedOp::Gte(v) => format!("{col_sql} >= {}", bind(Literal::Text(v.clone()), binds)),
        ParsedOp::Lt(v) => format!("{col_sql} < {}", bind(Literal::Text(v.clone()), binds)),
        ParsedOp::Lte(v) => format!("{col_sql} <= {}", bind(Literal::Text(v.clone()), binds)),
        ParsedOp::Like(v) => format!("{col_sql} LIKE {}", bind(Literal::Text(v.clone()), binds)),
        ParsedOp::Ilike(v) => format!("{col_sql} ILIKE {}", bind(Literal::Text(v.clone()), binds)),
        ParsedOp::IsNull => format!("{col_sql} IS NULL"),
        ParsedOp::IsTrue => format!("{col_sql} = {}", bind(Literal::Bool(true), binds)),
        ParsedOp::IsFalse => format!("{col_sql} = {}", bind(Literal::Bool(false), binds)),
        ParsedOp::In(values) => {
            let placeholders: Vec<String> = values
                .iter()
                .map(|v| bind(Literal::Text(v.clone()), binds))
                .collect();
            format!("{col_sql} IN ({})", placeholders.join(", "))
        }
    }
}

/// AND every filter's fragment together (`None` when `filters` is empty —
/// no `WHERE` clause at all).
pub(super) fn append_where(filters: &[Filter], binds: &mut Vec<Literal>) -> Option<String> {
    if filters.is_empty() {
        return None;
    }
    let clauses: Vec<String> = filters
        .iter()
        .map(|f| render_filter(binds, &quote_ident(&f.column), &f.op))
        .collect();
    Some(clauses.join(" AND "))
}

/// Pull at most one `in.(...)` filter out of `filters` (see this module's
/// doc comment for why `UPDATE`/`DELETE` need it expanded rather than
/// rendered natively); a second one is rejected rather than silently taking
/// only the first, since the combinatorial expansion isn't implemented.
/// A single `in.(...)` filter pulled off a filter list: its column name and
/// the parenthesized value list.
pub(super) type InFilter = (String, Vec<String>);

pub(super) fn extract_single_in(
    filters: Vec<Filter>,
) -> Result<(Vec<Filter>, Option<InFilter>), ApiError> {
    let mut scalar = Vec::with_capacity(filters.len());
    let mut found: Option<(String, Vec<String>)> = None;
    for f in filters {
        if let ParsedOp::In(values) = f.op {
            if found.is_some() {
                return Err(ApiError::bad_request(
                    "MULTIPLE_IN_FILTERS",
                    "at most one `in.(...)` filter is supported per PATCH/DELETE request",
                ));
            }
            found = Some((f.column, values));
        } else {
            scalar.push(f);
        }
    }
    Ok((scalar, found))
}

// ── INSERT / UPDATE value binding (JSON body -> binds) ──────────────────────

/// `col = $n` assignment list for `PATCH`'s JSON-object body. Every value is
/// bound via [`json_to_literal`] — the identical JSON->`Literal` mapping
/// `POST /sql`'s own `params` array uses (`dto::json_to_literal`) — so a
/// string, number, bool, object, or array assignment binds exactly as it
/// would over `/sql`.
///
/// The assignment target is deliberately **not** quoted — like
/// [`table_ident`], `sql/parser.rs::convert_update`'s
/// `AssignmentTarget::ColumnName(name) => name.to_string()` re-includes
/// quote characters instead of stripping them, so a quoted target fails to
/// resolve. `col` is still catalog-validated first, so this carries no
/// injection surface.
pub(super) fn build_assignments(
    def: &TableDef,
    body: &JsonMap<String, JsonValue>,
    binds: &mut Vec<Literal>,
) -> Result<String, ApiError> {
    let mut parts = Vec::with_capacity(body.len());
    for (col, val) in body {
        validate_column(def, col)?;
        binds.push(json_to_literal(val));
        parts.push(format!("{col} = ${}", binds.len()));
    }
    Ok(parts.join(", "))
}

/// Build one (possibly multi-row) `INSERT` statement + its binds from a
/// batch of JSON row objects. Every row must carry the exact same column
/// set (checked below) so a single `VALUES (...), (...), ...` list applies —
/// mirrors the existing `/tables/{name}/bulk` NDJSON loader's "one shape per
/// batch" contract.
fn build_insert(
    def: &TableDef,
    rows: &[JsonMap<String, JsonValue>],
) -> Result<(String, Vec<Literal>), ApiError> {
    let first = &rows[0];
    if first.is_empty() {
        return Err(ApiError::bad_request(
            "EMPTY_ROW",
            "insert row must have at least one column",
        ));
    }
    let columns: Vec<String> = first.keys().cloned().collect();
    for col in &columns {
        validate_column(def, col)?;
    }

    let mut binds: Vec<Literal> = Vec::new();
    let mut value_groups = Vec::with_capacity(rows.len());
    for row in rows {
        if row.len() != columns.len() || !columns.iter().all(|c| row.contains_key(c)) {
            return Err(ApiError::bad_request(
                "INCONSISTENT_COLUMNS",
                "every row in a batch insert must have the same set of columns",
            ));
        }
        let mut placeholders = Vec::with_capacity(columns.len());
        for col in &columns {
            binds.push(json_to_literal(&row[col]));
            placeholders.push(format!("${}", binds.len()));
        }
        value_groups.push(format!("({})", placeholders.join(", ")));
    }

    // Not quoted — same `parser.rs` stringification quirk as `table_ident`
    // (`convert_insert`'s `ins.columns.iter().map(|c| c.to_string())` also
    // re-includes quote characters instead of stripping them). `columns` is
    // already catalog-validated above, so this is still injection-safe.
    let cols_sql = columns.join(", ");
    let sql = format!(
        "INSERT INTO {} ({}) VALUES {}",
        table_ident(&def.name),
        cols_sql,
        value_groups.join(", ")
    );
    Ok((sql, binds))
}

// ── enforced execution (the whole point of this module) ────────────────────

/// Run one statement through the exact same enforced path `POST /sql` uses:
/// a table+column-grant pre-check (`authorize_sql_as_principal`), then
/// begin/execute-under-principal/commit-or-abort
/// (`execute_sql_params_as_principal` + `handlers::finish`). RLS,
/// `current_user()`/`auth.uid()`/`auth.jwt()` substitution, and table/column
/// grants all apply here exactly as they do for `/sql` — nothing in this
/// module re-implements or bypasses any of it.
pub(super) async fn run_stmts(
    state: &AppState,
    principal: &AuthPrincipal,
    stmts: Vec<(String, Vec<Literal>)>,
) -> Result<Vec<ExecResult>, ApiError> {
    for (sql, _) in &stmts {
        state
            .engine
            .authorize_sql_as_principal(principal.clone(), sql.clone())
            .await?;
    }
    let xid = state.engine.begin(None).await?;
    let mut results = Vec::with_capacity(stmts.len());
    let mut first_err = None;
    for (sql, binds) in stmts {
        match state
            .engine
            .execute_sql_params_as_principal(principal.clone(), xid, sql, binds)
            .await
        {
            Ok(mut r) => results.append(&mut r),
            Err(e) => {
                first_err = Some(e);
                break;
            }
        }
    }
    let outcome = match first_err {
        Some(e) => Err(e),
        None => Ok(results),
    };
    Ok(finish(&state.engine, xid, outcome).await?)
}

/// [`run_stmts`] for exactly one statement, unwrapping its single result.
pub(super) async fn run_stmt(
    state: &AppState,
    principal: &AuthPrincipal,
    sql: String,
    binds: Vec<Literal>,
) -> Result<ExecResult, ApiError> {
    run_stmts(state, principal, vec![(sql, binds)])
        .await?
        .pop()
        .ok_or_else(|| ApiError::internal("INTERNAL_ERROR", "statement produced no result"))
}

/// Sum the `count` of a batch of `Updated`/`Deleted` results (the `in.(...)`
/// multi-statement expansion, see this module's doc comment) into one
/// logical result — the client issued one `PATCH`/`DELETE`, so it gets back
/// one count, not N.
fn merge_counts(results: Vec<ExecResult>) -> Result<ExecResult, ApiError> {
    let mut total = 0usize;
    let mut updated = false;
    let mut deleted = false;
    for r in &results {
        match r {
            ExecResult::Updated { count } => {
                total += count;
                updated = true;
            }
            ExecResult::Deleted { count } => {
                total += count;
                deleted = true;
            }
            other => {
                return Err(ApiError::internal(
                    "INTERNAL_ERROR",
                    format!("unexpected result merging counts: {other:?}"),
                ))
            }
        }
    }
    if deleted {
        Ok(ExecResult::Deleted { count: total })
    } else if updated {
        Ok(ExecResult::Updated { count: total })
    } else {
        // No statements ran — `values` is validated non-empty in `parse_in`,
        // so this only happens for a plain (no `in.(...)`) request; the
        // caller always passes at least one statement in that case too, so
        // this arm is unreachable in practice. Fail safe rather than panic.
        Err(ApiError::internal(
            "INTERNAL_ERROR",
            "no statements executed",
        ))
    }
}

// ── C2: embedded resource expansion (`select=...,name(cols)`) ──────────────

/// One parsed `select=` entry: a plain column, or an embedded-resource spec
/// `name(col,col,...)` (`name(*)`/`name()` request every column of the
/// embedded resource).
#[derive(Clone, Debug)]
enum SelectItem {
    Column(String),
    Embed { name: String, columns: Vec<String> },
}

fn bad_select(msg: impl Into<String>) -> ApiError {
    ApiError::bad_request("INVALID_SELECT", msg.into())
}

/// Split a `select=` value on **top-level** commas only — a comma inside an
/// embed's `(...)` doesn't end the outer entry (`select=id,customer(id,name)`
/// is two entries, not three).
fn parse_select_list(raw: &str) -> Result<Vec<SelectItem>, ApiError> {
    let mut items = Vec::new();
    let mut depth = 0i32;
    let mut current = String::new();
    for ch in raw.chars() {
        match ch {
            '(' => {
                depth += 1;
                current.push(ch);
            }
            ')' => {
                depth -= 1;
                if depth < 0 {
                    return Err(bad_select("unbalanced ')' in `select`"));
                }
                current.push(ch);
            }
            ',' if depth == 0 => {
                push_select_item(&mut items, &current)?;
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    if depth != 0 {
        return Err(bad_select("unbalanced '(' in `select`"));
    }
    push_select_item(&mut items, &current)?;
    Ok(items)
}

fn push_select_item(items: &mut Vec<SelectItem>, raw: &str) -> Result<(), ApiError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(());
    }
    match raw.find('(') {
        Some(open) => {
            if !raw.ends_with(')') {
                return Err(bad_select(format!("malformed embed spec '{raw}'")));
            }
            let name = raw[..open].trim().to_string();
            if name.is_empty() {
                return Err(bad_select(format!("malformed embed spec '{raw}'")));
            }
            let inner = raw[open + 1..raw.len() - 1].trim();
            let columns = if inner.is_empty() || inner == "*" {
                Vec::new()
            } else {
                inner
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            };
            items.push(SelectItem::Embed { name, columns });
        }
        None => items.push(SelectItem::Column(raw.to_string())),
    }
    Ok(())
}

/// A resolved embeddable relationship between the base table and a related
/// one, derived purely from catalog FK metadata — see this module's doc
/// comment.
#[derive(Clone, Debug)]
pub(super) enum Relation {
    /// Many-to-one: `base.fk_column` -> `ref_table.ref_column`.
    Forward {
        fk_column: String,
        ref_table: String,
        ref_column: String,
    },
    /// One-to-many: `child_table.fk_column` -> `base.ref_column`.
    Reverse {
        child_table: String,
        fk_column: String,
        ref_column: String,
    },
}

/// Strip a case-insensitive `_id` suffix — the conventional relationship
/// alias for a FK column (`customer_id` -> `customer`). A naming
/// convenience only: the join itself always comes from the catalog FK, this
/// just widens what embed name matches it.
pub(super) fn strip_id_suffix(column: &str) -> Option<&str> {
    if column.len() > 3 && column[column.len() - 3..].eq_ignore_ascii_case("_id") {
        Some(&column[..column.len() - 3])
    } else {
        None
    }
}

/// Resolve a `select=...,<name>(...)` embed `name` against `base`'s catalog
/// FK metadata. Zero matches is `UNKNOWN_RELATIONSHIP` (400); more than one
/// is `AMBIGUOUS_RELATIONSHIP` (400) — e.g. two FK columns on `base` both
/// targeting the same table under the same derived alias, or two FK columns
/// on the same child table both targeting `base`. Composite (multi-column)
/// FKs are skipped (out of scope for v1 embedding).
fn resolve_relation(
    all_defs: &[TableDef],
    base: &TableDef,
    name: &str,
) -> Result<Relation, ApiError> {
    let mut candidates: Vec<Relation> = Vec::new();

    // Forward: base's own FK columns (column-level `REFERENCES` and
    // single-column table-level `FOREIGN KEY`) matched by referenced table
    // name, the FK column's own name, or its `_id`-stripped alias.
    for col in base.columns.iter().filter(|c| !c.dropped) {
        if let Some(fk) = &col.constraints.references {
            let alias = strip_id_suffix(&col.name);
            if fk.table == name || col.name == name || alias == Some(name) {
                if let Some(ref_def) = all_defs.iter().find(|d| d.name == fk.table) {
                    if let Ok(ref_col) =
                        crate::sql::executor::resolve_fk_ref_col(ref_def, fk.column.as_deref())
                    {
                        candidates.push(Relation::Forward {
                            fk_column: col.name.clone(),
                            ref_table: fk.table.clone(),
                            ref_column: ref_col.name.clone(),
                        });
                    }
                }
            }
        }
    }
    for fk in &base.constraints.foreign_keys {
        if fk.columns.len() != 1 {
            continue;
        }
        let col_name = &fk.columns[0];
        let alias = strip_id_suffix(col_name);
        if fk.ref_table == name || col_name == name || alias == Some(name) {
            if let Some(ref_col) = fk.ref_columns.first() {
                candidates.push(Relation::Forward {
                    fk_column: col_name.clone(),
                    ref_table: fk.ref_table.clone(),
                    ref_column: ref_col.clone(),
                });
            }
        }
    }

    // Reverse: every other table whose name matches `name` and that carries
    // a (single-column) FK targeting `base`.
    for child in all_defs.iter().filter(|d| !is_internal_table(&d.name)) {
        if child.name != name {
            continue;
        }
        for col in child.columns.iter().filter(|c| !c.dropped) {
            if let Some(fk) = &col.constraints.references {
                if fk.table == base.name {
                    if let Ok(ref_col) =
                        crate::sql::executor::resolve_fk_ref_col(base, fk.column.as_deref())
                    {
                        candidates.push(Relation::Reverse {
                            child_table: child.name.clone(),
                            fk_column: col.name.clone(),
                            ref_column: ref_col.name.clone(),
                        });
                    }
                }
            }
        }
        for fk in &child.constraints.foreign_keys {
            if fk.columns.len() != 1 || fk.ref_table != base.name {
                continue;
            }
            if let Some(ref_col) = fk.ref_columns.first() {
                candidates.push(Relation::Reverse {
                    child_table: child.name.clone(),
                    fk_column: fk.columns[0].clone(),
                    ref_column: ref_col.clone(),
                });
            }
        }
    }

    match candidates.len() {
        0 => Err(ApiError::bad_request(
            "UNKNOWN_RELATIONSHIP",
            format!(
                "no foreign-key relationship named '{name}' on '{}'",
                base.name
            ),
        )),
        1 => candidates
            .into_iter()
            .next()
            .ok_or_else(|| ApiError::internal("INTERNAL_ERROR", "relation candidate vanished")),
        _ => Err(ApiError::bad_request(
            "AMBIGUOUS_RELATIONSHIP",
            format!(
                "'{name}' matches more than one foreign-key relationship on '{}'; \
                 disambiguate by column name",
                base.name
            ),
        )),
    }
}

/// One embed spec resolved from `select=`: its output alias (the key it's
/// nested under), the relationship, and the requested sub-columns (empty =
/// all columns of the embedded resource).
struct EmbedSpec {
    alias: String,
    relation: Relation,
    sub_cols: Vec<String>,
}

/// Canonical string key for grouping/joining `Literal` values across the two
/// enforced queries (base + embed) — reuses [`dto::literal_to_json`]'s exact
/// per-variant mapping so a value round-trips to the same key regardless of
/// which query produced it (same column type on both sides of a FK).
fn literal_key(lit: &Literal) -> String {
    dto::literal_to_json(lit).to_string()
}

/// Fetch the related rows for one embed, keyed by `join_col`'s value — a
/// **second parameterized query**, run through the exact same [`run_stmt`]
/// enforced path (RLS + table/column grants) as everything else in this
/// module. `keys` are the distinct non-NULL join values collected from the
/// base result; empty `keys` short-circuits without a query.
async fn fetch_embedded(
    state: &AppState,
    principal: &AuthPrincipal,
    target_def: &TableDef,
    sub_cols: &[String],
    join_col: &str,
    keys: &[Literal],
) -> Result<Vec<(String, JsonMap<String, JsonValue>)>, ApiError> {
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    let wildcard = sub_cols.is_empty();
    let mut projection_cols = sub_cols.to_vec();
    if !wildcard && !projection_cols.iter().any(|c| c == join_col) {
        projection_cols.push(join_col.to_string());
    }
    let projection_sql = if wildcard {
        "*".to_string()
    } else {
        projection_cols
            .iter()
            .map(|c| quote_ident(c))
            .collect::<Vec<_>>()
            .join(", ")
    };

    let mut binds: Vec<Literal> = Vec::new();
    let placeholders: Vec<String> = keys
        .iter()
        .map(|k| {
            binds.push(k.clone());
            format!("${}", binds.len())
        })
        .collect();
    let sql = format!(
        "SELECT {projection_sql} FROM {} WHERE {} IN ({})",
        table_ident(&target_def.name),
        quote_ident(join_col),
        placeholders.join(", ")
    );

    let result = run_stmt(state, principal, sql, binds).await?;
    let (result_cols, rows) = match result {
        ExecResult::Rows { columns, rows } => (columns, rows),
        other => {
            return Err(ApiError::internal(
                "INTERNAL_ERROR",
                format!("expected rows from an embed query, got {other:?}"),
            ))
        }
    };
    let join_idx = result_cols
        .iter()
        .position(|c| c == join_col)
        .ok_or_else(|| {
            ApiError::internal(
                "INTERNAL_ERROR",
                "join column missing from embed query result",
            )
        })?;

    let wanted: Option<HashSet<&str>> =
        (!wildcard).then(|| sub_cols.iter().map(String::as_str).collect());

    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let key = literal_key(&row[join_idx]);
        let mut obj = JsonMap::new();
        for (i, cname) in result_cols.iter().enumerate() {
            if wanted.as_ref().is_none_or(|w| w.contains(cname.as_str())) {
                obj.insert(cname.clone(), dto::literal_to_json(&row[i]));
            }
        }
        out.push((key, obj));
    }
    Ok(out)
}

// ── route handlers ──────────────────────────────────────────────────────────

/// `GET /rest/v1/<table>?select=...&<filters>&order=...&limit=...&offset=...`
/// -> `SELECT`, plus (C2) embedded resource expansion for any `select=`
/// entries of the form `name(cols)`. Always exactly one base statement (plus
/// one follow-up statement per distinct embed, see [`fetch_embedded`]) — a
/// native SQL `IN (...)` promotes to the `Query` plan (see this module's doc
/// comment), so no multi-statement expansion is needed for filters.
pub async fn get_collection(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<AppState>,
    Path(table): Path<String>,
    RawQuery(raw): RawQuery,
) -> Result<Json<JsonValue>, ApiError> {
    let def = lookup_table(&state, &table).await?;
    let pairs = parse_query_pairs(raw.as_deref());

    let mut select_items: Vec<SelectItem> = Vec::new();
    let mut select_param_present = false;
    let mut order_keys: Vec<(String, bool)> = Vec::new();
    let mut limit: Option<i64> = None;
    let mut offset: Option<i64> = None;
    let mut filter_pairs: Vec<(String, String)> = Vec::new();

    for (k, v) in &pairs {
        match k.as_str() {
            "select" => {
                select_param_present = true;
                select_items.extend(parse_select_list(v)?);
            }
            "order" => order_keys.extend(parse_order_value(v)),
            "limit" => limit = Some(parse_nonneg_int(v, "limit")?),
            "offset" => offset = Some(parse_nonneg_int(v, "offset")?),
            _ => filter_pairs.push((k.clone(), v.clone())),
        }
    }

    let mut scalar_cols: Vec<String> = Vec::new();
    let mut embed_requests: Vec<(String, Vec<String>)> = Vec::new();
    for item in &select_items {
        match item {
            SelectItem::Column(c) => scalar_cols.push(c.clone()),
            SelectItem::Embed { name, columns } => {
                embed_requests.push((name.clone(), columns.clone()))
            }
        }
    }

    for c in &scalar_cols {
        validate_column(&def, c)?;
    }
    for (c, _) in &order_keys {
        validate_column(&def, c)?;
    }
    let filters = parse_filters(&def, &filter_pairs)?;

    // Resolve every embed against the full catalog (needed to find the
    // relationship's target/child table and validate its sub-columns).
    let mut embed_specs: Vec<EmbedSpec> = Vec::new();
    let all_defs = if embed_requests.is_empty() {
        Vec::new()
    } else {
        state.engine.table_defs().await?
    };
    for (name, columns) in &embed_requests {
        let relation = resolve_relation(&all_defs, &def, name)?;
        let target_name = match &relation {
            Relation::Forward { ref_table, .. } => ref_table,
            Relation::Reverse { child_table, .. } => child_table,
        };
        let target_def = all_defs
            .iter()
            .find(|d| &d.name == target_name)
            .ok_or_else(|| {
                ApiError::internal("INTERNAL_ERROR", "embedded table vanished from catalog")
            })?;
        for c in columns {
            validate_column(target_def, c)?;
        }
        embed_specs.push(EmbedSpec {
            alias: name.clone(),
            relation,
            sub_cols: columns.clone(),
        });
    }

    // The base query must also fetch each embed's join-key column, even if
    // the caller didn't select it explicitly (it's needed to stitch the
    // embed in afterwards; stripped from the output unless also requested).
    let mut hidden_needed: Vec<String> = Vec::new();
    for spec in &embed_specs {
        let needed = match &spec.relation {
            Relation::Forward { fk_column, .. } => fk_column.clone(),
            Relation::Reverse { ref_column, .. } => ref_column.clone(),
        };
        if !scalar_cols.contains(&needed) && !hidden_needed.contains(&needed) {
            hidden_needed.push(needed);
        }
    }

    let projection_sql = if select_param_present && !select_items.is_empty() {
        let mut cols = scalar_cols.clone();
        cols.extend(hidden_needed.iter().cloned());
        cols.iter()
            .map(|c| quote_ident(c))
            .collect::<Vec<_>>()
            .join(", ")
    } else {
        "*".to_string()
    };

    let mut binds: Vec<Literal> = Vec::new();
    let where_sql = append_where(&filters, &mut binds);

    let mut sql = format!("SELECT {projection_sql} FROM {}", table_ident(&def.name));
    if let Some(w) = where_sql {
        sql.push_str(" WHERE ");
        sql.push_str(&w);
    }
    if !order_keys.is_empty() {
        let order_sql = order_keys
            .iter()
            .map(|(c, desc)| format!("{} {}", quote_ident(c), if *desc { "DESC" } else { "ASC" }))
            .collect::<Vec<_>>()
            .join(", ");
        sql.push_str(" ORDER BY ");
        sql.push_str(&order_sql);
    }
    if let Some(l) = limit {
        sql.push_str(&format!(" LIMIT {l}"));
    }
    if let Some(o) = offset {
        sql.push_str(&format!(" OFFSET {o}"));
    }

    let result = run_stmt(&state, &principal, sql, binds).await?;

    if embed_specs.is_empty() {
        return Ok(Json(exec_result_to_json(&result)));
    }

    let (result_cols, rows) = match result {
        ExecResult::Rows { columns, rows } => (columns, rows),
        // A well-formed SELECT always yields `Rows`; fail safe rather than
        // silently dropping the requested embeds.
        other => return Ok(Json(exec_result_to_json(&other))),
    };

    /// One embed's fetched-and-grouped related rows, keyed by the base
    /// row's join-key value (canonical string key, see [`literal_key`]).
    struct Resolved {
        alias: String,
        is_forward: bool,
        base_key_idx: usize,
        grouped: HashMap<String, Vec<JsonMap<String, JsonValue>>>,
    }

    let mut resolved: Vec<Resolved> = Vec::with_capacity(embed_specs.len());
    for spec in &embed_specs {
        let (join_col_on_base, target_table_name, target_join_col, is_forward) =
            match &spec.relation {
                Relation::Forward {
                    fk_column,
                    ref_table,
                    ref_column,
                } => (
                    fk_column.clone(),
                    ref_table.clone(),
                    ref_column.clone(),
                    true,
                ),
                Relation::Reverse {
                    child_table,
                    fk_column,
                    ref_column,
                } => (
                    ref_column.clone(),
                    child_table.clone(),
                    fk_column.clone(),
                    false,
                ),
            };
        let base_key_idx = result_cols
            .iter()
            .position(|c| c == &join_col_on_base)
            .ok_or_else(|| {
                ApiError::internal("INTERNAL_ERROR", "join column missing from base result")
            })?;

        let mut seen: HashSet<String> = HashSet::new();
        let mut keys: Vec<Literal> = Vec::new();
        for row in &rows {
            let v = &row[base_key_idx];
            if matches!(v, Literal::Null) {
                continue;
            }
            if seen.insert(literal_key(v)) {
                keys.push(v.clone());
            }
        }

        let target_def = all_defs
            .iter()
            .find(|d| d.name == target_table_name)
            .ok_or_else(|| {
                ApiError::internal("INTERNAL_ERROR", "embedded table vanished from catalog")
            })?;
        let fetched = fetch_embedded(
            &state,
            &principal,
            target_def,
            &spec.sub_cols,
            &target_join_col,
            &keys,
        )
        .await?;

        let mut grouped: HashMap<String, Vec<JsonMap<String, JsonValue>>> = HashMap::new();
        for (k, obj) in fetched {
            grouped.entry(k).or_default().push(obj);
        }

        resolved.push(Resolved {
            alias: spec.alias.clone(),
            is_forward,
            base_key_idx,
            grouped,
        });
    }

    enum OutCol<'a> {
        Scalar(usize),
        Embed(&'a Resolved),
    }
    let mut out_plan: Vec<OutCol> = Vec::with_capacity(select_items.len());
    for item in &select_items {
        match item {
            SelectItem::Column(c) => {
                let idx = result_cols.iter().position(|rc| rc == c).ok_or_else(|| {
                    ApiError::internal("INTERNAL_ERROR", "selected column missing from result")
                })?;
                out_plan.push(OutCol::Scalar(idx));
            }
            SelectItem::Embed { name, .. } => {
                let r = resolved.iter().find(|r| &r.alias == name).ok_or_else(|| {
                    ApiError::internal("INTERNAL_ERROR", "embed alias missing from resolved set")
                })?;
                out_plan.push(OutCol::Embed(r));
            }
        }
    }

    let out_columns: Vec<String> = select_items
        .iter()
        .map(|item| match item {
            SelectItem::Column(c) => c.clone(),
            SelectItem::Embed { name, .. } => name.clone(),
        })
        .collect();

    let mut out_rows: Vec<JsonValue> = Vec::with_capacity(rows.len());
    for row in &rows {
        let mut out_row: Vec<JsonValue> = Vec::with_capacity(out_plan.len());
        for col in &out_plan {
            let value = match col {
                OutCol::Scalar(idx) => dto::literal_to_json(&row[*idx]),
                OutCol::Embed(r) => {
                    let key_val = &row[r.base_key_idx];
                    if matches!(key_val, Literal::Null) {
                        if r.is_forward {
                            JsonValue::Null
                        } else {
                            JsonValue::Array(Vec::new())
                        }
                    } else {
                        match r.grouped.get(&literal_key(key_val)) {
                            Some(list) if r.is_forward => list
                                .first()
                                .cloned()
                                .map(JsonValue::Object)
                                .unwrap_or(JsonValue::Null),
                            Some(list) => JsonValue::Array(
                                list.iter().cloned().map(JsonValue::Object).collect(),
                            ),
                            None if r.is_forward => JsonValue::Null,
                            None => JsonValue::Array(Vec::new()),
                        }
                    }
                }
            };
            out_row.push(value);
        }
        out_rows.push(JsonValue::Array(out_row));
    }

    Ok(Json(serde_json::json!({
        "type": "rows",
        "columns": out_columns,
        "rows": out_rows,
    })))
}

/// `POST /rest/v1/<table>` (JSON object or array of objects) -> `INSERT`.
pub async fn post_collection(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<AppState>,
    Path(table): Path<String>,
    body: Bytes,
) -> Result<Json<JsonValue>, ApiError> {
    let def = lookup_table(&state, &table).await?;
    let value: JsonValue = serde_json::from_slice(&body)
        .map_err(|e| ApiError::bad_request("INVALID_JSON", format!("invalid JSON body: {e}")))?;
    let rows: Vec<JsonMap<String, JsonValue>> = match value {
        JsonValue::Object(m) => vec![m],
        JsonValue::Array(items) => items
            .into_iter()
            .map(|item| match item {
                JsonValue::Object(m) => Ok(m),
                other => Err(ApiError::bad_request(
                    "INVALID_JSON",
                    format!("expected a JSON object in the array, got {other}"),
                )),
            })
            .collect::<Result<Vec<_>, _>>()?,
        other => {
            return Err(ApiError::bad_request(
                "INVALID_JSON",
                format!("expected a JSON object or array of objects, got {other}"),
            ))
        }
    };
    if rows.is_empty() {
        return Err(ApiError::bad_request(
            "EMPTY_BODY",
            "POST body must contain at least one row",
        ));
    }

    let (sql, binds) = build_insert(&def, &rows)?;
    let result = run_stmt(&state, &principal, sql, binds).await?;
    Ok(Json(exec_result_to_json(&result)))
}

fn parse_json_object_body(body: &Bytes) -> Result<JsonMap<String, JsonValue>, ApiError> {
    let value: JsonValue = serde_json::from_slice(body)
        .map_err(|e| ApiError::bad_request("INVALID_JSON", format!("invalid JSON body: {e}")))?;
    match value {
        JsonValue::Object(m) => Ok(m),
        other => Err(ApiError::bad_request(
            "INVALID_JSON",
            format!("body must be a JSON object, got {other}"),
        )),
    }
}

/// `PATCH /rest/v1/<table>?<filters>` (JSON object of assignments) ->
/// `UPDATE`.
pub async fn patch_collection(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<AppState>,
    Path(table): Path<String>,
    RawQuery(raw): RawQuery,
    body: Bytes,
) -> Result<Json<JsonValue>, ApiError> {
    let def = lookup_table(&state, &table).await?;
    let assignments_map = parse_json_object_body(&body)?;
    if assignments_map.is_empty() {
        return Err(ApiError::bad_request(
            "EMPTY_BODY",
            "PATCH body must contain at least one column assignment",
        ));
    }

    let pairs = parse_query_pairs(raw.as_deref());
    let filter_pairs: Vec<(String, String)> = pairs
        .into_iter()
        .filter(|(k, _)| !RESERVED_PARAMS.contains(&k.as_str()))
        .collect();
    let filters = parse_filters(&def, &filter_pairs)?;
    let (scalar_filters, in_filter) = extract_single_in(filters)?;

    let stmts = match in_filter {
        None => {
            let mut binds = Vec::new();
            let assign_sql = build_assignments(&def, &assignments_map, &mut binds)?;
            let where_sql = append_where(&scalar_filters, &mut binds);
            let mut sql = format!("UPDATE {} SET {assign_sql}", table_ident(&def.name));
            if let Some(w) = where_sql {
                sql.push_str(" WHERE ");
                sql.push_str(&w);
            }
            vec![(sql, binds)]
        }
        Some((col, values)) => values
            .into_iter()
            .map(|v| {
                let mut binds = Vec::new();
                let assign_sql = build_assignments(&def, &assignments_map, &mut binds)?;
                let mut all_filters = scalar_filters.clone();
                all_filters.push(Filter {
                    column: col.clone(),
                    op: ParsedOp::Eq(v),
                });
                let where_sql = append_where(&all_filters, &mut binds);
                let mut sql = format!("UPDATE {} SET {assign_sql}", table_ident(&def.name));
                if let Some(w) = where_sql {
                    sql.push_str(" WHERE ");
                    sql.push_str(&w);
                }
                Ok::<_, ApiError>((sql, binds))
            })
            .collect::<Result<Vec<_>, _>>()?,
    };

    let results = run_stmts(&state, &principal, stmts).await?;
    let merged = merge_counts(results)?;
    Ok(Json(exec_result_to_json(&merged)))
}

/// `DELETE /rest/v1/<table>?<filters>` -> `DELETE`.
pub async fn delete_collection(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<AppState>,
    Path(table): Path<String>,
    RawQuery(raw): RawQuery,
) -> Result<Json<JsonValue>, ApiError> {
    let def = lookup_table(&state, &table).await?;
    let pairs = parse_query_pairs(raw.as_deref());
    let filter_pairs: Vec<(String, String)> = pairs
        .into_iter()
        .filter(|(k, _)| !RESERVED_PARAMS.contains(&k.as_str()))
        .collect();
    let filters = parse_filters(&def, &filter_pairs)?;
    let (scalar_filters, in_filter) = extract_single_in(filters)?;

    let stmts = match in_filter {
        None => {
            let mut binds = Vec::new();
            let where_sql = append_where(&scalar_filters, &mut binds);
            let mut sql = format!("DELETE FROM {}", table_ident(&def.name));
            if let Some(w) = where_sql {
                sql.push_str(" WHERE ");
                sql.push_str(&w);
            }
            vec![(sql, binds)]
        }
        Some((col, values)) => values
            .into_iter()
            .map(|v| {
                let mut binds = Vec::new();
                let mut all_filters = scalar_filters.clone();
                all_filters.push(Filter {
                    column: col.clone(),
                    op: ParsedOp::Eq(v),
                });
                let where_sql = append_where(&all_filters, &mut binds);
                let mut sql = format!("DELETE FROM {}", table_ident(&def.name));
                if let Some(w) = where_sql {
                    sql.push_str(" WHERE ");
                    sql.push_str(&w);
                }
                (sql, binds)
            })
            .collect::<Vec<_>>(),
    };

    let results = run_stmts(&state, &principal, stmts).await?;
    let merged = merge_counts(results)?;
    Ok(Json(exec_result_to_json(&merged)))
}

// ── C3: minimal OpenAPI 3 document ──────────────────────────────────────────

/// `ColumnType` -> a minimal OpenAPI/JSON-Schema type descriptor.
fn column_type_schema(ty: &ColumnType) -> JsonValue {
    match ty {
        ColumnType::Int64 => serde_json::json!({"type": "integer", "format": "int64"}),
        ColumnType::Text => serde_json::json!({"type": "string"}),
        ColumnType::Bool => serde_json::json!({"type": "boolean"}),
        ColumnType::Json => serde_json::json!({}),
        ColumnType::Vector(n) => {
            serde_json::json!({"type": "array", "items": {"type": "number"}, "minItems": n, "maxItems": n})
        }
        // Rendered as exact decimal text on the wire (`dto::literal_to_json`).
        ColumnType::Decimal(_, _) => serde_json::json!({"type": "string", "format": "decimal"}),
        ColumnType::Timestamp => serde_json::json!({"type": "string", "format": "date-time"}),
        ColumnType::Float => serde_json::json!({"type": "number"}),
        ColumnType::Uuid => serde_json::json!({"type": "string", "format": "uuid"}),
        ColumnType::Bytea => serde_json::json!({"type": "string", "format": "byte"}),
        ColumnType::Date => serde_json::json!({"type": "string", "format": "date"}),
        ColumnType::Time => serde_json::json!({"type": "string"}),
    }
}

/// `GET /rest/v1/` (C3): a minimal OpenAPI 3 document generated from the
/// catalog — tables, columns, types, and PK/FK — feeding unidb-studio's
/// API-docs panel (item 123's G4 dependency). No hand-written per-table
/// business logic: this walks whatever tables exist at request time.
pub async fn get_openapi(State(state): State<AppState>) -> Result<Json<JsonValue>, ApiError> {
    let defs = state.engine.table_defs().await?;
    let mut paths = serde_json::Map::new();
    let mut schemas = serde_json::Map::new();

    for def in defs.iter().filter(|d| !is_internal_table(&d.name)) {
        let mut properties = serde_json::Map::new();
        let mut required = Vec::new();
        for col in &def.columns {
            if col.dropped {
                continue;
            }
            let mut schema = column_type_schema(&col.ty)
                .as_object()
                .cloned()
                .unwrap_or_default();
            if col.constraints.primary_key {
                schema.insert(
                    "description".into(),
                    JsonValue::String("primary key".into()),
                );
            }
            if let Some(fk) = &col.constraints.references {
                schema.insert(
                    "description".into(),
                    JsonValue::String(format!(
                        "references {}({})",
                        fk.table,
                        fk.column.clone().unwrap_or_default()
                    )),
                );
            }
            if col.constraints.not_null || col.constraints.primary_key {
                required.push(JsonValue::String(col.name.clone()));
            }
            properties.insert(col.name.clone(), JsonValue::Object(schema));
        }
        let mut table_schema = serde_json::Map::new();
        table_schema.insert("type".into(), JsonValue::String("object".into()));
        table_schema.insert("properties".into(), JsonValue::Object(properties));
        if !required.is_empty() {
            table_schema.insert("required".into(), JsonValue::Array(required));
        }
        if !def.constraints.primary_key.is_empty() {
            table_schema.insert(
                "x-primary-key".into(),
                JsonValue::Array(
                    def.constraints
                        .primary_key
                        .iter()
                        .cloned()
                        .map(JsonValue::String)
                        .collect(),
                ),
            );
        }
        schemas.insert(def.name.clone(), JsonValue::Object(table_schema));

        let schema_ref = serde_json::json!({"$ref": format!("#/components/schemas/{}", def.name)});
        let list_schema = serde_json::json!({"type": "array", "items": schema_ref.clone()});
        paths.insert(
            format!("/rest/v1/{}", def.name),
            serde_json::json!({
                "get": {
                    "summary": format!("List/query {}", def.name),
                    "parameters": [
                        {"name": "select", "in": "query", "schema": {"type": "string"}, "description": "comma-separated column projection"},
                        {"name": "order", "in": "query", "schema": {"type": "string"}, "description": "col.asc|col.desc, comma-separated"},
                        {"name": "limit", "in": "query", "schema": {"type": "integer"}},
                        {"name": "offset", "in": "query", "schema": {"type": "integer"}},
                    ],
                    "responses": {"200": {"description": "rows", "content": {"application/json": {"schema": list_schema}}}},
                },
                "post": {
                    "summary": format!("Insert into {}", def.name),
                    "requestBody": {"content": {"application/json": {"schema": {"oneOf": [schema_ref.clone(), list_schema]}}}},
                    "responses": {"200": {"description": "inserted"}},
                },
                "patch": {
                    "summary": format!("Update {}", def.name),
                    "requestBody": {"content": {"application/json": {"schema": schema_ref.clone()}}},
                    "responses": {"200": {"description": "updated"}},
                },
                "delete": {
                    "summary": format!("Delete from {}", def.name),
                    "responses": {"200": {"description": "deleted"}},
                },
            }),
        );
    }

    Ok(Json(serde_json::json!({
        "openapi": "3.0.3",
        "info": {"title": "unidb auto REST API", "version": "1"},
        "paths": paths,
        "components": {"schemas": schemas},
    })))
}
