//! One `async fn` per route. Every handler that mutates state wraps
//! exactly one `begin -> execute -> commit-or-abort` cycle around a single
//! call (or, for `/sql`/`/cypher`, a single call that may itself run
//! multiple `;`-separated statements atomically — see `lib.rs`'s crate
//! doc). The writer-thread channel (`EngineHandle`) is the serialization
//! point, so no explicit locking is needed here at all.

use axum::{
    body::Bytes,
    extract::{Path, State},
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
            exec_result_to_json, AckEventsRequest, CreateEdgeRequest, CypherRequest,
            DeleteEdgeRequest, RowIdResponse, SetIndexRequest, SqlRequest,
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
    State(state): State<AppState>,
    Json(body): Json<SqlRequest>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    let xid = state.engine.begin(None).await?;
    let result = state.engine.execute_sql(xid, body.sql).await;
    let results = finish(&state.engine, xid, result).await?;
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
    let xid = state.engine.begin(None).await?;
    let result = state.engine.get(xid, row_id).await;
    finish(&state.engine, xid, result).await.map_err(ApiError)
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
