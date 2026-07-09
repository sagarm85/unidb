//! One `async fn` per route. Every handler that mutates state wraps
//! exactly one `begin -> execute -> commit-or-abort` cycle around a single
//! call (or, for `/sql`/`/cypher`, a single call that may itself run
//! multiple `;`-separated statements atomically — see `lib.rs`'s crate
//! doc). The writer-thread channel (`EngineHandle`) is the serialization
//! point, so no explicit locking is needed here at all.

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde_json::json;

use crate::{
    error::Result,
    format::Xid,
    heap::RowId,
    server::{
        dto::{
            exec_result_to_json, slot_to_json, AckEventsRequest, AdvanceSlotRequest,
            CreateEdgeRequest, CreateSlotRequest, CypherRequest, DeleteEdgeRequest, RowIdResponse,
            SetIndexRequest, SqlRequest, StreamQuery,
        },
        engine_handle::EngineHandle,
        error::ApiError,
        AppState,
    },
};

/// Commit `xid` on `Ok`, abort it on `Err` — the one piece of boilerplate
/// every mutating handler shares. Returns the plain `crate::error::Result`
/// (not `ApiError`) — callers use `?` (which converts via the `From<DbError>
/// for ApiError` impl in `server::error`) or `.map_err(ApiError)` when
/// `finish`'s result is the handler's tail expression.
async fn finish<T>(engine: &EngineHandle, xid: Xid, result: Result<T>) -> Result<T> {
    match result {
        Ok(value) => {
            engine.commit(xid).await?;
            Ok(value)
        }
        Err(e) => {
            // Best-effort: if the abort itself fails (e.g. the writer
            // thread just died), the original error is still the one that
            // matters to the client.
            let _ = engine.abort(xid).await;
            Err(e)
        }
    }
}

// ── transactions ────────────────────────────────────────────────────────

pub async fn post_txn_begin(
    State(state): State<AppState>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    let xid = state.engine.begin(None).await?;
    Ok(Json(json!({ "xid": xid })))
}

// ── SQL / Cypher ─────────────────────────────────────────────────────────

pub async fn post_sql(
    axum::Extension(current_user): axum::Extension<crate::server::auth::CurrentUser>,
    State(state): State<AppState>,
    Json(body): Json<SqlRequest>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    let user = current_user.0;
    // Auth DDL (CREATE USER / GRANT / REVOKE, P6.e) isn't `sqlparser` grammar —
    // route it through the writer thread's `execute_sql_as`, which intercepts it
    // and requires superuser.
    if crate::authz::parse_auth_stmt(&body.sql)
        .map_err(ApiError)?
        .is_some()
    {
        let xid = state.engine.begin(None).await?;
        let result = state
            .engine
            .execute_sql_as(user.clone(), xid, body.sql.clone())
            .await;
        let results = finish(&state.engine, xid, result).await?;
        let json_results: Vec<_> = results.iter().map(exec_result_to_json).collect();
        return Ok(Json(json!({ "results": json_results })));
    }

    // Enforce per-user privileges (a no-op for the superuser / `None`) before the
    // fast-path dispatch below runs the statement.
    state
        .engine
        .authorize_sql(user.clone(), body.sql.clone())
        .await
        .map_err(ApiError)?;

    // Parameterized requests (P2.e) always go through the writer thread with
    // the values bound as data — the injection-safe path.
    let results = if !body.params.is_empty() {
        let params: Vec<_> = body
            .params
            .iter()
            .map(crate::server::dto::json_to_literal)
            .collect();
        let xid = state.engine.begin(None).await?;
        let result = state.engine.execute_sql_params(xid, body.sql, params).await;
        finish(&state.engine, xid, result).await?
    } else if crate::read_handle::is_concurrent_read_sql(&body.sql) {
        // Read-only SELECTs run on the concurrent read path (6b), off the single
        // writer thread — no begin/commit round-trips. Everything else (writes,
        // DDL, NEAR) goes through the writer thread as before.
        state.engine.execute_sql_read(body.sql).await?
    } else {
        let xid = state.engine.begin(None).await?;
        let result = state.engine.execute_sql(xid, body.sql).await;
        finish(&state.engine, xid, result).await?
    };
    let json_results: Vec<_> = results.iter().map(exec_result_to_json).collect();
    Ok(Json(json!({ "results": json_results })))
}

pub async fn post_cypher(
    State(state): State<AppState>,
    Json(body): Json<CypherRequest>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    let xid = state.engine.begin(None).await?;
    let result = state.engine.execute_cypher(xid, body.query).await;
    let results = finish(&state.engine, xid, result).await?;
    let json_results: Vec<_> = results.iter().map(exec_result_to_json).collect();
    Ok(Json(json!({ "results": json_results })))
}

// ── raw CRUD ─────────────────────────────────────────────────────────────

pub async fn post_row(
    State(state): State<AppState>,
    body: Bytes,
) -> std::result::Result<(StatusCode, Json<RowIdResponse>), ApiError> {
    let xid = state.engine.begin(None).await?;
    let result = state.engine.insert(xid, body.to_vec()).await;
    let row_id = finish(&state.engine, xid, result).await?;
    Ok((StatusCode::CREATED, Json(RowIdResponse { row_id })))
}

pub async fn get_row(
    State(state): State<AppState>,
    Path((page_id, slot)): Path<(u32, u16)>,
) -> std::result::Result<Vec<u8>, ApiError> {
    let row_id = RowId { page_id, slot };
    // Concurrent read path (6b): reads run off the single writer thread on a
    // shared, snapshot-consistent handle — no begin/get/commit round-trips.
    state.engine.get_row(row_id).await.map_err(ApiError)
}

pub async fn put_row(
    State(state): State<AppState>,
    Path((page_id, slot)): Path<(u32, u16)>,
    body: Bytes,
) -> std::result::Result<Json<RowIdResponse>, ApiError> {
    let row_id = RowId { page_id, slot };
    let xid = state.engine.begin(None).await?;
    let result = state.engine.update(xid, row_id, body.to_vec()).await;
    let new_row_id = finish(&state.engine, xid, result).await?;
    Ok(Json(RowIdResponse { row_id: new_row_id }))
}

pub async fn delete_row(
    State(state): State<AppState>,
    Path((page_id, slot)): Path<(u32, u16)>,
) -> std::result::Result<StatusCode, ApiError> {
    let row_id = RowId { page_id, slot };
    let xid = state.engine.begin(None).await?;
    let result = state.engine.delete(xid, row_id).await;
    finish(&state.engine, xid, result).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ── graph ────────────────────────────────────────────────────────────────

pub async fn post_edge(
    State(state): State<AppState>,
    Json(body): Json<CreateEdgeRequest>,
) -> std::result::Result<(StatusCode, Json<RowIdResponse>), ApiError> {
    let xid = state.engine.begin(None).await?;
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
    let row_id = finish(&state.engine, xid, result).await?;
    Ok((StatusCode::CREATED, Json(RowIdResponse { row_id })))
}

pub async fn delete_edge(
    State(state): State<AppState>,
    Path((page_id, slot)): Path<(u32, u16)>,
    Json(body): Json<DeleteEdgeRequest>,
) -> std::result::Result<StatusCode, ApiError> {
    let row_id = RowId { page_id, slot };
    let xid = state.engine.begin(None).await?;
    let result = state.engine.delete_edge(xid, row_id, body.from_id).await;
    finish(&state.engine, xid, result).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn get_edges_from(
    State(state): State<AppState>,
    Path(from_id): Path<i64>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    let xid = state.engine.begin(None).await?;
    let result = state.engine.edges_from(xid, from_id).await;
    let edges = finish(&state.engine, xid, result).await?;
    Ok(Json(json!({ "edges": edges })))
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
        .map_err(ApiError)?;
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
    state.engine.enable_events(table).await.map_err(ApiError)?;
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

// ── operational ──────────────────────────────────────────────────────────

pub async fn post_checkpoint(
    State(state): State<AppState>,
) -> std::result::Result<StatusCode, ApiError> {
    state.engine.checkpoint().await.map_err(ApiError)?;
    Ok(StatusCode::NO_CONTENT)
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
        .map_err(ApiError)?;
    Ok((StatusCode::CREATED, Json(slot_to_json(&info))))
}

pub async fn get_replication_slots(
    State(state): State<AppState>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    let slots = state.engine.replication_slots().await.map_err(ApiError)?;
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
        .map_err(ApiError)?;
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
        .map_err(ApiError)?;
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

    let (tail, bytes) = state.engine.ship_wal(q.from_lsn).await.map_err(ApiError)?;
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
