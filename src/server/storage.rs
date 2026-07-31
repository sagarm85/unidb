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
//!
//! **Item 120, Workstream F1 — per-object authorization.** Every handler that
//! touches an object (list/put/delete/presign) resolves the caller's
//! [`crate::storage_api::StorageCaller`] via `EngineHandle::storage_caller`
//! (built from the request's `AuthPrincipal`, the same identity `require_jwt`
//! already put in the request extensions) and threads it into the
//! `StorageApi` call, which enforces the F1 rule (owner, public bucket, or a
//! superuser/`service_role` bypass) — see `unidb-storage/src/service.rs`.

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
use crate::AuthPrincipal;

// ── 503 guard ───────────────────────────────────────────────────────────────

fn require_storage(state: &AppState) -> std::result::Result<Arc<dyn StorageApi>, ApiError> {
    state.storage.clone().ok_or_else(|| {
        ApiError::service_unavailable(
            "STORAGE_NOT_AVAILABLE",
            "storage service is not configured (STORAGE_BACKEND not set or init failed)",
        )
    })
}

// ── C1: GET /storage/buckets — list all buckets ──────────────────────────────

#[derive(Serialize)]
pub struct BucketDto {
    pub name: String,
    pub created_by: Option<String>,
    pub created_at_ms: i64,
    /// F1: public buckets are readable by any caller regardless of object
    /// ownership.
    pub is_public: bool,
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
                is_public: b.is_public,
            })
            .collect(),
    }))
}

// ── C2: POST /storage/buckets — create a bucket ──────────────────────────────

#[derive(Deserialize)]
pub struct CreateBucketRequest {
    pub name: String,
    /// F1 (item 120, Workstream F): when `true`, every object in this bucket
    /// is readable by any caller regardless of ownership (unchanged
    /// Supabase-style "public bucket" semantics). Defaults to `false`
    /// (private) — fail closed.
    #[serde(default)]
    pub is_public: bool,
}

pub async fn create_bucket(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<AppState>,
    Json(body): Json<CreateBucketRequest>,
) -> std::result::Result<StatusCode, ApiError> {
    let svc = require_storage(&state)?;
    let caller = state.engine.storage_caller(principal).await?;
    svc.create_bucket(&body.name, &caller, body.is_public)
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
    /// The object's owner (F1) — `None` for objects created before F1
    /// shipped or by an embedded/no-`sub` caller.
    pub owner: Option<String>,
}

#[derive(Serialize)]
pub struct ListObjectsResponse {
    pub objects: Vec<ObjectDto>,
    pub prefixes: Vec<String>,
}

/// F1: returns only the objects `caller` may read (public bucket, own
/// objects, or a superuser/`service_role` bypass) — filtered, never an error,
/// so a caller with no readable objects in a private bucket just sees `[]`.
pub async fn list_objects(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<AppState>,
    Path(bucket): Path<String>,
    Query(q): Query<ListObjectsQuery>,
) -> std::result::Result<Json<ListObjectsResponse>, ApiError> {
    let svc = require_storage(&state)?;
    let caller = state.engine.storage_caller(principal).await?;
    let result = svc
        .list_objects(
            &bucket,
            q.prefix.as_deref(),
            q.delimiter.as_deref(),
            &caller,
        )
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
                owner: o.owner,
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

/// F1: overwriting an existing object is gated to its owner or a bypass
/// caller (403 `STORAGE_FORBIDDEN` otherwise); creating a brand-new object is
/// allowed for any authenticated caller, who becomes its owner.
pub async fn put_object(
    Extension(principal): Extension<AuthPrincipal>,
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
    let caller = state.engine.storage_caller(principal).await?;
    let bytes = body.to_vec();

    if bytes.len() >= svc.inline_threshold() {
        // Large-object path: return presigned PUT URL; client uploads directly.
        let ticket = svc
            .begin_upload(&bucket, &key, content_type.as_deref(), &caller)
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
        .put_object(&bucket, &key, bytes, content_type.as_deref(), &caller)
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

/// F1: gated to the object's owner or a bypass caller.
pub async fn delete_object(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
) -> std::result::Result<StatusCode, ApiError> {
    let svc = require_storage(&state)?;
    let caller = state.engine.storage_caller(principal).await?;
    svc.delete_object(&bucket, &key, &caller).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ── C7: GET /storage/{bucket}/presign/{*key} — presigned GET URL ─────────────

/// F1: issuance is gated on the same read rule as a direct object read — a
/// caller cannot mint a presigned URL (which grants bearer access) for an
/// object they could not otherwise read.
pub async fn presign_get(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    let svc = require_storage(&state)?;
    let caller = state.engine.storage_caller(principal).await?;
    let url = svc.presign_get(&bucket, &key, &caller).await?;
    Ok(Json(serde_json::json!({ "presigned_get_url": url })))
}
