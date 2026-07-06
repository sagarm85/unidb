//! `DbError` -> HTTP mapping (M5.b wires this to axum's `IntoResponse`;
//! this file defines the shape now so `EngineHandle`'s reply types
//! (`crate::error::Result<T>`) stay stable across the M5.a/M5.b boundary
//! without pulling `axum` into M5.a's still axum-free dependency set).
//!
//! `ApiError` is a newtype, not an `impl IntoResponse for DbError` directly
//! on `crate::error::DbError` — `error.rs` is used by the default,
//! non-`server` build too and must stay completely axum-agnostic.

use crate::error::DbError;

pub struct ApiError(pub DbError);

impl From<DbError> for ApiError {
    fn from(err: DbError) -> Self {
        ApiError(err)
    }
}
