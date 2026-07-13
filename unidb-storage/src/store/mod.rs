//! The [`ObjectStore`] trait and its impls.
//!
//! One S3-wire impl ([`s3::S3ObjectStore`]) serves **both** MinIO (dev) and S3
//! (prod) — the backend is chosen by config (endpoint + path-style + creds), not
//! by a second code path. A second impl ([`memory::MemoryObjectStore`]) is the
//! Docker-free test double. See `docs/design/storage_service.md` §2.

use std::time::{Duration, SystemTime};

use async_trait::async_trait;

pub mod memory;
pub mod s3;

pub use memory::MemoryObjectStore;
pub use s3::S3ObjectStore;

/// A single object-store failure.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("object not found: {0}")]
    NotFound(String),
    #[error("object store backend error: {0}")]
    Backend(String),
    #[error("presign error: {0}")]
    Presign(String),
    #[error("store configuration error: {0}")]
    Config(String),
}

pub type StoreResult<T> = std::result::Result<T, StoreError>;

/// Metadata about a stored object (from `head`).
#[derive(Debug, Clone)]
pub struct ObjectMeta {
    pub size: u64,
    pub etag: Option<String>,
    pub last_modified: Option<SystemTime>,
}

/// One entry from a `list` sweep.
#[derive(Debug, Clone)]
pub struct ObjectEntry {
    pub key: String,
    pub size: u64,
    pub last_modified: Option<SystemTime>,
}

/// Byte storage behind the service. All keys are the physical storage key
/// (`"<bucket>/<object_key>"`), namespaced within the one configured store
/// bucket.
#[async_trait]
pub trait ObjectStore: Send + Sync {
    /// Store bytes at `key`, returning the etag + size.
    async fn put(
        &self,
        key: &str,
        bytes: Vec<u8>,
        content_type: Option<&str>,
    ) -> StoreResult<ObjectMeta>;

    /// Fetch the whole object. `NotFound` if the key is absent.
    async fn get(&self, key: &str) -> StoreResult<Vec<u8>>;

    /// Metadata for `key`, or `None` if it does not exist. Never errors on
    /// "not found" — that is `Ok(None)` (the reconciler distinguishes "missing"
    /// from "backend down").
    async fn head(&self, key: &str) -> StoreResult<Option<ObjectMeta>>;

    /// Delete `key`. Deleting a missing key is not an error (idempotent).
    async fn delete(&self, key: &str) -> StoreResult<()>;

    /// List every object whose key starts with `prefix` (paginated internally).
    async fn list(&self, prefix: &str) -> StoreResult<Vec<ObjectEntry>>;

    /// A presigned PUT URL a browser can upload to directly.
    async fn presign_put(&self, key: &str, ttl: Duration) -> StoreResult<String>;

    /// A presigned GET URL a browser can download from directly.
    async fn presign_get(&self, key: &str, ttl: Duration) -> StoreResult<String>;
}
