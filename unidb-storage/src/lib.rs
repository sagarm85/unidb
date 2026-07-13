//! # unidb-storage — object storage service (backlog item 23)
//!
//! A **Supabase-Storage analog** built as an **app-layer** service, honoring
//! CLAUDE.md §10 ("no S3 tiering *in* the engine"): bucket/object **metadata**
//! is kept transactional in ordinary unidb tables, while object **bytes** are
//! tiered:
//!
//! - **small objects (`< inline_threshold`, default 1 MiB) → engine LOBs.**
//!   Bytes and metadata commit/roll back in **one user transaction** (P3.d) —
//!   the ACID-inline edge Supabase Storage lacks.
//! - **large objects → an S3-wire object store** (MinIO in dev, S3 in prod),
//!   with browsers moving bytes **directly** via presigned PUT/GET so the engine
//!   never proxies a large payload.
//!
//! Consistency for the S3 tier rides an **outbox**: the metadata row and its
//! "upload-pending" event commit atomically (events are enabled on `objects`),
//! and a [`Reconciler`] confirms uploads (`pending → ready`) or **compensates**
//! (`pending → failed` + dead-letter, never a dangling pending) and sweeps
//! orphaned bytes. See `docs/design/storage_service.md` for the full design note
//! (crate choice, the outbox/reconciler decision, and the honest wall where the
//! item-20 Dispatcher's tight retry does not fit an upload grace window).
//!
//! ## No engine surface
//!
//! Like `unidb-dispatch`, this crate embeds `Arc<Engine>` and drives **only** the
//! shipped public API. `tokio` + the AWS SDK live here, never in the engine's
//! default build — the "engine stays sync" invariant is untouched.

pub mod config;
pub mod metadata;
pub mod outbox;
pub mod reconcile;
pub mod service;
pub mod store;

pub use config::{Backend, StorageConfig};
pub use reconcile::{ReconcileReport, Reconciler};
pub use service::{PutOutcome, StorageService, UploadTicket};
pub use store::{
    MemoryObjectStore, ObjectEntry, ObjectMeta, ObjectStore, S3ObjectStore, StoreError,
};

/// Errors surfaced by the storage service.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("engine error: {0}")]
    Engine(#[from] unidb::DbError),
    #[error("object store error: {0}")]
    Store(#[from] StoreError),
    #[error("object not found: {0}")]
    NotFound(String),
    #[error("configuration error: {0}")]
    Config(String),
    #[error("blocking engine task failed to join")]
    Join,
}

pub type Result<T> = std::result::Result<T, StorageError>;

/// Run one blocking `Engine` call on the tokio blocking pool — the same
/// choke-point pattern `unidb-dispatch` and the server use, so the sync engine
/// is never called from an async task directly.
pub(crate) async fn spawn_engine<T, F>(f: F) -> Result<T>
where
    F: FnOnce() -> std::result::Result<T, unidb::DbError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| StorageError::Join)?
        .map_err(StorageError::from)
}
