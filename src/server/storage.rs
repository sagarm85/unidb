//! `/storage/*` HTTP handlers — bucket CRUD, object put/delete/list,
//! presigned GET URL (backlog item 31, Phase C). All 7 routes are protected
//! (JWT) and live under the `protected` sub-router in `router.rs`.
//!
//! **503 contract:** every handler begins with `require_storage`. If
//! `state.storage` is `None` (STORAGE_BACKEND not set or init failed at
//! startup) the handler immediately returns
//! `503 {"error":"…","code":"STORAGE_NOT_AVAILABLE"}`. No 500, no panic —
//! the server boots cleanly without storage configured.
//!
//! All methods are called through the `StorageApi` trait object
//! (`dyn crate::storage_api::StorageApi`) so `unidb` has no compile-time
//! dependency on `unidb-storage` — the concrete impl lives there and is wired
//! in by callers (tests, custom binaries) that already depend on both crates.

use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    Extension, Json,
};
use serde::{Deserialize, Serialize};

use crate::server::{auth::CurrentUser, error::ApiError, AppState};
use crate::storage_api::StorageApi;

// ── 503 guard ───────────────────────────────────────────────────────────────

fn require_storage(state: &AppState) -> std::result::Result<Arc<dyn StorageApi>, ApiError> {
    state.storage.clone().ok_or_else(|| {
        ApiError::service_unavailable(
            "STORAGE_NOT_AVAILABLE",
            "storage service is not configured (STORAGE_BACKEND not set or init failed)",
        )
    })
}

fn created_by(user: &CurrentUser) -> Option<String> {
    user.0.clone()
}

// ── C1: GET /storage/buckets — list all buckets ──────────────────────────────

#[derive(Serialize)]
pub struct BucketDto {
    pub name: String,
    pub created_by: Option<String>,
    pub created_at_ms: i64,
}

#[derive(Serialize)]
pub struct BucketListResponse {
    pub buckets: Vec<BucketDto>,
}

pub async fn list_buckets(
    Extension(_user): Extension<CurrentUser>,
    State(state): State<AppState>,
) -> std::result::Result<Json<BucketListResponse>, ApiError> {
    let svc = require_storage(&state)?;
    let rows = svc.list_buckets().await?;
    Ok(Json(BucketListResponse {
        buckets: rows
            .into_iter()
            .map(|b| BucketDto {
                name: b.name,
                created_by: b.created_by,
                created_at_ms: b.created_at_ms,
            })
            .collect(),
    }))
}

// ── C2: POST /storage/buckets — create a bucket ──────────────────────────────

#[derive(Deserialize)]
pub struct CreateBucketRequest {
    pub name: String,
}

pub async fn create_bucket(
    Extension(user): Extension<CurrentUser>,
    State(state): State<AppState>,
    Json(body): Json<CreateBucketRequest>,
) -> std::result::Result<StatusCode, ApiError> {
    let svc = require_storage(&state)?;
    svc.create_bucket(&body.name, created_by(&user).as_deref())
        .await?;
    Ok(StatusCode::CREATED)
}

// ── C3: DELETE /storage/buckets/{name} — delete a bucket ─────────────────────
//  Returns 409 BUCKET_NOT_EMPTY if the bucket still has object rows.

pub async fn delete_bucket(
    Extension(_user): Extension<CurrentUser>,
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> std::result::Result<StatusCode, ApiError> {
    let svc = require_storage(&state)?;
    svc.delete_bucket(&name).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ── C4: GET /storage/{bucket}/objects — list objects (virtual-folder aware) ──
//  Query: ?prefix=photos/&delimiter=/
//  Response: { "objects": [...], "prefixes": ["photos/vacation/"] }

#[derive(Deserialize)]
pub struct ListObjectsQuery {
    pub prefix: Option<String>,
    pub delimiter: Option<String>,
}

#[derive(Serialize)]
pub struct ObjectDto {
    pub object_key: String,
    pub size: i64,
    pub etag: Option<String>,
    pub content_type: Option<String>,
    pub status: String,
    pub tier: String,
    pub created_at_ms: i64,
}

#[derive(Serialize)]
pub struct ListObjectsResponse {
    pub objects: Vec<ObjectDto>,
    pub prefixes: Vec<String>,
}

pub async fn list_objects(
    Extension(_user): Extension<CurrentUser>,
    State(state): State<AppState>,
    Path(bucket): Path<String>,
    Query(q): Query<ListObjectsQuery>,
) -> std::result::Result<Json<ListObjectsResponse>, ApiError> {
    let svc = require_storage(&state)?;
    let result = svc
        .list_objects(&bucket, q.prefix.as_deref(), q.delimiter.as_deref())
        .await?;
    Ok(Json(ListObjectsResponse {
        objects: result
            .objects
            .into_iter()
            .map(|o| ObjectDto {
                object_key: o.object_key,
                size: o.size,
                etag: o.etag,
                content_type: o.content_type,
                status: o.status,
                tier: o.tier,
                created_at_ms: o.created_at_ms,
            })
            .collect(),
        prefixes: result.prefixes,
    }))
}

// ── C5: PUT /storage/{bucket}/objects/{*key} — put an object ─────────────────
//  Inline (body.len() < inline_threshold): body bytes stored as engine LOB;
//  response 201 { "tier":"inline", "size":N, "etag":"…" }.
//  Large (body.len() >= inline_threshold): begin_upload writes a pending
//  metadata row and returns a presigned PUT URL for direct client upload;
//  response 200 { "presigned_put_url":"…", "storage_key":"…" }.

pub async fn put_object(
    Extension(user): Extension<CurrentUser>,
    State(state): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> std::result::Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let svc = require_storage(&state)?;
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let by = created_by(&user);
    let bytes = body.to_vec();

    if bytes.len() >= svc.inline_threshold() {
        // Large-object path: return presigned PUT URL; client uploads directly.
        let ticket = svc
            .begin_upload(&bucket, &key, content_type.as_deref(), by.as_deref())
            .await?;
        return Ok((
            StatusCode::OK,
            Json(serde_json::json!({
                "presigned_put_url": ticket.presigned_put_url,
                "storage_key": ticket.storage_key,
            })),
        ));
    }

    // Inline path: store bytes as engine LOB in one transaction.
    let outcome = svc
        .put_object(&bucket, &key, bytes, content_type.as_deref(), by.as_deref())
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({
            "tier": outcome.tier,
            "size": outcome.size,
            "etag": outcome.etag,
        })),
    ))
}

// ── C6: DELETE /storage/{bucket}/objects/{*key} — delete an object ───────────

pub async fn delete_object(
    Extension(_user): Extension<CurrentUser>,
    State(state): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
) -> std::result::Result<StatusCode, ApiError> {
    let svc = require_storage(&state)?;
    svc.delete_object(&bucket, &key).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ── C7: GET /storage/{bucket}/presign/{*key} — presigned GET URL ─────────────

pub async fn presign_get(
    Extension(_user): Extension<CurrentUser>,
    State(state): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    let svc = require_storage(&state)?;
    let url = svc.presign_get(&bucket, &key).await?;
    Ok(Json(serde_json::json!({ "presigned_get_url": url })))
}
