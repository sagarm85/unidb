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

use std::net::SocketAddr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::{
    body::Bytes,
    extract::{ConnectInfo, Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
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
            table_def_to_info, AckEventsRequest, AdvanceSlotRequest, AuthEmailFlowAck,
            AuthLoginOutcome, AuthLoginRequest, AuthLoginResponse, AuthMagicLinkRequest,
            AuthMagicLinkVerifyRequest, AuthMetaResponse, AuthPreviewRequest, AuthRecoverRequest,
            AuthVerifyRequest, BatchInsertRequest, BatchSqlRequest, BatchSqlResponse,
            BeginTxnRequest, ChannelPolicyDeleteRequest, ChannelPolicyDto,
            ChannelPolicyUpsertRequest, CreateEdgeRequest, CreateSlotRequest, CursorQuery,
            CypherRequest, DeleteEdgeRequest, GroupCommitWindowRequest, HistoryQuery, IsolationDto,
            MfaChallengeRequest, MfaDisableRequest, MfaEnrollResponse, MfaVerifyRequest,
            MfaVerifyResponse, OAuthCallbackQuery, RlsRequest, RowIdResponse, SetIndexRequest,
            SlowQueryThresholdRequest, SqlRequest, StreamQuery, TableInfo, WhoamiPrivilege,
            WhoamiResponse,
        },
        engine_handle::EngineHandle,
        error::ApiError,
        oauth::OAuthError,
        txn_session::SessionGuard,
        AppState,
    },
    sql::executor::ExecResult,
};

/// Commit `xid` on `Ok`, abort it on `Err` — the one piece of boilerplate
/// every one-shot mutating handler shares. `pub(crate)` so `rest_resource.rs`
/// (item 123, C1) reuses the exact same commit-or-abort contract instead of
/// re-implementing it.
pub(crate) async fn finish<T>(engine: &EngineHandle, xid: Xid, result: Result<T>) -> Result<T> {
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
    Extension(principal): Extension<crate::AuthPrincipal>,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SqlRequest>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    let user = current_user.0;
    debug_assert_eq!(
        principal.subject, user,
        "CurrentUser and AuthPrincipal must carry the same subject (both come from require_jwt)"
    );

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
        // item 122, B3: principal-aware pre-check so a service_role token
        // isn't rejected here before the engine's own audited bypass runs.
        state
            .engine
            .authorize_sql_as_principal(principal.clone(), body.sql.clone())
            .await?;
        // Use execute_sql_as_principal (auth seam) to carry the full
        // principal so the RLS bypass for superusers and the no-sub path
        // applies correctly (item 103); claims/roles ride along inert.
        let result = if body.params.is_empty() {
            state
                .engine
                .execute_sql_as_principal(principal.clone(), xid, body.sql)
                .await
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
    // grammar — route it through `execute_sql_as_principal`, which
    // intercepts it and requires superuser.
    if crate::authz::parse_auth_stmt(&body.sql)
        .map_err(ApiError::from)?
        .is_some()
    {
        let xid = state.engine.begin(None).await?;
        let result = state
            .engine
            .execute_sql_as_principal(principal.clone(), xid, body.sql.clone())
            .await;
        let results = finish(&state.engine, xid, result).await?;
        return sql_response(&state, user, results, body.cursor);
    }

    // Enforce per-user privileges (a no-op for the superuser / `None`, and
    // for a service_role token — item 122, B3) before the fast-path
    // dispatch below runs the statement.
    state
        .engine
        .authorize_sql_as_principal(principal.clone(), body.sql.clone())
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
        // Pass the full principal (not just the user identity) so RLS is
        // correctly applied / bypassed for superusers and the no-sub path
        // (item 103), and so auth.uid()/auth.jwt() policies (item 122) see
        // the same verified claims here as on the writer path.
        state
            .engine
            .execute_sql_read_as_principal(principal.clone(), body.sql)
            .await?
    } else {
        // Use execute_sql_as_principal so the RLS bypass logic for
        // superusers/no-sub callers is applied correctly on the writer path
        // (item 103); claims/roles ride along inert (auth seam).
        let xid = state.engine.begin(isolation).await?;
        let result = state
            .engine
            .execute_sql_as_principal(principal.clone(), xid, body.sql)
            .await;
        finish(&state.engine, xid, result).await?
    };
    sql_response(&state, user, results, body.cursor)
}

/// `POST /batch-sql` (item 99): execute up to 256 independent one-shot SQL
/// statements in a single HTTP round-trip.  Each statement is auto-committed
/// independently — there is **no** shared transaction across the batch.
///
/// Error handling is governed by `stop_on_error`:
/// - `false` (default): all statements are attempted; failed slots get
///   `null` result + error string, others get their normal result.
/// - `true`: stop at the first error; remaining slots get `null` result +
///   `"skipped"` error string.
///
/// The response is always `200 OK`; per-statement failures are reported
/// inside the payload, not via the HTTP status code.
pub async fn post_batch_sql(
    Extension(current_user): Extension<CurrentUser>,
    Extension(principal): Extension<crate::AuthPrincipal>,
    State(state): State<AppState>,
    Json(body): Json<BatchSqlRequest>,
) -> std::result::Result<Json<BatchSqlResponse>, ApiError> {
    const MAX_BATCH_STMTS: usize = 256;

    let user = current_user.0;

    if body.statements.len() > MAX_BATCH_STMTS {
        return Err(ApiError::bad_request(
            "BATCH_TOO_LARGE",
            format!(
                "batch of {} statements exceeds the {MAX_BATCH_STMTS}-statement limit",
                body.statements.len()
            ),
        ));
    }

    let n = body.statements.len();
    let mut results: Vec<Option<serde_json::Value>> = Vec::with_capacity(n);
    let mut errors: Vec<Option<String>> = Vec::with_capacity(n);
    let mut stopped = false;

    for sql in body.statements {
        if stopped {
            results.push(None);
            errors.push(Some("skipped".to_string()));
            continue;
        }

        // Auth DDL (CREATE USER / GRANT / REVOKE) is routed via
        // `execute_sql_as`, which requires superuser — same gate as `post_sql`.
        let stmt_result: std::result::Result<Vec<crate::sql::executor::ExecResult>, ApiError> =
            if crate::authz::parse_auth_stmt(&sql)
                .map_err(ApiError::from)?
                .is_some()
            {
                let xid = state.engine.begin(None).await?;
                let result = state
                    .engine
                    .execute_sql_as(user.clone(), xid, sql.clone())
                    .await;
                finish(&state.engine, xid, result)
                    .await
                    .map_err(ApiError::from)
            } else {
                // Enforce per-user privileges before dispatch.
                if let Err(e) = state.engine.authorize_sql(user.clone(), sql.clone()).await {
                    Err(ApiError::from(e))
                } else if crate::read_handle::is_concurrent_read_sql(&sql) {
                    // Read-only SELECTs use the concurrent read path. Pass the
                    // full principal so RLS bypass applies correctly for
                    // superusers and the no-sub path (item 103), and so
                    // auth.uid()/auth.jwt() policies (item 122) see the same
                    // verified claims here as on the writer path.
                    state
                        .engine
                        .execute_sql_read_as_principal(principal.clone(), sql.clone())
                        .await
                        .map_err(ApiError::from)
                } else {
                    // Use execute_sql_as to carry user identity for RLS bypass
                    // (item 103).
                    let xid = state.engine.begin(None).await?;
                    let result = state
                        .engine
                        .execute_sql_as(user.clone(), xid, sql.clone())
                        .await;
                    finish(&state.engine, xid, result)
                        .await
                        .map_err(ApiError::from)
                }
            };

        match stmt_result {
            Ok(exec_results) => {
                // Batch SQL always produces a single-statement result per
                // slot — the engine's `execute_sql` parses the SQL and runs
                // it, returning one `ExecResult` per parsed statement.  For
                // the single-SQL-string-per-slot contract here, there is
                // exactly one entry.  We flatten multiple results the same
                // way `POST /sql`'s `sql_response` does: wrap as an array
                // and return the first element when there is exactly one, or
                // return the whole array for the rare multi-stmt slot.
                let json_results: Vec<serde_json::Value> =
                    exec_results.iter().map(exec_result_to_json).collect();
                let slot_value = if json_results.len() == 1 {
                    json_results
                        .into_iter()
                        .next()
                        .unwrap_or(serde_json::Value::Null)
                } else {
                    serde_json::Value::Array(json_results)
                };
                results.push(Some(slot_value));
                errors.push(None);
            }
            Err(e) => {
                let msg = match &e {
                    ApiError::Db(db_err) => db_err.to_string(),
                    ApiError::Api { message, .. } => message.clone(),
                };
                results.push(None);
                errors.push(Some(msg));
                if body.stop_on_error {
                    stopped = true;
                }
            }
        }
    }

    Ok(Json(BatchSqlResponse { results, errors }))
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

/// Query CDC status for a table (item 33). Returns `{ "enabled": bool }`.
/// `404 TABLE_NOT_FOUND` if the table does not exist.
pub async fn get_table_events_status(
    State(state): State<AppState>,
    Path(table): Path<String>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    let enabled = state
        .engine
        .is_events_enabled(table)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(json!({ "enabled": enabled })))
}

/// Disable CDC on a table (item 33). Idempotent — returns `204` even when
/// CDC was already off. Already-captured events remain in `__events__` until
/// consumed and vacuumed; only future writes stop emitting.
pub async fn delete_table_events(
    State(state): State<AppState>,
    Path(table): Path<String>,
) -> std::result::Result<StatusCode, ApiError> {
    state
        .engine
        .disable_events(table)
        .await
        .map_err(ApiError::from)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Return the current highest committed `seq` in `__events__` (item 33).
/// Returns `{ "seq": 0 }` if no events have ever been written.
/// Useful for "start from now" positioning without opening an SSE stream.
pub async fn get_events_head(
    State(state): State<AppState>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    let seq = state
        .engine
        .events_head_seq()
        .await
        .map_err(ApiError::from)?;
    Ok(Json(json!({ "seq": seq })))
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

// ── Realtime channel authorization (item 140) ──────────────────────────────

fn parse_channel_op(raw: &str) -> std::result::Result<crate::authz::ChannelOp, ApiError> {
    crate::authz::ChannelOp::parse(raw).ok_or_else(|| {
        ApiError::bad_request(
            "INVALID_OPERATION",
            format!("operation must be one of publish|subscribe|presence|all, got '{raw}'"),
        )
    })
}

/// `PUT /realtime/policies` (item 140 admin surface): upsert a
/// channel-authorization policy `{topic_pattern, operation, roles}`.
/// Superuser-gated, the same posture as [`put_table_rls`] — topics aren't
/// SQL objects, but the access-control stakes are the same.
pub async fn put_realtime_policy(
    Extension(current_user): Extension<CurrentUser>,
    State(state): State<AppState>,
    Json(body): Json<ChannelPolicyUpsertRequest>,
) -> std::result::Result<StatusCode, ApiError> {
    state
        .engine
        .ensure_superuser(current_user.0.clone())
        .await
        .map_err(ApiError::from)?;
    let operation = parse_channel_op(&body.operation)?;
    let roles: std::collections::BTreeSet<String> = body.roles.into_iter().collect();
    state
        .engine
        .set_channel_policy(body.topic_pattern, operation, roles)
        .await
        .map_err(ApiError::from)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /realtime/policies` (item 140 admin surface): remove a
/// channel-authorization policy `{topic_pattern, operation}`. Idempotent —
/// removing an unknown `(topic_pattern, operation)` pair is a no-op, not an
/// error (mirrors `RoleStore::remove_channel_policy`'s posture).
pub async fn delete_realtime_policy(
    Extension(current_user): Extension<CurrentUser>,
    State(state): State<AppState>,
    Json(body): Json<ChannelPolicyDeleteRequest>,
) -> std::result::Result<StatusCode, ApiError> {
    state
        .engine
        .ensure_superuser(current_user.0.clone())
        .await
        .map_err(ApiError::from)?;
    let operation = parse_channel_op(&body.operation)?;
    state
        .engine
        .remove_channel_policy(body.topic_pattern, operation)
        .await
        .map_err(ApiError::from)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /realtime/policies` (item 140 admin surface): list every stored
/// channel-authorization policy. Superuser-gated (same as the write routes
/// above) — the policy set is access-control configuration, not something
/// every authenticated caller should be able to enumerate.
pub async fn get_realtime_policies(
    Extension(current_user): Extension<CurrentUser>,
    State(state): State<AppState>,
) -> std::result::Result<Json<Vec<ChannelPolicyDto>>, ApiError> {
    state
        .engine
        .ensure_superuser(current_user.0.clone())
        .await
        .map_err(ApiError::from)?;
    let policies = state
        .engine
        .list_channel_policies()
        .await
        .map_err(ApiError::from)?;
    Ok(Json(
        policies
            .into_iter()
            .map(|p| ChannelPolicyDto {
                topic_pattern: p.topic_pattern,
                operation: p.operation.as_str().to_string(),
                roles: p.roles.into_iter().collect(),
            })
            .collect(),
    ))
}

/// `POST /auth/preview` (item-24 Z6): run `sql` as `as_role` and return the
/// row-filtered result, including RLS policies that reference `current_user()`.
/// This is the "preview as user" surface: an admin can verify exactly what a
/// given role sees without logging in as that role.
///
/// **Superuser-gated**: only a superuser may impersonate another role.
/// Returns `403 FORBIDDEN` for a non-superuser caller when the engine has
/// at least one user configured (bootstrap mode always passes, same gate as
/// other admin routes).
pub async fn post_auth_preview(
    Extension(current_user): Extension<CurrentUser>,
    State(state): State<AppState>,
    Json(body): Json<AuthPreviewRequest>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    // Superuser-only gate: only superusers may impersonate another role.
    state
        .engine
        .ensure_superuser(current_user.0)
        .await
        .map_err(ApiError::from)?;

    let xid = state.engine.begin(None).await?;
    let result = state
        .engine
        .execute_sql_as(Some(body.as_role.clone()), xid, body.sql.clone())
        .await;
    let results = finish(&state.engine, xid, result).await?;
    let first = results.into_iter().next().unwrap_or(ExecResult::Rows {
        columns: vec![],
        rows: vec![],
    });
    Ok(Json(exec_result_to_json(&first)))
}

// ── auth: meta / login / whoami (item 100) ────────────────────────────────

/// `GET /auth/meta` — public, no JWT required.
///
/// Returns static metadata about this server's auth configuration:
/// - `open_mode` — whether RLS is inactive (no users registered yet).
/// - `privilege_types` / `policy_operations` — the enumerated constants a
///   client needs to display a permission editor without hard-coding them.
/// - `catalog_tables` — all queryable system-catalog relations.
/// - `dev_login_enabled` — whether `POST /auth/login` is available.
/// - `signup_enabled` — whether `POST /auth/signup` is available (item 121 A3).
///
/// This endpoint is the answer to the blank-slate UX gap: a client hitting
/// a fresh server can call this first to discover what auth features are
/// active and how to configure them.
pub async fn get_auth_meta(
    State(state): State<AppState>,
) -> std::result::Result<Json<AuthMetaResponse>, ApiError> {
    let open_mode = !state.engine.has_users().await;
    Ok(Json(AuthMetaResponse {
        open_mode,
        privilege_types: vec!["SELECT", "INSERT", "UPDATE", "DELETE"],
        policy_operations: vec!["SELECT", "INSERT", "UPDATE", "DELETE", "ALL"],
        catalog_tables: crate::sql::information_schema::RELATIONS.to_vec(),
        dev_login_enabled: state.dev_login_jwt.is_some(),
        signup_enabled: state.allow_signup,
    }))
}

/// A route disabled behind a policy flag (dev-login, signup) returns 404 —
/// indistinguishable from a non-existent route — rather than a 4xx that
/// hints the route exists but is merely gated. Shared by `POST /auth/signup`
/// (item 121 A3); `POST /auth/login`'s pre-existing disabled path predates
/// this helper and keeps its own historical (400) error, documented there.
fn route_disabled(message: impl Into<String>) -> ApiError {
    ApiError::Api {
        status: StatusCode::NOT_FOUND,
        code: "NOT_FOUND",
        message: message.into(),
    }
}

/// Issue a fresh access+refresh token pair for `username` (item 121, A2/A3/
/// A4 shared helper): the 1 h HS256 access token via `jwt_cfg`, plus a new
/// refresh-token session via [`EngineHandle::create_session`]. Used by
/// login, signup, and (for the access-token half only) refresh.
async fn issue_token_pair(
    engine: &EngineHandle,
    jwt_cfg: &crate::server::auth::JwtConfig,
    username: &str,
) -> std::result::Result<AuthLoginResponse, ApiError> {
    let access_token = jwt_cfg.issue_token(username).map_err(|e| {
        ApiError::from(crate::error::DbError::SqlPlan(format!(
            "token issuance failed: {e}"
        )))
    })?;
    let (refresh_token, _expires_at) = engine
        .create_session(username.to_string())
        .await
        .map_err(ApiError::from)?;
    Ok(AuthLoginResponse {
        token: access_token.clone(),
        access_token,
        refresh_token,
        expires_in: 3600,
    })
}

/// `POST /auth/login` — public, no JWT required.
///
/// Requires a signing key to be configured at startup — `UNIDB_JWT_SIGNING_KEY`
/// (item 121 A5, the first-class production issuer path) or `UNIDB_DEV_LOGIN=1`
/// (pre-A5, kept for back-compat); see `src/server/auth.rs`'s module doc for
/// the exact precedence. Neither configured ⇒ the existing disabled error.
///
/// **Item 121 A2 — real authentication.** The supplied password is verified
/// against the user's stored argon2id credential
/// ([`crate::authz::RoleStore::verify_password`]). Wrong password, an
/// unknown username, and a known username with no stored credential all
/// return the identical 401 response — there is no way for a caller to
/// distinguish "no such user" from "wrong password" from timing, status
/// code, or body shape (no user-enumeration oracle). On success, issues a
/// 1 h signed JWT using the same HS256 secret as `require_jwt`, so
/// `current_user()` and all privilege checks work immediately without any
/// downstream change.
///
/// **Item 121 A4** — the response now also carries a long-lived opaque
/// refresh token (a new session, [`EngineHandle::create_session`]); `token`
/// is kept as a deprecated alias of `access_token` for pre-A4 clients.
///
/// **Item 127 (Workstream D4) — MFA gate.** When the authenticating user has
/// TOTP MFA enabled ([`EngineHandle::mfa_enabled`]), a correct password does
/// **not** issue a session: instead the response is `{"mfa_required": true,
/// "challenge": "...", "expires_in": ...}` — a short-lived, single-use
/// challenge token (never a real access/refresh token) that must be redeemed
/// with a valid TOTP/recovery code at `POST /auth/mfa/challenge` before a
/// session is minted. A user *without* MFA enabled logs in exactly as
/// before (this branch is a no-op for them) — back-compat is unconditional.
pub async fn post_auth_login(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(body): Json<AuthLoginRequest>,
) -> std::result::Result<Json<AuthLoginOutcome>, ApiError> {
    let jwt_cfg = state.dev_login_jwt.as_ref().ok_or_else(|| {
        ApiError::from(crate::error::DbError::SqlPlan(
            "POST /auth/login is disabled (set UNIDB_JWT_SIGNING_KEY or UNIDB_DEV_LOGIN=1 to enable)".into(),
        ))
    })?;
    // Item 131 (Workstream I2) — CAPTCHA gate, before any credential work
    // (see `server::captcha`'s module doc). A no-op unless
    // `UNIDB_CAPTCHA_PROTECT` includes "login"; the client's peer IP is
    // passed through as informational context for the provider.
    state
        .captcha
        .enforce(
            &state.engine,
            "login",
            body.captcha_token.as_deref(),
            Some(&addr.ip().to_string()),
        )
        .await?;
    // Real credential verification (item 121 A2): unknown user, no stored
    // credential, and a genuine mismatch all take this same branch and
    // return this same 401 — see `RoleStore::verify_password`'s doc comment
    // for the constant-shape guarantee that makes this a real defense
    // against user enumeration, not just a same-looking error string.
    if !state
        .engine
        .verify_password(body.username.clone(), body.password.clone())
        .await
    {
        return Err(ApiError::Api {
            status: StatusCode::UNAUTHORIZED,
            code: "INVALID_CREDENTIALS",
            message: "invalid username or password".into(),
        });
    }
    if state.engine.mfa_enabled(body.username.clone()).await {
        let (challenge, _expires_at) = state
            .engine
            .create_mfa_challenge(body.username.clone())
            .await
            .map_err(ApiError::from)?;
        return Ok(Json(AuthLoginOutcome::MfaRequired {
            challenge,
            expires_in: crate::authz::MFA_CHALLENGE_TTL_SECS,
        }));
    }
    let resp = issue_token_pair(&state.engine, jwt_cfg, &body.username).await?;
    Ok(Json(AuthLoginOutcome::Session(resp)))
}

/// `POST /auth/mfa/challenge` — public, no JWT required (the challenge token
/// itself is the pre-session credential, exactly like `POST /auth/refresh`'s
/// refresh token). Rate-limited alongside `/auth/login`/`/auth/signup`/
/// `/auth/refresh` (item 121 I1) — see `router.rs`.
///
/// Redeems the MFA challenge issued by `POST /auth/login` plus a live
/// 6-digit TOTP code (or a one-time recovery code) for a full session —
/// reuses the identical [`issue_token_pair`] helper every other login path
/// uses, so the minted session is indistinguishable from a non-MFA login's.
/// Wrong code, expired/used/unknown challenge, and MFA having been disabled
/// out from under the challenge all return the identical
/// `401 MFA_CHALLENGE_INVALID` — no oracle on *why* it failed.
pub async fn post_mfa_challenge(
    State(state): State<AppState>,
    Json(body): Json<MfaChallengeRequest>,
) -> std::result::Result<Json<AuthLoginResponse>, ApiError> {
    let jwt_cfg = state.dev_login_jwt.as_ref().ok_or_else(|| {
        ApiError::from(crate::error::DbError::SqlPlan(
            "POST /auth/mfa/challenge cannot issue tokens (set UNIDB_JWT_SIGNING_KEY or UNIDB_DEV_LOGIN=1 to enable a signing key)"
                .into(),
        ))
    })?;
    let username = state
        .engine
        .verify_mfa_challenge(body.challenge.clone(), body.code.clone())
        .await
        .map_err(ApiError::from)?;
    let Some(username) = username else {
        return Err(ApiError::Api {
            status: StatusCode::UNAUTHORIZED,
            code: "MFA_CHALLENGE_INVALID",
            message: "invalid, expired, used, or wrong-code MFA challenge".into(),
        });
    };
    let resp = issue_token_pair(&state.engine, jwt_cfg, &username).await?;
    Ok(Json(resp))
}

// ── item 138: email transport + password-reset / magic-link flows ─────────

/// `POST /auth/recover` — public, no JWT required (item 138).
///
/// Request a password-reset email for `email`. **Always returns `200`**
/// (`{"ok": true}`, [`AuthEmailFlowAck`]) regardless of whether `email`
/// matches a registered account — this is the entire no-account-
/// enumeration contract for this route: `email` is the only signal a
/// caller ever gets, never the HTTP status, body shape, or timing.
///
/// **Note on `email`:** this milestone has no separate `users.email`
/// column — `email` is looked up directly as a username (see
/// `server::email`'s module doc for the full design-decision writeup and
/// the tracked follow-up). When it matches a registered account
/// ([`EngineHandle::user_exists`]), a single-use,
/// [`crate::authz::RECOVERY_TOKEN_TTL_SECS`]-second-TTL recovery token is
/// minted ([`EngineHandle::create_recovery_token`]) and emailed via the
/// configured [`crate::server::email::EmailConfig`]
/// ([`crate::server::email::EmailConfig::send_recovery_email`]). Both the
/// mint step and the send step are best-effort from the caller's point of
/// view — a persistence hiccup or a transport failure is logged
/// server-side (`tracing::warn!`) but never changes this route's `200`
/// response, so a delivery failure can never become a distinguishable
/// oracle either.
///
/// Rate-limited (item 121 I1) + CAPTCHA-eligible (item 131 I2, endpoint
/// name `"recover"`) exactly like `POST /auth/login`/`/auth/signup`.
pub async fn post_auth_recover(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(body): Json<AuthRecoverRequest>,
) -> std::result::Result<Json<AuthEmailFlowAck>, ApiError> {
    state
        .captcha
        .enforce(
            &state.engine,
            "recover",
            body.captcha_token.as_deref(),
            Some(&addr.ip().to_string()),
        )
        .await?;
    if state.engine.user_exists(body.email.clone()).await {
        match state.engine.create_recovery_token(body.email.clone()).await {
            Ok((raw_token, _expires_at)) => {
                if let Err(e) = state
                    .email
                    .send_recovery_email(&state.engine, &body.email, &body.email, &raw_token)
                    .await
                {
                    tracing::warn!(
                        error = %e,
                        "POST /auth/recover: failed to send the recovery email (still \
                         returning 200 — no account-enumeration oracle)"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "POST /auth/recover: failed to mint a recovery token (still returning 200)"
                );
            }
        }
    }
    Ok(Json(AuthEmailFlowAck { ok: true }))
}

/// `POST /auth/verify` — public, no JWT required (item 138; the recovery
/// token itself is the pre-session credential, same posture as `POST
/// /auth/mfa/challenge`'s challenge token).
///
/// Redeem a password-recovery token (from the emailed link) for a new
/// password: validates + single-use-consumes `token`
/// ([`EngineHandle::verify_recovery_token`]), sets the new argon2id
/// credential via the existing `set_password` path
/// ([`EngineHandle::set_password`] — item 121 A1, the same machinery
/// `ALTER USER ... PASSWORD` uses), and revokes every existing session for
/// that user ([`EngineHandle::revoke_all_sessions_for_user`]) so a session
/// obtained with the *old* password stops working immediately. Unknown/
/// expired/already-used token maps to a uniform `401
/// RECOVERY_TOKEN_INVALID` — no oracle on *why* it failed, same posture as
/// the OAuth callback's state validation / `POST /auth/mfa/challenge`.
pub async fn post_auth_verify(
    State(state): State<AppState>,
    Json(body): Json<AuthVerifyRequest>,
) -> std::result::Result<Json<AuthEmailFlowAck>, ApiError> {
    let username = state
        .engine
        .verify_recovery_token(body.token.clone())
        .await
        .map_err(ApiError::from)?;
    let Some(username) = username else {
        return Err(ApiError::Api {
            status: StatusCode::UNAUTHORIZED,
            code: "RECOVERY_TOKEN_INVALID",
            message: "invalid, expired, or already-used recovery token".into(),
        });
    };
    state
        .engine
        .set_password(username.clone(), body.new_password.clone())
        .await
        .map_err(ApiError::from)?;
    state
        .engine
        .revoke_all_sessions_for_user(username)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(AuthEmailFlowAck { ok: true }))
}

/// `POST /auth/magiclink` — public, no JWT required (item 138).
///
/// Request a magic sign-in link email for `email`. Same always-`200`,
/// no-account-enumeration contract as [`post_auth_recover`] (see that
/// handler's doc comment, including the "`email` is looked up as a
/// username" note) — the only differences are the token's shorter TTL
/// ([`crate::authz::MAGICLINK_TOKEN_TTL_SECS`]) and its own independent
/// token space ([`EngineHandle::create_magiclink_token`]).
pub async fn post_auth_magiclink(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(body): Json<AuthMagicLinkRequest>,
) -> std::result::Result<Json<AuthEmailFlowAck>, ApiError> {
    state
        .captcha
        .enforce(
            &state.engine,
            "magiclink",
            body.captcha_token.as_deref(),
            Some(&addr.ip().to_string()),
        )
        .await?;
    if state.engine.user_exists(body.email.clone()).await {
        match state
            .engine
            .create_magiclink_token(body.email.clone())
            .await
        {
            Ok((raw_token, _expires_at)) => {
                if let Err(e) = state
                    .email
                    .send_magiclink_email(&state.engine, &body.email, &body.email, &raw_token)
                    .await
                {
                    tracing::warn!(
                        error = %e,
                        "POST /auth/magiclink: failed to send the magic-link email (still \
                         returning 200 — no account-enumeration oracle)"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "POST /auth/magiclink: failed to mint a magic-link token (still returning 200)"
                );
            }
        }
    }
    Ok(Json(AuthEmailFlowAck { ok: true }))
}

/// `POST /auth/magiclink/verify` — public, no JWT required (item 138; the
/// magic-link token itself is the pre-session credential, same posture as
/// `POST /auth/mfa/challenge`'s challenge token / the OAuth callback's
/// `code`).
///
/// Redeems a magic-link login token for a real session via the exact same
/// [`issue_token_pair`] helper every other login path (password, signup,
/// MFA challenge, OAuth callback) uses. Unknown/expired/already-used token
/// maps to a uniform `401 MAGICLINK_TOKEN_INVALID`.
pub async fn post_auth_magiclink_verify(
    State(state): State<AppState>,
    Json(body): Json<AuthMagicLinkVerifyRequest>,
) -> std::result::Result<Json<AuthLoginResponse>, ApiError> {
    let jwt_cfg = state.dev_login_jwt.as_ref().ok_or_else(|| {
        ApiError::from(crate::error::DbError::SqlPlan(
            "POST /auth/magiclink/verify cannot issue tokens (set UNIDB_JWT_SIGNING_KEY or UNIDB_DEV_LOGIN=1 to enable a signing key)"
                .into(),
        ))
    })?;
    let username = state
        .engine
        .verify_magiclink_token(body.token.clone())
        .await
        .map_err(ApiError::from)?;
    let Some(username) = username else {
        return Err(ApiError::Api {
            status: StatusCode::UNAUTHORIZED,
            code: "MAGICLINK_TOKEN_INVALID",
            message: "invalid, expired, or already-used magic-link token".into(),
        });
    };
    let resp = issue_token_pair(&state.engine, jwt_cfg, &username).await?;
    Ok(Json(resp))
}

// ── item 127 (Workstream D4): TOTP MFA enroll/verify/disable ───────────────

/// The implicit superuser (a bearer token without a `sub` claim) has no
/// named-user identity for MFA state to attach to — shared guard for every
/// `/auth/mfa/*` handler below.
fn require_named_user(current_user: &CurrentUser) -> std::result::Result<String, ApiError> {
    current_user.0.clone().ok_or_else(|| ApiError::Api {
        status: StatusCode::BAD_REQUEST,
        code: "MFA_REQUIRES_NAMED_USER",
        message:
            "MFA requires a named user (a token with a `sub` claim), not the implicit superuser"
                .into(),
    })
}

/// `POST /auth/mfa/enroll` — protected (JWT required).
///
/// Generates a fresh, per-user TOTP secret and stores it **pending** — MFA
/// is not enabled yet. Returns the base32 secret plus an `otpauth://` URI
/// for the client to render as a QR code. Confirm with `POST
/// /auth/mfa/verify` to actually enable MFA on the account.
pub async fn post_mfa_enroll(
    Extension(current_user): Extension<CurrentUser>,
    State(state): State<AppState>,
) -> std::result::Result<Json<MfaEnrollResponse>, ApiError> {
    let user = require_named_user(&current_user)?;
    let (secret, otpauth_url) = state
        .engine
        .mfa_enroll(user)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(MfaEnrollResponse {
        secret,
        otpauth_url,
    }))
}

/// `POST /auth/mfa/verify` — protected (JWT required).
///
/// Confirms a pending enrollment with a live 6-digit code. On success, MFA
/// flips to enabled and a batch of one-time recovery codes is returned —
/// **shown exactly once**; only their hashes are ever persisted (item
/// 127, mirroring how refresh-token sessions store only a hash — see
/// `authz::RoleStore::mfa_confirm`). A wrong/malformed/replayed code returns
/// a uniform `401 MFA_INVALID_CODE`.
pub async fn post_mfa_verify(
    Extension(current_user): Extension<CurrentUser>,
    State(state): State<AppState>,
    Json(body): Json<MfaVerifyRequest>,
) -> std::result::Result<Json<MfaVerifyResponse>, ApiError> {
    let user = require_named_user(&current_user)?;
    let recovery_codes = state
        .engine
        .mfa_confirm(user, body.code)
        .await
        .map_err(ApiError::from)?;
    let Some(recovery_codes) = recovery_codes else {
        return Err(ApiError::Api {
            status: StatusCode::UNAUTHORIZED,
            code: "MFA_INVALID_CODE",
            message: "invalid MFA code".into(),
        });
    };
    Ok(Json(MfaVerifyResponse {
        enabled: true,
        recovery_codes,
    }))
}

/// `POST /auth/mfa/disable` — protected (JWT required).
///
/// Disables MFA (clears the secret + recovery codes) for the caller's own
/// account. Requires a currently-valid TOTP/recovery `code` in the body,
/// **unless** the caller is a superuser — the same effective-superuser rule
/// as every other admin gate ([`EngineHandle::ensure_superuser`]) — in which
/// case no code is needed (emergency account-recovery path, e.g. a user who
/// lost both their authenticator and their recovery codes). A missing code
/// (non-superuser) is a clear `400`; a present-but-wrong code is a uniform
/// `401` — no oracle on *why* a wrong code was rejected.
pub async fn post_mfa_disable(
    Extension(current_user): Extension<CurrentUser>,
    State(state): State<AppState>,
    Json(body): Json<MfaDisableRequest>,
) -> std::result::Result<StatusCode, ApiError> {
    let user = require_named_user(&current_user)?;
    let is_superuser = state
        .engine
        .ensure_superuser(current_user.0.clone())
        .await
        .is_ok();
    if !is_superuser && body.code.is_none() {
        return Err(ApiError::Api {
            status: StatusCode::BAD_REQUEST,
            code: "MFA_CODE_REQUIRED",
            message: "a current TOTP or recovery code is required to disable MFA".into(),
        });
    }
    let disabled = state
        .engine
        .mfa_disable(user, body.code, is_superuser)
        .await
        .map_err(ApiError::from)?;
    if !disabled {
        return Err(ApiError::Api {
            status: StatusCode::UNAUTHORIZED,
            code: "MFA_INVALID_CODE",
            message: "invalid MFA code".into(),
        });
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /auth/signup` (item 121, A3) — public, no JWT required.
///
/// Gated behind `UNIDB_ALLOW_SIGNUP=1` (default off — opt-in, never open by
/// default); returns `404 NOT_FOUND` when disabled, indistinguishable from a
/// non-existent route, same posture as `POST /auth/login`'s dev-login gate.
///
/// Creates a **non-superuser** user with an argon2id password credential
/// ([`crate::authz::RoleStore::apply`]'s `CreateUser` branch, via
/// [`EngineHandle::create_user_with_password`]) and immediately returns an
/// access+refresh token pair — the same shape `POST /auth/login` returns —
/// so a client never has to make a second round-trip to log in right after
/// signing up. A duplicate username is rejected with a clear
/// `400 AUTHZ_ERROR` (not a generic 500, and not silently overwriting the
/// existing account); the password is never stored or logged in plaintext
/// (argon2id, same as `CREATE USER ... PASSWORD`).
///
/// Token issuance still requires a signing key — `UNIDB_JWT_SIGNING_KEY`
/// (item 121 A5, production) or `UNIDB_DEV_LOGIN=1` (back-compat) — if
/// `UNIDB_ALLOW_SIGNUP=1` is set without either, this returns the same
/// "disabled" error `POST /auth/login` does, *before* creating the user, so
/// a signup attempt that can't hand back a usable token never leaves an
/// orphaned account behind.
pub async fn post_auth_signup(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(body): Json<crate::server::dto::AuthSignupRequest>,
) -> std::result::Result<Json<AuthLoginResponse>, ApiError> {
    if !state.allow_signup {
        return Err(route_disabled(
            "POST /auth/signup is disabled (set UNIDB_ALLOW_SIGNUP=1 to enable)",
        ));
    }
    // `state.dev_login_jwt` holds the issuing `JwtConfig` whenever issuance
    // is enabled at all — via `UNIDB_DEV_LOGIN=1` (its original name/purpose)
    // or item 121 A5's `UNIDB_JWT_SIGNING_KEY` production path; both wire
    // through the same `AppState::with_dev_login` setter (see
    // `src/bin/unidb-server.rs`), so this field name predates but still
    // accurately describes "the config that can mint tokens."
    let jwt_cfg = state.dev_login_jwt.as_ref().ok_or_else(|| {
        ApiError::from(crate::error::DbError::SqlPlan(
            "POST /auth/signup cannot issue tokens (set UNIDB_JWT_SIGNING_KEY or UNIDB_DEV_LOGIN=1 to enable a signing key)"
                .into(),
        ))
    })?;
    // Item 131 (Workstream I2) — CAPTCHA gate, before creating the user
    // (see `server::captcha`'s module doc). A no-op unless
    // `UNIDB_CAPTCHA_PROTECT` includes "signup".
    state
        .captcha
        .enforce(
            &state.engine,
            "signup",
            body.captcha_token.as_deref(),
            Some(&addr.ip().to_string()),
        )
        .await?;
    state
        .engine
        .create_user_with_password(body.username.clone(), body.password.clone())
        .await
        .map_err(ApiError::from)?;
    let resp = issue_token_pair(&state.engine, jwt_cfg, &body.username).await?;
    Ok(Json(resp))
}

/// `POST /auth/refresh` (item 121, A4) — public, no JWT required (the
/// refresh token itself *is* the credential).
///
/// Verifies the supplied refresh token (exists, unexpired, unrevoked —
/// [`EngineHandle::rotate_session`]) and, on success, **rotates** it: the
/// old refresh token is revoked and a brand-new one issued alongside a
/// fresh access token, so a leaked-and-later-replayed old refresh token is
/// a one-shot window, not a standing valid credential. Unknown, expired, and
/// revoked tokens all return the identical `401 INVALID_REFRESH_TOKEN` — no
/// way to distinguish *why* refresh failed.
///
/// The signing-key check happens **before** touching session state
/// (verify/rotate), so a server temporarily unable to issue tokens (neither
/// `UNIDB_JWT_SIGNING_KEY` nor `UNIDB_DEV_LOGIN` configured) never revokes a
/// still-good refresh token only to fail handing back its replacement.
pub async fn post_auth_refresh(
    State(state): State<AppState>,
    Json(body): Json<crate::server::dto::AuthRefreshRequest>,
) -> std::result::Result<Json<AuthLoginResponse>, ApiError> {
    let jwt_cfg = state.dev_login_jwt.as_ref().ok_or_else(|| {
        ApiError::from(crate::error::DbError::SqlPlan(
            "POST /auth/refresh cannot issue tokens (set UNIDB_JWT_SIGNING_KEY or UNIDB_DEV_LOGIN=1 to enable a signing key)"
                .into(),
        ))
    })?;
    let rotated = state
        .engine
        .rotate_session(body.refresh_token.clone())
        .await
        .map_err(ApiError::from)?;
    let Some((username, new_refresh_token, _expires_at)) = rotated else {
        return Err(ApiError::Api {
            status: StatusCode::UNAUTHORIZED,
            code: "INVALID_REFRESH_TOKEN",
            message: "invalid, expired, or revoked refresh token".into(),
        });
    };
    let access_token = jwt_cfg.issue_token(&username).map_err(|e| {
        ApiError::from(crate::error::DbError::SqlPlan(format!(
            "token issuance failed: {e}"
        )))
    })?;
    Ok(Json(AuthLoginResponse {
        token: access_token.clone(),
        access_token,
        refresh_token: new_refresh_token,
        expires_in: 3600,
    }))
}

/// `POST /auth/logout` (item 121, A4) — public, no JWT required (the
/// refresh token itself is the credential being revoked).
///
/// Marks the refresh-token session revoked ([`EngineHandle::
/// revoke_session`]); after this call the token can no longer mint access
/// tokens via `POST /auth/refresh`. Idempotent — logging out twice, or with
/// an unknown/garbage token, always returns `204 NO_CONTENT` rather than
/// leaking whether the token was ever valid.
pub async fn post_auth_logout(
    State(state): State<AppState>,
    Json(body): Json<crate::server::dto::AuthLogoutRequest>,
) -> std::result::Result<StatusCode, ApiError> {
    state
        .engine
        .revoke_session(body.refresh_token)
        .await
        .map_err(ApiError::from)?;
    Ok(StatusCode::NO_CONTENT)
}

// ── item 128 (Workstream D1): OAuth 2.0 social login ────────────────────

/// Map an [`OAuthError`] (from a provider HTTP call) to its HTTP status:
/// `502` when the provider was unreachable/erroring/unparseable, `401` when
/// it was reached and explicitly rejected the request. See
/// `crate::server::oauth::OAuthError`'s doc comment for the exact split.
fn oauth_error_to_api(err: OAuthError) -> ApiError {
    match err {
        OAuthError::ProviderUnavailable(msg) => ApiError::Api {
            status: StatusCode::BAD_GATEWAY,
            code: "OAUTH_PROVIDER_UNAVAILABLE",
            message: msg,
        },
        OAuthError::ProviderRejected(msg) => ApiError::Api {
            status: StatusCode::UNAUTHORIZED,
            code: "OAUTH_TOKEN_EXCHANGE_FAILED",
            message: msg,
        },
    }
}

/// `GET /auth/oauth/{provider}/authorize` (item 128) — public, no JWT
/// required (this route *is* the entry point to establishing identity).
///
/// A `provider` with no `UNIDB_OAUTH_<PROVIDER>_CLIENT_ID`/`_CLIENT_SECRET`/
/// `_REDIRECT_URI` configured (i.e. absent from [`crate::server::oauth::
/// OAuthConfig`]) returns `404 NOT_FOUND` — indistinguishable from a
/// non-existent route, same posture as dev-login/signup being disabled.
///
/// Mints a fresh CSRF `state` + PKCE `code_verifier`
/// ([`EngineHandle::create_oauth_state`]), persisted server-side
/// (single-use, [`crate::authz::OAUTH_STATE_TTL_SECS`]-second TTL), derives
/// the PKCE `code_challenge` ([`crate::server::oauth::code_challenge_s256`]),
/// and redirects (`302 Found`) to the provider's authorize URL with
/// `client_id`/`redirect_uri`/`scope`/`state`/`code_challenge`/
/// `code_challenge_method=S256`/`response_type=code`. The PKCE
/// `code_verifier` itself is never sent to the client — only the derived
/// `code_challenge` is, per RFC 7636. A JSON body carrying the same URL is
/// deliberately not offered — the redirect is the one documented contract,
/// simplest for a browser-driven flow and for the mock-provider tests here.
pub async fn get_oauth_authorize(
    Path(provider): Path<String>,
    State(state): State<AppState>,
) -> std::result::Result<Response, ApiError> {
    let Some(cfg) = state.oauth.get(&provider).cloned() else {
        return Err(route_disabled(format!(
            "OAuth provider '{provider}' is not configured"
        )));
    };
    let (oauth_state, code_verifier, _expires_at) = state
        .engine
        .create_oauth_state(provider)
        .await
        .map_err(ApiError::from)?;
    let code_challenge = crate::server::oauth::code_challenge_s256(&code_verifier);
    let url = crate::server::oauth::build_authorize_url(&cfg, &oauth_state, &code_challenge);
    Ok((StatusCode::FOUND, [(header::LOCATION, url)]).into_response())
}

/// `GET /auth/oauth/{provider}/callback?code=&state=` (item 128) — public,
/// no JWT required (the provider's redirect *is* the credential, same
/// posture as `POST /auth/refresh`'s refresh token).
///
/// Same `404` posture as [`get_oauth_authorize`] for an unconfigured
/// `provider`. Steps, in order:
/// 1. If the provider reports `?error=...` (user denied consent, etc.),
///    fails immediately with `400 OAUTH_PROVIDER_DENIED` — no state/token
///    work attempted.
/// 2. Validates + single-use-consumes `state`
///    ([`EngineHandle::verify_oauth_state`]) — unknown, expired, replayed,
///    or provider-mismatched state all return the identical `401
///    OAUTH_STATE_INVALID` (no oracle on *why*).
/// 3. Exchanges `code` for a provider access token, with the PKCE
///    `code_verifier` recovered from step 2
///    ([`crate::server::oauth::exchange_code_for_token`]) — provider
///    unreachable/erroring maps to `502`, an explicit provider rejection to
///    `401` (see [`oauth_error_to_api`]).
/// 4. Fetches the provider's userinfo (id + email) with that access token
///    ([`crate::server::oauth::fetch_userinfo`]) — same 502/401 split.
/// 5. Resolves `(provider, provider_user_id)` to a unidb user
///    ([`EngineHandle::oauth_link_or_create`]) — reuses the existing account
///    on a repeat login for the same identity, creates a fresh
///    non-superuser account on first login (see that method's doc comment
///    for the create-vs-link-by-email design note: this engine never
///    auto-links by email).
/// 6. Issues a real unidb session via the exact same [`issue_token_pair`]
///    helper every other login path (password, signup, MFA challenge)
///    uses, and returns it as the response body — the same
///    `{token, access_token, refresh_token, expires_in}` shape as `POST
///    /auth/login`.
pub async fn get_oauth_callback(
    Path(provider): Path<String>,
    State(state): State<AppState>,
    Query(params): Query<OAuthCallbackQuery>,
) -> std::result::Result<Json<AuthLoginResponse>, ApiError> {
    let Some(cfg) = state.oauth.get(&provider).cloned() else {
        return Err(route_disabled(format!(
            "OAuth provider '{provider}' is not configured"
        )));
    };
    if let Some(err) = params.error {
        return Err(ApiError::Api {
            status: StatusCode::BAD_REQUEST,
            code: "OAUTH_PROVIDER_DENIED",
            message: format!("provider returned an error: {err}"),
        });
    }
    let Some(code) = params.code else {
        return Err(ApiError::bad_request(
            "OAUTH_MISSING_CODE",
            "missing 'code' query parameter",
        ));
    };
    let Some(raw_state) = params.state else {
        return Err(ApiError::Api {
            status: StatusCode::UNAUTHORIZED,
            code: "OAUTH_STATE_INVALID",
            message: "missing 'state' query parameter".into(),
        });
    };
    let jwt_cfg = state.dev_login_jwt.as_ref().ok_or_else(|| {
        ApiError::from(crate::error::DbError::SqlPlan(
            "OAuth callback cannot issue tokens (set UNIDB_JWT_SIGNING_KEY or UNIDB_DEV_LOGIN=1 to enable a signing key)"
                .into(),
        ))
    })?;

    let code_verifier = state
        .engine
        .verify_oauth_state(raw_state, provider.clone())
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::Api {
            status: StatusCode::UNAUTHORIZED,
            code: "OAUTH_STATE_INVALID",
            message: "invalid, expired, replayed, or provider-mismatched OAuth state".into(),
        })?;

    // Item 129 (I3): resolve the effective client secret — a vault-stored
    // secret named `oauth.<provider>.client_secret` takes precedence over
    // the `UNIDB_OAUTH_<PROVIDER>_CLIENT_SECRET` env value baked into `cfg`
    // at startup. See `oauth::resolve_client_secret`'s doc comment for the
    // exact order and fail-closed behavior.
    let client_secret = crate::server::oauth::resolve_client_secret(&state.engine, &provider, &cfg)
        .await
        .map_err(oauth_error_to_api)?;

    let access_token = crate::server::oauth::exchange_code_for_token(
        state.oauth.http(),
        &cfg,
        &client_secret,
        &code,
        &code_verifier,
    )
    .await
    .map_err(oauth_error_to_api)?;

    let (provider_user_id, _email) =
        crate::server::oauth::fetch_userinfo(state.oauth.http(), &cfg, &access_token)
            .await
            .map_err(oauth_error_to_api)?;

    let username_hint = format!("oauth_{provider}_{provider_user_id}");
    let username = state
        .engine
        .oauth_link_or_create(provider, provider_user_id, username_hint)
        .await
        .map_err(ApiError::from)?;

    let resp = issue_token_pair(&state.engine, jwt_cfg, &username).await?;
    Ok(Json(resp))
}

/// `GET /auth/whoami` — protected (JWT required).
///
/// Returns the identity and authorisation summary for the caller:
/// - `user` — the `sub` claim from the token, or `null` for the implicit
///   superuser (token without `sub`).
/// - `is_superuser` — whether the user was created `SUPERUSER`.
/// - `roles` — direct role memberships.
/// - `privileges` — per-table grants: `[{table, ops}]`.
/// - `open_mode` — `true` when no users exist (all access passes anyway).
/// - `mfa_enabled` — item 127 (Workstream D4): whether the caller has TOTP
///   MFA enabled. Never the secret or recovery codes.
pub async fn get_auth_whoami(
    Extension(current_user): Extension<CurrentUser>,
    State(state): State<AppState>,
) -> std::result::Result<Json<WhoamiResponse>, ApiError> {
    let open_mode = !state.engine.has_users().await;
    let user = current_user.0.clone();
    let (is_superuser, roles, privileges, mfa_enabled) = if let Some(ref u) = user {
        let snapshot = state.engine.user_snapshot().await;
        let is_su = snapshot.iter().any(|(n, su)| n == u && *su);
        let roles = state.engine.user_roles(u.clone()).await;
        let grants = state.engine.user_grants(u.clone()).await;
        let privileges = grants
            .into_iter()
            .map(|(table, ops)| WhoamiPrivilege { table, ops })
            .collect();
        let mfa_enabled = state.engine.mfa_enabled(u.clone()).await;
        (is_su, roles, privileges, mfa_enabled)
    } else {
        // Implicit superuser (token without `sub`) — no named-user MFA state.
        (true, Vec::new(), Vec::new(), false)
    };
    Ok(Json(WhoamiResponse {
        user,
        is_superuser,
        roles,
        privileges,
        open_mode,
        mfa_enabled,
    }))
}

/// `DELETE /auth/sessions/{id}` — protected (JWT required).
///
/// Revoke one refresh-token session by its opaque `session_id` (item 4 —
/// Studio "revoke a specific session" action; the id comes from
/// `unidb_catalog.sessions`, never the raw token or its hash). Superuser/
/// self gated, reusing the same effective-superuser rule as every other
/// admin gate ([`EngineHandle::ensure_superuser`]): a superuser (the
/// implicit embedded caller, a named `SUPERUSER`, or open/bootstrap mode)
/// may revoke any session; a named non-superuser may only revoke a session
/// they own.
///
/// Idempotent and deliberately shape-uniform: an unknown id and a foreign
/// session (one that isn't the caller's) both resolve to `204 NO_CONTENT`
/// without the foreign session being touched — this endpoint never lets a
/// non-superuser caller distinguish "no such session" from "that session
/// isn't yours" (the same no-oracle posture as `POST /auth/logout`).
pub async fn delete_auth_session(
    Extension(current_user): Extension<CurrentUser>,
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> std::result::Result<StatusCode, ApiError> {
    let user = current_user.0.clone();
    let is_superuser = state.engine.ensure_superuser(user.clone()).await.is_ok();
    if !is_superuser {
        let owner = state.engine.session_owner(session_id.clone()).await;
        if owner.as_deref() != user.as_deref() {
            // Unknown id or someone else's session: no-op, 204 either way.
            return Ok(StatusCode::NO_CONTENT);
        }
    }
    state
        .engine
        .revoke_session_by_id(session_id)
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

// ── Observability extras (item 34) ────────────────────────────────────────

/// `PUT /config/slow_query_threshold_ms` (item 34, Part A): update the
/// slow-query threshold at runtime without a restart. Superuser-gated (same
/// gate as `PUT /tables/{table}/rls` and `POST /admin/flush`). `204 No Content`.
/// The threshold is an `AtomicU64` — no lock contention.
pub async fn put_config_slow_query_threshold_ms(
    Extension(current_user): Extension<CurrentUser>,
    State(state): State<AppState>,
    Json(body): Json<SlowQueryThresholdRequest>,
) -> std::result::Result<StatusCode, ApiError> {
    state
        .engine
        .ensure_superuser(current_user.0)
        .await
        .map_err(ApiError::from)?;
    state
        .engine
        .set_slow_query_threshold(body.threshold_ms)
        .await
        .map_err(ApiError::from)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `PUT /config/group_commit_window_us` (item 101): update the WAL group-commit
/// dwell window at runtime without a restart. Superuser-gated. `204 No Content`.
/// Zero disables the window (default); a positive value (e.g. 500) makes the
/// flush-lock leader sleep that many microseconds before fsyncing, giving
/// concurrent committers time to coalesce into one fsync.
pub async fn put_config_group_commit_window_us(
    Extension(current_user): Extension<CurrentUser>,
    State(state): State<AppState>,
    Json(body): Json<GroupCommitWindowRequest>,
) -> std::result::Result<StatusCode, ApiError> {
    state
        .engine
        .ensure_superuser(current_user.0)
        .await
        .map_err(ApiError::from)?;
    state
        .engine
        .set_group_commit_window_us(body.value)
        .await
        .map_err(ApiError::from)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /stats/history` (item 34, Part B): return the ring-buffered stats
/// history. Rate fields (`commits_per_sec`, `wal_bytes_per_sec`) are computed
/// server-side from consecutive ring entries — the Studio reads them directly
/// without client-side delta math. Points are oldest-first. An empty array is
/// returned on a fresh engine (not an error).
pub async fn get_stats_history(
    State(state): State<AppState>,
    Query(params): Query<HistoryQuery>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    let n = params.points.unwrap_or(60).min(300) as usize;
    let interval_ms = params.interval_ms.unwrap_or(5000);
    let points = state
        .engine
        .stats_history(n)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(json!({
        "interval_ms": interval_ms,
        "points": points,
    })))
}
