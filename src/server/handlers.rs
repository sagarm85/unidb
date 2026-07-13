//! One `async fn` per route. Every mutating handler runs under exactly one
//! transaction — either its own one-shot `begin -> execute ->
//! commit-or-abort` cycle (the default), or, when the request carries an
//! `X-Txn-Id` header (R1), a statement inside a client-held **transaction
//! session** that a later `POST /txn/{id}/commit` / `/rollback` finishes.
//! Session checkout (existence, principal binding, in-session serialization)
//! is enforced by [`txn_session::TxnSessions`]; different sessions and
//! one-shot requests all execute concurrently through the shared
//! `Arc<Engine>` (P5.e-3).
//!
//! **Session error semantics (documented contract):** a failed *mutating*
//! statement may have left partial effects inside the open transaction, so
//! it aborts the transaction and destroys the session (Postgres-without-
//! savepoints semantics — the client re-begins). Failed *pure reads*
//! (`GET /rows/…`, `GET /edges/from/…`) leave the session open: a 404 probe
//! for a missing row is a normal outcome, not a transaction fault. Requests
//! rejected *before* execution (unknown session, busy session, DDL-in-
//! session, authorization failure) never touch the transaction and leave
//! the session open.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    Extension, Json,
};
use serde_json::json;

use crate::{
    error::Result,
    format::Xid,
    heap::RowId,
    server::{
        auth::CurrentUser,
        dto::{
            exec_result_to_json, is_internal_table, json_to_literal, literal_to_json, slot_to_json,
            table_def_to_info, AckEventsRequest, AdvanceSlotRequest, BatchInsertRequest,
            BeginTxnRequest, CreateEdgeRequest, CreateSlotRequest, CursorQuery, CypherRequest,
            DeleteEdgeRequest, IsolationDto, RlsRequest, RowIdResponse, SetIndexRequest,
            SqlRequest, StreamQuery, TableInfo,
        },
        engine_handle::EngineHandle,
        error::ApiError,
        txn_session::SessionGuard,
        AppState,
    },
    sql::executor::ExecResult,
};

/// Commit `xid` on `Ok`, abort it on `Err` — the one piece of boilerplate
/// every one-shot mutating handler shares.
async fn finish<T>(engine: &EngineHandle, xid: Xid, result: Result<T>) -> Result<T> {
    match result {
        Ok(value) => {
            engine.commit(xid).await?;
            Ok(value)
        }
        Err(e) => {
            // Best-effort: if the abort itself fails (e.g. the engine is
            // shutting down), the original error is still the one that
            // matters to the client.
            let _ = engine.abort(xid).await;
            Err(e)
        }
    }
}

// ── transaction-session plumbing (R1) ────────────────────────────────────

/// Parse the optional `X-Txn-Id` header. Absent → `None` (one-shot);
/// present-but-malformed → `400 BAD_TXN_ID`.
fn parse_txn_header(headers: &HeaderMap) -> std::result::Result<Option<Xid>, ApiError> {
    let Some(value) = headers.get("x-txn-id") else {
        return Ok(None);
    };
    value
        .to_str()
        .ok()
        .and_then(|s| s.trim().parse::<Xid>().ok())
        .map(Some)
        .ok_or_else(|| {
            ApiError::bad_request(
                "BAD_TXN_ID",
                "X-Txn-Id must be the decimal transaction id returned by POST /txn/begin",
            )
        })
}

/// How the current request's transaction is scoped: a self-contained
/// one-shot transaction, or a statement inside a checked-out session.
enum TxnScope {
    OneShot,
    Session(SessionGuard),
}

/// Resolve the request's transaction: check the session out if `X-Txn-Id`
/// is present, otherwise begin a fresh one-shot transaction.
async fn begin_scoped(
    state: &AppState,
    headers: &HeaderMap,
    principal: &Option<String>,
) -> std::result::Result<(Xid, TxnScope), ApiError> {
    match parse_txn_header(headers)? {
        Some(txn_id) => {
            let guard = state.sessions.checkout(txn_id, principal)?;
            Ok((guard.session.xid, TxnScope::Session(guard)))
        }
        None => {
            let xid = state.engine.begin(None).await?;
            Ok((xid, TxnScope::OneShot))
        }
    }
}

/// Conclude a **mutating** statement. One-shot: commit-or-abort as always.
/// Session: keep the transaction open on success (refreshing the idle
/// clock); on failure abort it and destroy the session — a failed mutation
/// may have left partial effects the client must not be able to commit.
async fn finish_scoped<T>(
    state: &AppState,
    xid: Xid,
    scope: TxnScope,
    result: Result<T>,
) -> std::result::Result<T, ApiError> {
    match scope {
        TxnScope::OneShot => finish(&state.engine, xid, result)
            .await
            .map_err(ApiError::from),
        TxnScope::Session(guard) => match result {
            Ok(value) => {
                drop(guard); // releases the session + refreshes its idle clock
                Ok(value)
            }
            Err(e) => {
                let _ = state.engine.abort(xid).await;
                state.sessions.remove(xid);
                drop(guard);
                Err(e.into())
            }
        },
    }
}

/// Pre-execution gate for SQL inside a session: catalog DDL and auth DDL
/// are rejected up front (the session stays open — nothing executed). The
/// engine's DDL rollback is *request*-scoped (P2.c), not transaction-scoped,
/// so DDL held open across requests could not be rolled back correctly on
/// `POST /txn/{id}/rollback`; auth DDL mutates the role store outside the
/// transaction entirely.
fn ensure_session_sql_allowed(sql: &str) -> std::result::Result<(), ApiError> {
    if crate::authz::parse_auth_stmt(sql)
        .map_err(ApiError::from)?
        .is_some()
    {
        return Err(ApiError::bad_request(
            "DDL_IN_SESSION",
            "auth DDL (CREATE USER/ROLE, GRANT, REVOKE) is not transactional; run it as a one-shot request",
        ));
    }
    use crate::sql::logical::LogicalPlan as P;
    for plan in crate::sql::parser::parse_sql(sql).map_err(ApiError::from)? {
        if matches!(
            plan,
            P::CreateTable { .. }
                | P::CreateIndex { .. }
                | P::AlterTableAddColumn { .. }
                | P::AlterTableDropColumn { .. }
                | P::DropTable { .. }
                | P::Truncate { .. }
                | P::Analyze { .. }
        ) {
            return Err(ApiError::bad_request(
                "DDL_IN_SESSION",
                "catalog DDL is not supported inside a transaction session (DDL rollback is request-scoped, not transaction-scoped); run it as a one-shot request",
            ));
        }
    }
    Ok(())
}

/// Pre-execution gate for cursor mode (R4): the request must be a single
/// rows-producing statement (SELECT / query / EXPLAIN). Checked *before*
/// execution so a mutating statement is never executed-then-rejected.
fn ensure_cursor_sql(sql: &str) -> std::result::Result<(), ApiError> {
    let not_rows = || {
        ApiError::bad_request(
            "CURSOR_NOT_ROWS",
            "cursor mode requires exactly one rows-producing statement (SELECT/query/EXPLAIN)",
        )
    };
    if crate::authz::parse_auth_stmt(sql)
        .map_err(ApiError::from)?
        .is_some()
    {
        return Err(not_rows());
    }
    use crate::sql::logical::LogicalPlan as P;
    match crate::sql::parser::parse_sql(sql)
        .map_err(ApiError::from)?
        .as_slice()
    {
        [P::Select { .. }] | [P::Query(_)] | [P::Explain { .. }] => Ok(()),
        _ => Err(not_rows()),
    }
}

/// The session's sliding idle deadline as a wall-clock UTC timestamp — each
/// completed request pushes it out again.
fn idle_deadline_string(idle: Duration) -> String {
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as i64
        + idle.as_micros() as i64;
    crate::sql::datetime::format_timestamp(micros)
}

// ── transactions (R1) ────────────────────────────────────────────────────

/// Open a transaction session: a real, client-held engine transaction.
/// Subsequent `/sql`, `/cypher`, `/rows`, `/edges` requests carrying
/// `X-Txn-Id: <txn_id>` run inside it; `POST /txn/{id}/commit` /
/// `/rollback` finish it. Body optional: `{"isolation":
/// "read_committed" | "repeatable_read" | "serializable"}`.
pub async fn post_txn_begin(
    Extension(current_user): Extension<CurrentUser>,
    State(state): State<AppState>,
    body: Bytes,
) -> std::result::Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    // Manual body parse so an *invalid* body is a hard 400, while an
    // *absent* body cleanly defaults to READ COMMITTED.
    let req: BeginTxnRequest = if body.is_empty() {
        BeginTxnRequest::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| {
            ApiError::bad_request("BAD_REQUEST_BODY", format!("invalid begin body: {e}"))
        })?
    };
    let iso_dto = req.isolation.unwrap_or(IsolationDto::ReadCommitted);
    let xid = state.engine.begin(Some(iso_dto.to_engine())).await?;
    state
        .sessions
        .register(xid, current_user.0, iso_dto.to_engine());
    let idle = state.sessions.idle_timeout();
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "txn_id": xid,
            // Historical field name from when this route was
            // introspection-only — kept so old callers keep working.
            "xid": xid,
            "isolation": iso_dto.as_str(),
            "idle_timeout_secs": idle.as_secs(),
            // Sliding deadline: every completed request on the session
            // pushes it out by idle_timeout_secs again.
            "expires_at": idle_deadline_string(idle),
        })),
    ))
}

/// Commit a transaction session. Whatever the outcome, the session is
/// finished afterwards: `Engine::commit` either committed, or (e.g.
/// `SERIALIZATION_FAILURE`) already rolled the transaction back — the error
/// is reported on a fully cleaned-up transaction, and the client re-begins.
pub async fn post_txn_commit(
    Extension(current_user): Extension<CurrentUser>,
    State(state): State<AppState>,
    Path(txn_id): Path<Xid>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    let guard = state.sessions.checkout(txn_id, &current_user.0)?;
    let xid = guard.session.xid;
    let result = state.engine.commit(xid).await;
    if result.is_err() {
        // SerializationFailure has already rolled back inside Engine::commit;
        // for anything else this is a best-effort double-abort (harmless).
        let _ = state.engine.abort(xid).await;
    }
    state.sessions.remove(xid);
    drop(guard);
    result.map_err(ApiError::from)?;
    Ok(Json(json!({ "txn_id": txn_id, "state": "committed" })))
}

/// Roll a transaction session back and discard it.
pub async fn post_txn_rollback(
    Extension(current_user): Extension<CurrentUser>,
    State(state): State<AppState>,
    Path(txn_id): Path<Xid>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    let guard = state.sessions.checkout(txn_id, &current_user.0)?;
    let xid = guard.session.xid;
    let result = state.engine.abort(xid).await;
    state.sessions.remove(xid);
    drop(guard);
    result.map_err(ApiError::from)?;
    Ok(Json(json!({ "txn_id": txn_id, "state": "rolled_back" })))
}

// ── SQL / Cypher ─────────────────────────────────────────────────────────

/// Turn a statement's results into the response body: the ordinary
/// `{"results": [...]}` array, or — with `"cursor": true` (R4) — buffer the
/// single `rows` result server-side and return a `cursor_id` for paging.
fn sql_response(
    state: &AppState,
    principal: Option<String>,
    results: Vec<ExecResult>,
    cursor: bool,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    if !cursor {
        let json_results: Vec<_> = results.iter().map(exec_result_to_json).collect();
        return Ok(Json(json!({ "results": json_results })));
    }
    // Cursor mode: exactly one rows-producing statement.
    if results.len() != 1 {
        return Err(ApiError::bad_request(
            "CURSOR_NOT_ROWS",
            "cursor mode requires exactly one statement producing rows",
        ));
    }
    match results.into_iter().next() {
        Some(ExecResult::Rows { columns, rows }) => {
            let row_count = rows.len();
            let cursor_id = state.cursors.create(principal, columns.clone(), rows);
            Ok(Json(json!({
                "cursor_id": cursor_id,
                "columns": columns,
                "row_count": row_count,
            })))
        }
        _ => Err(ApiError::bad_request(
            "CURSOR_NOT_ROWS",
            "cursor mode requires a rows-producing statement (SELECT/query)",
        )),
    }
}

pub async fn post_sql(
    Extension(current_user): Extension<CurrentUser>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SqlRequest>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    let user = current_user.0;

    // Cursor mode is validated before anything executes (R4).
    if body.cursor {
        ensure_cursor_sql(&body.sql)?;
    }

    // ── session statement (R1) ──
    if let Some(txn_id) = parse_txn_header(&headers)? {
        if body.isolation.is_some() {
            return Err(ApiError::bad_request(
                "ISOLATION_IN_SESSION",
                "isolation is fixed at POST /txn/begin and cannot be set per-statement inside a session",
            ));
        }
        let guard = state.sessions.checkout(txn_id, &user)?;
        let xid = guard.session.xid;
        // Everything up to execution is a pre-check: a rejection here has
        // touched nothing, so the session stays open.
        ensure_session_sql_allowed(&body.sql)?;
        state
            .engine
            .authorize_sql(user.clone(), body.sql.clone())
            .await?;
        let result = if body.params.is_empty() {
            state.engine.execute_sql(xid, body.sql).await
        } else {
            let params: Vec<_> = body.params.iter().map(json_to_literal).collect();
            state.engine.execute_sql_params(xid, body.sql, params).await
        };
        let results = match result {
            Ok(results) => results,
            Err(e) => {
                // A failed statement may have left partial effects inside
                // the open transaction — abort it and destroy the session.
                let _ = state.engine.abort(xid).await;
                state.sessions.remove(xid);
                drop(guard);
                return Err(e.into());
            }
        };
        drop(guard);
        return sql_response(&state, user, results, body.cursor);
    }

    // ── one-shot statement ──
    // Auth DDL (CREATE USER / GRANT / REVOKE, P6.e) isn't `sqlparser`
    // grammar — route it through `execute_sql_as`, which intercepts it and
    // requires superuser.
    if crate::authz::parse_auth_stmt(&body.sql)
        .map_err(ApiError::from)?
        .is_some()
    {
        let xid = state.engine.begin(None).await?;
        let result = state
            .engine
            .execute_sql_as(user.clone(), xid, body.sql.clone())
            .await;
        let results = finish(&state.engine, xid, result).await?;
        return sql_response(&state, user, results, body.cursor);
    }

    // Enforce per-user privileges (a no-op for the superuser / `None`)
    // before the fast-path dispatch below runs the statement.
    state
        .engine
        .authorize_sql(user.clone(), body.sql.clone())
        .await
        .map_err(ApiError::from)?;

    let isolation = body.isolation.map(IsolationDto::to_engine);
    // Parameterized requests (P2.e) always execute with the values bound as
    // data — the injection-safe path.
    let results = if !body.params.is_empty() {
        let params: Vec<_> = body.params.iter().map(json_to_literal).collect();
        let xid = state.engine.begin(isolation).await?;
        let result = state.engine.execute_sql_params(xid, body.sql, params).await;
        finish(&state.engine, xid, result).await?
    } else if isolation.is_none() && crate::read_handle::is_concurrent_read_sql(&body.sql) {
        // Read-only SELECTs run on the concurrent read path (6b) — no
        // begin/commit round-trips. An explicit isolation request (R2)
        // deliberately takes the transactional path instead, so the chosen
        // level actually governs the statement.
        state.engine.execute_sql_read(body.sql).await?
    } else {
        let xid = state.engine.begin(isolation).await?;
        let result = state.engine.execute_sql(xid, body.sql).await;
        finish(&state.engine, xid, result).await?
    };
    sql_response(&state, user, results, body.cursor)
}

pub async fn post_cypher(
    Extension(current_user): Extension<CurrentUser>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CypherRequest>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    let (xid, scope) = begin_scoped(&state, &headers, &current_user.0).await?;
    let result = state.engine.execute_cypher(xid, body.query).await;
    let results = finish_scoped(&state, xid, scope, result).await?;
    let json_results: Vec<_> = results.iter().map(exec_result_to_json).collect();
    Ok(Json(json!({ "results": json_results })))
}

// ── SQL result cursors (R4) ──────────────────────────────────────────────

/// Fetch the next page of a cursor opened by `POST /sql` with
/// `"cursor": true`. The final page reports `"done": true` and the cursor
/// is dropped; fetching it again is `404 CURSOR_NOT_FOUND`.
pub async fn get_sql_cursor(
    Extension(current_user): Extension<CurrentUser>,
    State(state): State<AppState>,
    Path(cursor_id): Path<u64>,
    Query(q): Query<CursorQuery>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    let limit = q.limit.clamp(1, 10_000);
    let page = state.cursors.fetch(cursor_id, &current_user.0, limit)?;
    let rows: Vec<serde_json::Value> = page
        .rows
        .iter()
        .map(|row| serde_json::Value::Array(row.iter().map(literal_to_json).collect()))
        .collect();
    Ok(Json(json!({
        "columns": page.columns,
        "rows": rows,
        "done": page.done,
        "remaining": page.remaining,
    })))
}

/// Drop a cursor before exhausting it.
pub async fn delete_sql_cursor(
    Extension(current_user): Extension<CurrentUser>,
    State(state): State<AppState>,
    Path(cursor_id): Path<u64>,
) -> std::result::Result<StatusCode, ApiError> {
    state.cursors.remove(cursor_id, &current_user.0)?;
    Ok(StatusCode::NO_CONTENT)
}

// ── raw CRUD ─────────────────────────────────────────────────────────────

pub async fn post_row(
    Extension(current_user): Extension<CurrentUser>,
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> std::result::Result<(StatusCode, Json<RowIdResponse>), ApiError> {
    let (xid, scope) = begin_scoped(&state, &headers, &current_user.0).await?;
    let result = state.engine.insert(xid, body.to_vec()).await;
    let row_id = finish_scoped(&state, xid, scope, result).await?;
    Ok((StatusCode::CREATED, Json(RowIdResponse { row_id })))
}

/// Insert a bounded batch of raw rows atomically (R4): one transaction, N
/// inserts, all-or-nothing. Rows are base64-encoded (they are opaque bytes;
/// JSON cannot carry them verbatim). Session-aware like `POST /rows`.
pub async fn post_rows_batch(
    Extension(current_user): Extension<CurrentUser>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<BatchInsertRequest>,
) -> std::result::Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    const MAX_BATCH_ROWS: usize = 10_000;
    const MAX_BATCH_BYTES: usize = 32 << 20; // 32 MiB decoded

    if body.rows.is_empty() {
        return Err(ApiError::bad_request(
            "EMPTY_BATCH",
            "rows must be non-empty",
        ));
    }
    if body.rows.len() > MAX_BATCH_ROWS {
        return Err(ApiError::bad_request(
            "BATCH_TOO_LARGE",
            format!(
                "batch of {} rows exceeds the {MAX_BATCH_ROWS}-row bound",
                body.rows.len()
            ),
        ));
    }
    // Decode everything up front so a malformed entry rejects the whole
    // request before any insert runs (atomicity without a wasted abort).
    let mut decoded: Vec<Vec<u8>> = Vec::with_capacity(body.rows.len());
    let mut total = 0usize;
    for (i, encoded) in body.rows.iter().enumerate() {
        use base64::Engine as _;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|e| {
                ApiError::bad_request("BAD_BASE64", format!("rows[{i}] is not valid base64: {e}"))
            })?;
        total += bytes.len();
        if total > MAX_BATCH_BYTES {
            return Err(ApiError::bad_request(
                "BATCH_TOO_LARGE",
                format!("decoded batch exceeds the {MAX_BATCH_BYTES}-byte bound"),
            ));
        }
        decoded.push(bytes);
    }

    let (xid, scope) = begin_scoped(&state, &headers, &current_user.0).await?;
    let result: Result<Vec<RowId>> = async {
        let mut row_ids = Vec::with_capacity(decoded.len());
        for data in decoded {
            row_ids.push(state.engine.insert(xid, data).await?);
        }
        Ok(row_ids)
    }
    .await;
    let row_ids = finish_scoped(&state, xid, scope, result).await?;
    Ok((StatusCode::CREATED, Json(json!({ "row_ids": row_ids }))))
}

pub async fn get_row(
    Extension(current_user): Extension<CurrentUser>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((page_id, slot)): Path<(u32, u16)>,
) -> std::result::Result<Vec<u8>, ApiError> {
    let row_id = RowId { page_id, slot };
    match parse_txn_header(&headers)? {
        // Session read (R1): run under the session's xid so the transaction
        // sees its own uncommitted writes and an RR/serializable session
        // keeps its stable snapshot. A miss is a normal outcome — the
        // session stays open.
        Some(txn_id) => {
            let guard = state.sessions.checkout(txn_id, &current_user.0)?;
            let xid = guard.session.xid;
            state.engine.get(xid, row_id).await.map_err(ApiError::from)
        }
        // Concurrent read path (6b): off the write path entirely, no
        // begin/get/commit round-trips.
        None => state.engine.get_row(row_id).await.map_err(ApiError::from),
    }
}

pub async fn put_row(
    Extension(current_user): Extension<CurrentUser>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((page_id, slot)): Path<(u32, u16)>,
    body: Bytes,
) -> std::result::Result<Json<RowIdResponse>, ApiError> {
    let row_id = RowId { page_id, slot };
    let (xid, scope) = begin_scoped(&state, &headers, &current_user.0).await?;
    let result = state.engine.update(xid, row_id, body.to_vec()).await;
    let new_row_id = finish_scoped(&state, xid, scope, result).await?;
    Ok(Json(RowIdResponse { row_id: new_row_id }))
}

pub async fn delete_row(
    Extension(current_user): Extension<CurrentUser>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((page_id, slot)): Path<(u32, u16)>,
) -> std::result::Result<StatusCode, ApiError> {
    let row_id = RowId { page_id, slot };
    let (xid, scope) = begin_scoped(&state, &headers, &current_user.0).await?;
    let result = state.engine.delete(xid, row_id).await;
    finish_scoped(&state, xid, scope, result).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ── graph ────────────────────────────────────────────────────────────────

pub async fn post_edge(
    Extension(current_user): Extension<CurrentUser>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateEdgeRequest>,
) -> std::result::Result<(StatusCode, Json<RowIdResponse>), ApiError> {
    let (xid, scope) = begin_scoped(&state, &headers, &current_user.0).await?;
    let result = state
        .engine
        .create_edge(
            xid,
            body.from_id,
            body.to_id,
            body.edge_type,
            body.props.to_string(),
        )
        .await;
    let row_id = finish_scoped(&state, xid, scope, result).await?;
    Ok((StatusCode::CREATED, Json(RowIdResponse { row_id })))
}

pub async fn delete_edge(
    Extension(current_user): Extension<CurrentUser>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((page_id, slot)): Path<(u32, u16)>,
    Json(body): Json<DeleteEdgeRequest>,
) -> std::result::Result<StatusCode, ApiError> {
    let row_id = RowId { page_id, slot };
    let (xid, scope) = begin_scoped(&state, &headers, &current_user.0).await?;
    let result = state.engine.delete_edge(xid, row_id, body.from_id).await;
    finish_scoped(&state, xid, scope, result).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn get_edges_from(
    Extension(current_user): Extension<CurrentUser>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(from_id): Path<i64>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    match parse_txn_header(&headers)? {
        // Session read: same leniency as `get_row` — an error leaves the
        // session open (a traversal cannot leave partial write effects).
        Some(txn_id) => {
            let guard = state.sessions.checkout(txn_id, &current_user.0)?;
            let xid = guard.session.xid;
            let edges = state
                .engine
                .edges_from(xid, from_id)
                .await
                .map_err(ApiError::from)?;
            drop(guard);
            Ok(Json(json!({ "edges": edges })))
        }
        None => {
            let xid = state.engine.begin(None).await?;
            let result = state.engine.edges_from(xid, from_id).await;
            let edges = finish(&state.engine, xid, result).await?;
            Ok(Json(json!({ "edges": edges })))
        }
    }
}

// ── secondary indexing ───────────────────────────────────────────────────

pub async fn post_index(
    State(state): State<AppState>,
    Json(body): Json<SetIndexRequest>,
) -> std::result::Result<StatusCode, ApiError> {
    state
        .engine
        .set_column_index(body.table, body.column, body.kind)
        .await
        .map_err(ApiError::from)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn get_index_status(
    State(state): State<AppState>,
    Path((table, column)): Path<(String, String)>,
) -> Json<serde_json::Value> {
    let status = state.engine.index_status(table, column).await;
    Json(json!({ "status": status }))
}

// ── events ───────────────────────────────────────────────────────────────

/// Opt a table into event capture (`Engine::enable_events`) — no `xid`,
/// mirrors `post_index`'s shape (a catalog-only operation, not a
/// transaction). Needed before `GET /events/subscribe` or `POST
/// /events/ack` return anything meaningful for a given table.
pub async fn post_enable_events(
    State(state): State<AppState>,
    Path(table): Path<String>,
) -> std::result::Result<StatusCode, ApiError> {
    state
        .engine
        .enable_events(table)
        .await
        .map_err(ApiError::from)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn post_events_ack(
    State(state): State<AppState>,
    Json(body): Json<AckEventsRequest>,
) -> std::result::Result<StatusCode, ApiError> {
    let xid = state.engine.begin(None).await?;
    let result = state
        .engine
        .ack_events(xid, body.consumer, body.up_to_seq)
        .await;
    finish(&state.engine, xid, result).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Reclaim fully-acked events (R3): `Engine::vacuum_events` deletes every
/// `__events__` row already acknowledged by *all* registered consumers —
/// the M4 slow-consumer durability contract (an event outlives vacuum until
/// its slowest consumer has durably acked past it). Returns the reclaimed
/// count.
pub async fn post_events_vacuum(
    State(state): State<AppState>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    let xid = state.engine.begin(None).await?;
    let result = state.engine.vacuum_events(xid).await;
    let reclaimed = finish(&state.engine, xid, result).await?;
    Ok(Json(json!({ "reclaimed": reclaimed })))
}

// ── RLS (R3) ─────────────────────────────────────────────────────────────

/// Attach a row-level-security policy to a table, as a SQL predicate string
/// (`{"predicate": "tenant_id = 7"}`) — the policy is AND-rewritten into
/// every query on the table. Superuser-gated: RLS is an access-control
/// boundary, so letting any authenticated principal rewrite it would defeat
/// its purpose.
pub async fn put_table_rls(
    Extension(current_user): Extension<CurrentUser>,
    State(state): State<AppState>,
    Path(table): Path<String>,
    Json(body): Json<RlsRequest>,
) -> std::result::Result<StatusCode, ApiError> {
    state
        .engine
        .ensure_superuser(current_user.0.clone())
        .await
        .map_err(ApiError::from)?;
    state
        .engine
        .set_rls_policy_sql(table, body.predicate)
        .await
        .map_err(ApiError::from)?;
    Ok(StatusCode::NO_CONTENT)
}

// ── operational ──────────────────────────────────────────────────────────

pub async fn post_checkpoint(
    State(state): State<AppState>,
) -> std::result::Result<StatusCode, ApiError> {
    state.engine.checkpoint().await.map_err(ApiError::from)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Force the WAL durable and flush every dirty page (R3) — `Engine::flush`,
/// previously test-only. Superuser-gated admin surface (it is an I/O
/// amplification lever, not a data-plane operation).
pub async fn post_admin_flush(
    Extension(current_user): Extension<CurrentUser>,
    State(state): State<AppState>,
) -> std::result::Result<StatusCode, ApiError> {
    state
        .engine
        .ensure_superuser(current_user.0)
        .await
        .map_err(ApiError::from)?;
    state.engine.flush().await.map_err(ApiError::from)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /logs` (item 22, L3): a **superuser-gated**, bounded, cursor-paged tail
/// over the rotated JSON log files. Not a log database — a filtered reverse read
/// only (see `server::logs`). The file scan is blocking IO, so it runs on the
/// blocking pool; both the page size and the per-request scan are hard-capped so
/// a multi-GB log directory can neither OOM nor stall the server.
pub async fn get_logs(
    Extension(current_user): Extension<CurrentUser>,
    State(state): State<AppState>,
    Query(params): Query<crate::server::logs::LogQueryParams>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    state
        .engine
        .ensure_superuser(current_user.0)
        .await
        .map_err(ApiError::from)?;

    let query: crate::server::logs::LogQuery = params.into();
    let log_dir = state.log_dir.clone();
    let page =
        tokio::task::spawn_blocking(move || crate::server::logs::read_logs(&log_dir, &query))
            .await
            .map_err(|_| ApiError::internal("LOG_READ_TASK", "log read task panicked"))?
            .map_err(|e| ApiError::internal("LOG_READ", format!("failed to read logs: {e}")))?;

    Ok(Json(json!({
        "logs": page.logs,
        "returned": page.logs.len(),
        "scanned": page.scanned,
        "truncated": page.truncated,
        "next_cursor": page.next_cursor,
    })))
}

/// `pg_stat_*`-style activity view (P6.g): commits/aborts/checkpoints, active
/// sessions, WAL pressure, replication lag, and recent slow queries.
pub async fn get_stats(
    State(state): State<AppState>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    let stats = state.engine.stats().await.map_err(ApiError::from)?;
    let mut value = serde_json::to_value(stats).unwrap_or_default();
    // Server-layer gauges (R1/R4) alongside the engine counters.
    if let serde_json::Value::Object(map) = &mut value {
        map.insert("open_txn_sessions".into(), json!(state.sessions.len()));
        map.insert("open_cursors".into(), json!(state.cursors.len()));
        // item 21 server-session panel: abandoned-transaction reaper churn.
        map.insert(
            "idle_reaper_aborts".into(),
            json!(state.sessions.reaper_aborts()),
        );
    }
    Ok(Json(value))
}

/// Schema introspection (S1): list every **user** table with its columns.
/// Internal engine tables (`__events__`/`__edges__`/`__lobs__`/`__consumers__`)
/// are omitted. No row counts — a count is a full scan, deliberately out of
/// scope for v1 (see `docs/REST_API.md`). Auth-gated exactly like every other
/// data-plane route.
pub async fn get_tables(
    State(state): State<AppState>,
) -> std::result::Result<Json<Vec<TableInfo>>, ApiError> {
    let mut tables: Vec<TableInfo> = state
        .engine
        .table_defs()
        .await?
        .iter()
        .filter(|def| !is_internal_table(&def.name))
        .map(table_def_to_info)
        .collect();
    // `Catalog::tables` yields tables in `HashMap` order; sort by name so the
    // response is deterministic (stable for clients, tests, and diffs).
    tables.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Json(tables))
}

// ── replication (P6.b) ─────────────────────────────────────────────────────

pub async fn post_replication_slot(
    State(state): State<AppState>,
    Json(body): Json<CreateSlotRequest>,
) -> std::result::Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let kind = if body.sync {
        crate::replication::SlotKind::Sync
    } else {
        crate::replication::SlotKind::Async
    };
    let info = state
        .engine
        .create_replication_slot(body.name, kind)
        .await
        .map_err(ApiError::from)?;
    Ok((StatusCode::CREATED, Json(slot_to_json(&info))))
}

pub async fn get_replication_slots(
    State(state): State<AppState>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    let slots = state
        .engine
        .replication_slots()
        .await
        .map_err(ApiError::from)?;
    let arr: Vec<serde_json::Value> = slots.iter().map(slot_to_json).collect();
    Ok(Json(json!({ "slots": arr })))
}

pub async fn delete_replication_slot(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> std::result::Result<StatusCode, ApiError> {
    state
        .engine
        .drop_replication_slot(name)
        .await
        .map_err(ApiError::from)?;
    Ok(StatusCode::NO_CONTENT)
}

/// A replica confirms it has durably applied up to `lsn`; the slot advances and
/// the WAL past that point may be truncated at the next checkpoint.
pub async fn post_replication_slot_advance(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<AdvanceSlotRequest>,
) -> std::result::Result<StatusCode, ApiError> {
    state
        .engine
        .advance_replication_slot(name, body.lsn)
        .await
        .map_err(ApiError::from)?;
    Ok(StatusCode::NO_CONTENT)
}

/// WAL shipping: stream every record after `from_lsn` as framed bytes
/// (`application/octet-stream`). The primary's current tail LSN is returned in
/// the `x-unidb-tail-lsn` header so the replica knows where the batch ends.
pub async fn get_replication_stream(
    State(state): State<AppState>,
    Query(q): Query<StreamQuery>,
) -> std::result::Result<axum::response::Response, ApiError> {
    use axum::http::header;
    use axum::response::IntoResponse;

    let (tail, bytes) = state
        .engine
        .ship_wal(q.from_lsn)
        .await
        .map_err(ApiError::from)?;
    let mut resp = (StatusCode::OK, Bytes::from(bytes)).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/octet-stream"),
    );
    resp.headers_mut().insert(
        "x-unidb-tail-lsn",
        header::HeaderValue::from_str(&tail.to_string())
            .unwrap_or_else(|_| header::HeaderValue::from_static("0")),
    );
    Ok(resp)
}
