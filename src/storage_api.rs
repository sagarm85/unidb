//! Minimal async storage API abstraction for the HTTP server layer (item 31).
//!
//! Defined at crate root (not under `server`) so `unidb-storage` can implement
//! the trait without enabling the `server` feature of `unidb`. The server
//! handlers in `src/server/storage.rs` consume `dyn StorageApi`. `unidb`'s
//! library has no `unidb-storage` dependency — the concrete implementation
//! lives entirely in `unidb-storage/src/api_impl.rs`, which already depends
//! on `unidb` and just adds the impl.

pub struct BucketInfo {
    pub name: String,
    pub created_by: Option<String>,
    pub created_at_ms: i64,
}

pub struct ObjectInfo {
    pub object_key: String,
    pub size: i64,
    pub etag: Option<String>,
    pub content_type: Option<String>,
    pub status: String,
    pub tier: String,
    pub created_at_ms: i64,
}

pub struct ListObjectsResult {
    pub objects: Vec<ObjectInfo>,
    pub prefixes: Vec<String>,
}

pub struct PutObjectResult {
    pub tier: &'static str,
    pub size: u64,
    pub etag: Option<String>,
}

pub struct UploadTicket {
    pub presigned_put_url: String,
    pub storage_key: String,
}

#[derive(Debug, thiserror::Error)]
pub enum StorageApiError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("bucket not empty: {0}")]
    BucketNotEmpty(String),
    #[error("configuration error: {0}")]
    Config(String),
    #[error("object store error: {0}")]
    Store(String),
    #[error("engine error: {0}")]
    Engine(String),
    #[error("task join failure")]
    Join,
}

/// A boxed, heap-allocated, send-safe future — used to make `StorageApi`
/// object-safe (dyn-compatible) while keeping async method signatures.
pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// Async storage operations exposed to the HTTP server layer (item 31 Phase C).
/// All methods are dyn-compatible via `BoxFuture`.
pub trait StorageApi: Send + Sync {
    fn list_buckets<'a>(&'a self) -> BoxFuture<'a, Result<Vec<BucketInfo>, StorageApiError>>;

    fn create_bucket<'a>(
        &'a self,
        name: &'a str,
        created_by: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), StorageApiError>>;

    fn delete_bucket<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<(), StorageApiError>>;

    fn list_objects<'a>(
        &'a self,
        bucket: &'a str,
        prefix: Option<&'a str>,
        delimiter: Option<&'a str>,
    ) -> BoxFuture<'a, Result<ListObjectsResult, StorageApiError>>;

    fn put_object<'a>(
        &'a self,
        bucket: &'a str,
        key: &'a str,
        bytes: Vec<u8>,
        content_type: Option<&'a str>,
        created_by: Option<&'a str>,
    ) -> BoxFuture<'a, Result<PutObjectResult, StorageApiError>>;

    fn begin_upload<'a>(
        &'a self,
        bucket: &'a str,
        key: &'a str,
        content_type: Option<&'a str>,
        created_by: Option<&'a str>,
    ) -> BoxFuture<'a, Result<UploadTicket, StorageApiError>>;

    fn delete_object<'a>(
        &'a self,
        bucket: &'a str,
        key: &'a str,
    ) -> BoxFuture<'a, Result<(), StorageApiError>>;

    fn presign_get<'a>(
        &'a self,
        bucket: &'a str,
        key: &'a str,
    ) -> BoxFuture<'a, Result<String, StorageApiError>>;

    /// Byte threshold below which an object is stored inline as an engine LOB.
    /// Objects at or above this threshold go through the presigned-PUT path.
    fn inline_threshold(&self) -> usize;
}
