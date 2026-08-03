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
//!
//! **Item 139 — `Prefer: count=exact` / `return=representation|minimal`**
//! (PostgREST parity, response controls only, no engine change):
//! - `GET … Prefer: count=exact` runs a **second** `SELECT COUNT(*) …` with
//!   the identical `WHERE`/binds as the main query, through the same
//!   [`run_stmt`] enforced path, and reports it as a `Content-Range` response
//!   header ([`build_content_range`]). Omitting the header costs nothing
//!   extra — the count query only ever runs when explicitly requested.
//! - `POST`/`PATCH`/`DELETE … Prefer: return=representation` appends
//!   `RETURNING *` to the generated statement(s) (same pattern
//!   `graphql.rs`'s mutation fields already use) and returns the affected
//!   rows as the body; `return=minimal` returns an empty `201`/`204` with no
//!   body. No `Prefer` header keeps every handler's exact pre-item-139
//!   response (status, body, headers) — see [`parse_prefer`].

use std::collections::{HashMap, HashSet};

use axum::{
    body::Bytes,
    extract::{Path, RawQuery, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
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

const RESERVED_PARAMS: [&str; 5] = ["select", "order", "limit", "offset", "on_conflict"];

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

/// Merge a batch of `Rows` results (the `in.(...)` multi-statement
/// expansion, see this module's doc comment) produced by `RETURNING *` under
/// `Prefer: return=representation` — every statement targets the same table
/// with the same `RETURNING *`, so the column list is identical across all
/// of them; only the row lists are concatenated, in statement order (the
/// same shape as [`merge_counts`], for `Rows` instead of counts).
fn merge_rows(results: Vec<ExecResult>) -> Result<ExecResult, ApiError> {
    let mut columns: Option<Vec<String>> = None;
    let mut all_rows = Vec::new();
    for r in results {
        match r {
            ExecResult::Rows { columns: c, rows } => {
                if columns.is_none() {
                    columns = Some(c);
                }
                all_rows.extend(rows);
            }
            other => {
                return Err(ApiError::internal(
                    "INTERNAL_ERROR",
                    format!("unexpected result merging rows: {other:?}"),
                ))
            }
        }
    }
    Ok(ExecResult::Rows {
        columns: columns.unwrap_or_default(),
        rows: all_rows,
    })
}

// ── item 139: `Prefer` header (count + return-shape controls) ──────────────

/// Parsed `Prefer` request-header preferences this module understands.
/// `Default` (`count_exact: false`, `return_pref: None`) is exactly
/// "no `Prefer` header" — every handler's behavior at that default must stay
/// byte-identical to its pre-item-139 behavior (see [`parse_prefer`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Prefer {
    pub(super) count_exact: bool,
    pub(super) return_pref: Option<ReturnPref>,
    /// Item 150: `Prefer: resolution=merge-duplicates|ignore-duplicates` —
    /// `POST /rest/v1/<table>` upsert mode. `None` (no `resolution=` token)
    /// means the pre-150 behavior: a conflicting row is a plain
    /// `UniqueViolation` error, exactly as before this item shipped.
    pub(super) resolution: Option<Resolution>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReturnPref {
    Representation,
    Minimal,
}

/// Item 150: PostgREST's two `Prefer: resolution=` upsert modes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Resolution {
    /// `resolution=merge-duplicates` — `ON CONFLICT (<on_conflict>) DO UPDATE
    /// SET <col> = EXCLUDED.<col>, ...` for every payload column other than
    /// the conflict target itself.
    MergeDuplicates,
    /// `resolution=ignore-duplicates` — `ON CONFLICT [(<on_conflict>)] DO
    /// NOTHING`.
    IgnoreDuplicates,
}

/// Parse every `Prefer` header on the request (a client may repeat the
/// header; each occurrence's value may itself be a comma-separated list —
/// both forms are folded together) case-insensitively into a [`Prefer`].
/// Unrecognized tokens (`count=planned`, a typo, a future PostgREST
/// preference this module doesn't implement, …) are silently ignored rather
/// than rejected — the PostgREST posture the backlog spec calls for. Also
/// returns the list of tokens actually recognized, verbatim, for an
/// (optional) `Preference-Applied` echo header.
pub(super) fn parse_prefer(headers: &HeaderMap) -> (Prefer, Vec<&'static str>) {
    let mut prefer = Prefer::default();
    let mut applied = Vec::new();
    for value in headers.get_all("prefer") {
        let Ok(s) = value.to_str() else { continue };
        for token in s.split(',') {
            match token.trim().to_ascii_lowercase().as_str() {
                "count=exact" => {
                    prefer.count_exact = true;
                    applied.push("count=exact");
                }
                "return=representation" => {
                    prefer.return_pref = Some(ReturnPref::Representation);
                    applied.push("return=representation");
                }
                "return=minimal" => {
                    prefer.return_pref = Some(ReturnPref::Minimal);
                    applied.push("return=minimal");
                }
                // Item 150: upsert resolution mode (`POST` only — a `PATCH`/
                // `DELETE`/`GET` request naming one simply has no effect,
                // same "recognized but not applicable here" posture as any
                // other `Prefer` token that handler doesn't consult).
                "resolution=merge-duplicates" => {
                    prefer.resolution = Some(Resolution::MergeDuplicates);
                    applied.push("resolution=merge-duplicates");
                }
                "resolution=ignore-duplicates" => {
                    prefer.resolution = Some(Resolution::IgnoreDuplicates);
                    applied.push("resolution=ignore-duplicates");
                }
                _ => {} // unrecognized preference: ignored, not an error
            }
        }
    }
    (prefer, applied)
}

/// `Content-Range: <from>-<to>/<total>` (item 139 §1) — `<from>-<to>` is the
/// returned row window (`offset..offset+returned-1`), or `*` when the query
/// returned zero rows (PostgREST's convention — there is no meaningful
/// window to report).
fn build_content_range(offset: i64, returned: usize, total: i64) -> String {
    if returned == 0 {
        format!("*/{total}")
    } else {
        format!("{offset}-{}/{total}", offset + returned as i64 - 1)
    }
}

/// Attach the item-139 response headers (`Content-Range` when a count was
/// computed, `Preference-Applied` when at least one recognized preference
/// was echoed) to an already-built response, without disturbing its status
/// or body. A no-`Prefer` request passes `content_range: None` and an empty
/// `applied` list, so this is a no-op — the byte-identical-response
/// guarantee holds structurally, not by a special-cased branch.
fn with_prefer_headers(
    mut resp: Response,
    content_range: Option<String>,
    applied: &[&'static str],
) -> Response {
    if let Some(cr) = content_range {
        if let Ok(hv) = HeaderValue::from_str(&cr) {
            resp.headers_mut().insert(header::CONTENT_RANGE, hv);
        }
    }
    if !applied.is_empty() {
        if let Ok(hv) = HeaderValue::from_str(&applied.join(", ")) {
            resp.headers_mut().insert("preference-applied", hv);
        }
    }
    resp
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

/// Item 136: per-embed `{filter, order, limit, offset}` parsed from
/// `<embed>.<col>=<op>.<val>` / `<embed>.order=col.asc,...` /
/// `<embed>.limit=n` / `<embed>.offset=n` query params — reserved sub-keys
/// are `order`/`limit`/`offset`; everything else under the prefix is a
/// filter on that column of the embedded resource. `limit`/`offset` are
/// applied **per-parent** (lateral semantics) after the embedded rows are
/// stitched to their parent by join key — see [`get_collection`]'s slicing
/// step — never as a single combined SQL `LIMIT` on the `IN (...)` query,
/// which would wrongly cap across all parents.
#[derive(Clone, Debug, Default)]
struct EmbedQuery {
    filters: Vec<Filter>,
    order: Vec<(String, bool)>,
    limit: Option<i64>,
    offset: Option<i64>,
}

/// Parse one embed's dotted param group (prefix already stripped) against
/// the embedded resource's own catalog (`target_def`) — reuses the exact
/// same operator/order/int grammar as the top-level path
/// ([`parse_op`]/[`parse_order_value`]/[`parse_nonneg_int`]), just scoped to
/// a different table's columns.
fn parse_embed_query(
    target_def: &TableDef,
    pairs: &[(String, String)],
) -> Result<EmbedQuery, ApiError> {
    let mut q = EmbedQuery::default();
    for (sub, v) in pairs {
        match sub.as_str() {
            "order" => q.order.extend(parse_order_value(v)),
            "limit" => q.limit = Some(parse_nonneg_int(v, "limit")?),
            "offset" => q.offset = Some(parse_nonneg_int(v, "offset")?),
            _ => {
                validate_column(target_def, sub)?;
                q.filters.push(Filter {
                    column: sub.clone(),
                    op: parse_op(v)?,
                });
            }
        }
    }
    for (c, _) in &q.order {
        validate_column(target_def, c)?;
    }
    Ok(q)
}

/// One embed spec resolved from `select=`: its output alias (the key it's
/// nested under), the relationship, the requested sub-columns (empty = all
/// columns of the embedded resource), and (item 136) any dotted-param
/// filter/order/limit/offset scoped to it.
struct EmbedSpec {
    alias: String,
    relation: Relation,
    sub_cols: Vec<String>,
    query: EmbedQuery,
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
///
/// Item 136: `query`'s filters are AND-combined with the `join_col IN
/// (...)` clause (same `append_where` fragment builder + `$n` binds as the
/// top-level path — no new enforcement or injection surface), and its
/// requested order is appended **after** `join_col` in `ORDER BY` so a
/// caller's per-embed ordering is stable within each parent's group.
/// Deliberately **no SQL `LIMIT`/`OFFSET` here** — per-parent pagination is
/// sliced in Rust by the caller after grouping by join key (a single SQL
/// `LIMIT` would wrongly cap rows across all parents combined, not per
/// parent).
async fn fetch_embedded(
    state: &AppState,
    principal: &AuthPrincipal,
    target_def: &TableDef,
    sub_cols: &[String],
    join_col: &str,
    keys: &[Literal],
    query: &EmbedQuery,
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
    let in_clause = format!("{} IN ({})", quote_ident(join_col), placeholders.join(", "));
    let where_sql = match append_where(&query.filters, &mut binds) {
        Some(f) => format!("{in_clause} AND {f}"),
        None => in_clause,
    };
    let mut order_parts = vec![quote_ident(join_col)];
    order_parts.extend(
        query
            .order
            .iter()
            .map(|(c, desc)| format!("{} {}", quote_ident(c), if *desc { "DESC" } else { "ASC" })),
    );
    let sql = format!(
        "SELECT {projection_sql} FROM {} WHERE {where_sql} ORDER BY {}",
        table_ident(&target_def.name),
        order_parts.join(", "),
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
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let (prefer, applied) = parse_prefer(&headers);
    let def = lookup_table(&state, &table).await?;
    let pairs = parse_query_pairs(raw.as_deref());

    // item 136: split into top-level params and `<embed>.<rest>` groups on
    // the FIRST dot — safe because no top-level column/param identifier in
    // this module ever contains a dot (see this module's doc comment).
    // Reserved sub-keys (`order`/`limit`/`offset`) and filter columns are
    // disambiguated per-group below, once the embed's target table (and
    // thus its catalog) is known.
    let mut top_pairs: Vec<(String, String)> = Vec::new();
    let mut embed_param_groups: HashMap<String, Vec<(String, String)>> = HashMap::new();
    for (k, v) in pairs {
        match k.split_once('.') {
            Some((prefix, sub)) => embed_param_groups
                .entry(prefix.to_string())
                .or_default()
                .push((sub.to_string(), v)),
            None => top_pairs.push((k, v)),
        }
    }

    let mut select_items: Vec<SelectItem> = Vec::new();
    let mut select_param_present = false;
    let mut order_keys: Vec<(String, bool)> = Vec::new();
    let mut limit: Option<i64> = None;
    let mut offset: Option<i64> = None;
    let mut filter_pairs: Vec<(String, String)> = Vec::new();

    for (k, v) in &top_pairs {
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
        let query = match embed_param_groups.remove(name) {
            Some(group) => parse_embed_query(target_def, &group)?,
            None => EmbedQuery::default(),
        };
        embed_specs.push(EmbedSpec {
            alias: name.clone(),
            relation,
            sub_cols: columns.clone(),
            query,
        });
    }

    // item 136: any dotted param group whose prefix never matched an
    // embedded relation from `select=` is a clear 400, not a silently
    // ignored no-op.
    if let Some((unknown_prefix, _)) = embed_param_groups.into_iter().next() {
        return Err(ApiError::bad_request(
            "UNKNOWN_EMBED_PARAM",
            format!("`{unknown_prefix}.*` does not name an embedded relation in `select=`"),
        ));
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

    // item 139 §1: the returned-row window is the base query's own row
    // count, computed before `result` is consumed below — embeds never
    // affect `Content-Range` (it describes the base resource, matching
    // PostgREST). Borrowed, not moved, so `result` is still usable after.
    let returned = match &result {
        ExecResult::Rows { rows, .. } => rows.len(),
        _ => 0,
    };
    let content_range_header = if prefer.count_exact {
        // Same `WHERE`/binds as the main query, rebuilt fresh (`binds` above
        // was already moved into `run_stmt`) — reuses `append_where` on the
        // same `filters`, so no new interpolation of user values. Runs
        // through the identical enforced `run_stmt` path (RLS/grants apply).
        let mut count_binds: Vec<Literal> = Vec::new();
        let count_where = append_where(&filters, &mut count_binds);
        let mut count_sql = format!("SELECT COUNT(*) FROM {}", table_ident(&def.name));
        if let Some(w) = count_where {
            count_sql.push_str(" WHERE ");
            count_sql.push_str(&w);
        }
        let count_result = run_stmt(&state, &principal, count_sql, count_binds).await?;
        let total = match &count_result {
            ExecResult::Rows { rows, .. } => match rows.first().and_then(|r| r.first()) {
                Some(Literal::Int(n)) => *n,
                _ => {
                    return Err(ApiError::internal(
                        "INTERNAL_ERROR",
                        "COUNT(*) did not return an integer",
                    ))
                }
            },
            other => {
                return Err(ApiError::internal(
                    "INTERNAL_ERROR",
                    format!("expected rows from a COUNT(*) query, got {other:?}"),
                ))
            }
        };
        Some(build_content_range(offset.unwrap_or(0), returned, total))
    } else {
        None
    };

    if embed_specs.is_empty() {
        let resp = Json(exec_result_to_json(&result)).into_response();
        return Ok(with_prefer_headers(resp, content_range_header, &applied));
    }

    let (result_cols, rows) = match result {
        ExecResult::Rows { columns, rows } => (columns, rows),
        // A well-formed SELECT always yields `Rows`; fail safe rather than
        // silently dropping the requested embeds.
        other => {
            let resp = Json(exec_result_to_json(&other)).into_response();
            return Ok(with_prefer_headers(resp, content_range_header, &applied));
        }
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
            &spec.query,
        )
        .await?;

        let mut grouped: HashMap<String, Vec<JsonMap<String, JsonValue>>> = HashMap::new();
        for (k, obj) in fetched {
            grouped.entry(k).or_default().push(obj);
        }

        // item 136: per-parent (lateral) limit/offset — slice EACH parent's
        // group in the already-applied SQL order, never a single combined
        // cap across all parents. Rows already arrived pre-sorted by
        // `join_col, <requested order>` (see `fetch_embedded`), and
        // `HashMap::entry(...).or_default().push` above preserves that
        // per-group arrival order.
        if spec.query.limit.is_some() || spec.query.offset.is_some() {
            let offset = spec.query.offset.unwrap_or(0) as usize;
            let limit = spec.query.limit.map(|l| l as usize).unwrap_or(usize::MAX);
            for v in grouped.values_mut() {
                *v = v.drain(..).skip(offset).take(limit).collect();
            }
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

    let resp = Json(serde_json::json!({
        "type": "rows",
        "columns": out_columns,
        "rows": out_rows,
    }))
    .into_response();
    Ok(with_prefer_headers(resp, content_range_header, &applied))
}

/// Item 150: build the ` ON CONFLICT ...` clause text for `POST /rest/v1`'s
/// upsert modes. `target` is the (already-validated) `on_conflict=<col>`
/// query param; `payload_columns` is the insert row's own column set (the
/// same list [`build_insert`] derives its column list from — every row in
/// the batch shares it, already checked there).
///
/// `MergeDuplicates` sets every payload column **except** the conflict
/// target itself (setting the target to its own `EXCLUDED` value is both
/// redundant — it's exactly the value that just matched — and pure overhead
/// on the engine side, since a target-column `SET` forces the non-HOT/
/// unique-recheck path per `has_unique_in_set`, item 117). A payload with no
/// non-target column errors clearly rather than emitting invalid empty-SET
/// SQL. `target: None` is rejected here (mirrors the engine grammar: DO
/// UPDATE requires an explicit conflict target) — `on_conflict=<col>` is
/// required whenever `resolution=merge-duplicates` is requested.
///
/// Column names here are deliberately **not** `quote_ident`-wrapped, unlike
/// `render_filter`/embed ORDER BY elsewhere in this module — matching
/// `build_insert`'s / `build_assignments`'s existing convention for the
/// exact same reason: an assignment target parses via `sqlparser`'s
/// `AssignmentTarget::ColumnName` -> `ObjectName::to_string()`, which
/// (like `table_ident`'s documented quirk) re-includes quote characters
/// instead of stripping them, so a quoted `SET "col" = ...` here would
/// literally name a column called `"col"` (quotes and all) and 404.
/// Already catalog-validated (`validate_column`) either way, so safety
/// doesn't depend on quoting here.
fn append_on_conflict(
    sql: &mut String,
    def: &TableDef,
    target: Option<&str>,
    resolution: Resolution,
    payload_columns: &[String],
) -> Result<(), ApiError> {
    match resolution {
        Resolution::IgnoreDuplicates => {
            sql.push_str(" ON CONFLICT");
            if let Some(col) = target {
                validate_column(def, col)?;
                sql.push_str(&format!(" ({col})"));
            }
            sql.push_str(" DO NOTHING");
        }
        Resolution::MergeDuplicates => {
            let Some(col) = target else {
                return Err(ApiError::bad_request(
                    "MISSING_ON_CONFLICT",
                    "Prefer: resolution=merge-duplicates requires an on_conflict=<column> \
                     query parameter naming the PRIMARY KEY or a UNIQUE column",
                ));
            };
            validate_column(def, col)?;
            let set_cols: Vec<&String> = payload_columns.iter().filter(|c| *c != col).collect();
            if set_cols.is_empty() {
                return Err(ApiError::bad_request(
                    "EMPTY_MERGE",
                    "resolution=merge-duplicates has no column to merge — the payload \
                     only carries the on_conflict target column",
                ));
            }
            let assignments = set_cols
                .iter()
                .map(|c| format!("{c} = EXCLUDED.{c}"))
                .collect::<Vec<_>>()
                .join(", ");
            sql.push_str(&format!(" ON CONFLICT ({col}) DO UPDATE SET {assignments}"));
        }
    }
    Ok(())
}

/// `POST /rest/v1/<table>` (JSON object or array of objects) -> `INSERT`.
///
/// Item 139 §2: with no `Prefer` header this is exactly the pre-item-139
/// response (`200`, `{"type":"inserted","count":N}`) — unchanged.
/// `Prefer: return=representation` appends `RETURNING *` to the generated
/// `INSERT` (same enforced [`run_stmt`] path, same `Engine::check_returning`
/// grant check `/sql`'s own `RETURNING` uses) and returns the inserted rows.
/// `Prefer: return=minimal` returns `201 Created` with an empty body.
///
/// Item 150 — upsert: `on_conflict=<col>` + `Prefer: resolution=
/// merge-duplicates|ignore-duplicates` appends an `ON CONFLICT` clause (see
/// [`append_on_conflict`]). No `resolution=` token → byte-identical
/// pre-150 behavior (a conflicting row is a plain `UniqueViolation` 4xx),
/// regardless of whether `on_conflict=` is present — mirrors real
/// PostgREST, where the target param alone does nothing without the
/// `Prefer` token driving the actual mode. Composes with `return=` exactly
/// as a plain insert does (`RETURNING *` on either arm — G5/item 19's
/// existing INSERT+UPDATE RETURNING support).
pub async fn post_collection(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<AppState>,
    Path(table): Path<String>,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let (prefer, applied) = parse_prefer(&headers);
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

    let (mut sql, binds) = build_insert(&def, &rows)?;
    if let Some(resolution) = prefer.resolution {
        let pairs = parse_query_pairs(raw.as_deref());
        let on_conflict_target = pairs
            .iter()
            .find(|(k, _)| k == "on_conflict")
            .map(|(_, v)| v.as_str());
        let payload_columns: Vec<String> = rows[0].keys().cloned().collect();
        append_on_conflict(
            &mut sql,
            &def,
            on_conflict_target,
            resolution,
            &payload_columns,
        )?;
    }
    let representation = prefer.return_pref == Some(ReturnPref::Representation);
    if representation {
        sql.push_str(" RETURNING *");
    }
    let result = run_stmt(&state, &principal, sql, binds).await?;

    let resp = if prefer.return_pref == Some(ReturnPref::Minimal) {
        StatusCode::CREATED.into_response()
    } else {
        Json(exec_result_to_json(&result)).into_response()
    };
    Ok(with_prefer_headers(resp, None, &applied))
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
///
/// Item 139 §2: no `Prefer` header is the exact pre-item-139 response
/// (`200`, `{"type":"updated","count":N}`). `return=representation` adds
/// `RETURNING *` to every generated statement (incl. each `in.(...)`
/// expansion, see this module's doc comment) and merges their rows via
/// [`merge_rows`] instead of [`merge_counts`]. `return=minimal` returns
/// `204 No Content` with an empty body.
pub async fn patch_collection(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<AppState>,
    Path(table): Path<String>,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let (prefer, applied) = parse_prefer(&headers);
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

    let representation = prefer.return_pref == Some(ReturnPref::Representation);
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
            if representation {
                sql.push_str(" RETURNING *");
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
                if representation {
                    sql.push_str(" RETURNING *");
                }
                Ok::<_, ApiError>((sql, binds))
            })
            .collect::<Result<Vec<_>, _>>()?,
    };

    let results = run_stmts(&state, &principal, stmts).await?;

    let resp = match prefer.return_pref {
        Some(ReturnPref::Minimal) => StatusCode::NO_CONTENT.into_response(),
        Some(ReturnPref::Representation) => {
            let merged = merge_rows(results)?;
            Json(exec_result_to_json(&merged)).into_response()
        }
        None => {
            let merged = merge_counts(results)?;
            Json(exec_result_to_json(&merged)).into_response()
        }
    };
    Ok(with_prefer_headers(resp, None, &applied))
}

/// `DELETE /rest/v1/<table>?<filters>` -> `DELETE`.
///
/// Item 139 §2: same three-way `Prefer: return=` behavior as
/// [`patch_collection`] — no header keeps the pre-item-139 `200`
/// count-body response, `representation` adds `RETURNING *` + [`merge_rows`],
/// `minimal` returns `204 No Content` with an empty body.
pub async fn delete_collection(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<AppState>,
    Path(table): Path<String>,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let (prefer, applied) = parse_prefer(&headers);
    let def = lookup_table(&state, &table).await?;
    let pairs = parse_query_pairs(raw.as_deref());
    let filter_pairs: Vec<(String, String)> = pairs
        .into_iter()
        .filter(|(k, _)| !RESERVED_PARAMS.contains(&k.as_str()))
        .collect();
    let filters = parse_filters(&def, &filter_pairs)?;
    let (scalar_filters, in_filter) = extract_single_in(filters)?;

    let representation = prefer.return_pref == Some(ReturnPref::Representation);
    let stmts = match in_filter {
        None => {
            let mut binds = Vec::new();
            let where_sql = append_where(&scalar_filters, &mut binds);
            let mut sql = format!("DELETE FROM {}", table_ident(&def.name));
            if let Some(w) = where_sql {
                sql.push_str(" WHERE ");
                sql.push_str(&w);
            }
            if representation {
                sql.push_str(" RETURNING *");
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
                if representation {
                    sql.push_str(" RETURNING *");
                }
                (sql, binds)
            })
            .collect::<Vec<_>>(),
    };

    let results = run_stmts(&state, &principal, stmts).await?;

    let resp = match prefer.return_pref {
        Some(ReturnPref::Minimal) => StatusCode::NO_CONTENT.into_response(),
        Some(ReturnPref::Representation) => {
            let merged = merge_rows(results)?;
            Json(exec_result_to_json(&merged)).into_response()
        }
        None => {
            let merged = merge_counts(results)?;
            Json(exec_result_to_json(&merged)).into_response()
        }
    };
    Ok(with_prefer_headers(resp, None, &applied))
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
