//! Minimal async storage API abstraction for the HTTP server layer (item 31).
//!
//! Defined at crate root (not under `server`) so `unidb-storage` can implement
//! the trait without enabling the `server` feature of `unidb`. The server
//! handlers in `src/server/storage.rs` consume `dyn StorageApi`. `unidb`'s
//! library has no `unidb-storage` dependency — the concrete implementation
//! lives entirely in `unidb-storage/src/api_impl.rs`, which already depends
//! on `unidb` and just adds the impl.
//!
//! [`StorageCaller`] is the item 120 (Workstream F, F1) per-object-authz seam:
//! everything the storage service needs to know about the caller to gate a
//! read/write/delete. It is deliberately *not* a second identity system — it
//! is built once per request by `EngineHandle::storage_caller` from the same
//! `AuthPrincipal`/`authz::RoleStore` machinery every SQL statement already
//! goes through (subject resolution, `effective_roles`, `is_superuser`,
//! `service_role`), so a `/storage/*` caller is authorized with exactly the
//! same identity the rest of the server trusts.

/// Everything the storage service needs to know about the caller to gate a
/// per-object read/write/delete (item 120, Workstream F1). Built once per
/// request by `EngineHandle::storage_caller` — never constructed from
/// caller-supplied data directly, mirroring `AuthPrincipal::roles`'s own
/// "engine-side only" rule.
#[derive(Debug, Clone, Default)]
pub struct StorageCaller {
    /// The JWT `sub` claim / unidb username. `None` only for the implicit
    /// embedded superuser (a verified token with no `sub`, or the embedded
    /// API) — which is always `is_superuser`.
    pub subject: Option<String>,
    /// Effective roles (`anon`/`authenticated`/`service_role`/granted roles),
    /// as resolved by `authz::RoleStore::effective_roles`. Carried for
    /// completeness / a future role-scoped storage policy; the shipped F1
    /// gate consumes only `is_superuser` (which already folds in the
    /// `service_role` bypass).
    pub roles: Vec<String>,
    /// True for a named `SUPERUSER`, the implicit embedded/no-`sub` caller,
    /// open/bootstrap mode (no users registered yet), or a `service_role` JWT
    /// claim — bypasses every per-object rule exactly like RLS's superuser/
    /// `service_role` bypass (item-103 lesson). Audited by
    /// `EngineHandle::storage_caller` when it resolves `true` for a named
    /// subject.
    pub is_superuser: bool,
}

impl StorageCaller {
    /// A bypass-everything caller — the embedded/system caller shape (tests,
    /// internal callers that aren't a per-request HTTP principal).
    pub fn superuser() -> Self {
        Self {
            subject: None,
            roles: Vec::new(),
            is_superuser: true,
        }
    }

    /// A named, non-superuser, `authenticated` caller — the common case for
    /// exercising the per-object ownership gate.
    pub fn user(subject: impl Into<String>) -> Self {
        Self {
            subject: Some(subject.into()),
            roles: vec!["authenticated".to_string()],
            is_superuser: false,
        }
    }
}

pub struct BucketInfo {
    pub name: String,
    pub created_by: Option<String>,
    pub created_at_ms: i64,
    /// F1: public buckets are readable by any caller regardless of object
    /// ownership; private (the default) requires ownership or a bypass.
    pub is_public: bool,
}

pub struct ObjectInfo {
    pub object_key: String,
    pub size: i64,
    pub etag: Option<String>,
    pub content_type: Option<String>,
    pub status: String,
    pub tier: String,
    pub created_at_ms: i64,
    /// The object's owner (F1: the caller whose `AuthPrincipal.subject`
    /// created it) — `None` for objects created before F1 shipped or by an
    /// embedded/no-`sub` caller.
    pub owner: Option<String>,
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
    /// F1: the caller may not read/write/delete this object (not the owner,
    /// the bucket isn't public, and the caller doesn't bypass like a
    /// superuser/`service_role`). Maps to HTTP 403.
    #[error("forbidden: {0}")]
    Forbidden(String),
    #[error("task join failure")]
    Join,
}

/// A boxed, heap-allocated, send-safe future — used to make `StorageApi`
/// object-safe (dyn-compatible) while keeping async method signatures.
pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// Async storage operations exposed to the HTTP server layer (item 31 Phase C;
/// item 120 F1 adds the `caller` parameter used for per-object authorization).
/// All methods are dyn-compatible via `BoxFuture`.
pub trait StorageApi: Send + Sync {
    fn list_buckets<'a>(&'a self) -> BoxFuture<'a, Result<Vec<BucketInfo>, StorageApiError>>;

    fn create_bucket<'a>(
        &'a self,
        name: &'a str,
        caller: &'a StorageCaller,
        is_public: bool,
    ) -> BoxFuture<'a, Result<(), StorageApiError>>;

    fn delete_bucket<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<(), StorageApiError>>;

    /// Lists only the objects `caller` may read (F1): every object in a
    /// public bucket, or (in a private bucket) the caller's own objects, or
    /// every object when `caller` bypasses like a superuser/`service_role`.
    /// Filters rather than erroring — an unreadable object is simply absent.
    fn list_objects<'a>(
        &'a self,
        bucket: &'a str,
        prefix: Option<&'a str>,
        delimiter: Option<&'a str>,
        caller: &'a StorageCaller,
    ) -> BoxFuture<'a, Result<ListObjectsResult, StorageApiError>>;

    /// Creates or overwrites the object. F1: overwriting an existing object
    /// is gated to its owner or a bypass caller (`StorageApiError::Forbidden`
    /// otherwise); creating a brand-new object is allowed for any
    /// authenticated caller, who becomes its owner.
    fn put_object<'a>(
        &'a self,
        bucket: &'a str,
        key: &'a str,
        bytes: Vec<u8>,
        content_type: Option<&'a str>,
        caller: &'a StorageCaller,
    ) -> BoxFuture<'a, Result<PutObjectResult, StorageApiError>>;

    /// Same write gate as [`Self::put_object`] (F1), for the large-object
    /// presigned-PUT path.
    fn begin_upload<'a>(
        &'a self,
        bucket: &'a str,
        key: &'a str,
        content_type: Option<&'a str>,
        caller: &'a StorageCaller,
    ) -> BoxFuture<'a, Result<UploadTicket, StorageApiError>>;

    /// F1: gated to the object's owner or a bypass caller.
    fn delete_object<'a>(
        &'a self,
        bucket: &'a str,
        key: &'a str,
        caller: &'a StorageCaller,
    ) -> BoxFuture<'a, Result<(), StorageApiError>>;

    /// F1: issuance is gated on the same read rule as `get_object` — a caller
    /// cannot mint a presigned URL for an object they could not otherwise
    /// read.
    fn presign_get<'a>(
        &'a self,
        bucket: &'a str,
        key: &'a str,
        caller: &'a StorageCaller,
    ) -> BoxFuture<'a, Result<String, StorageApiError>>;

    /// Byte threshold below which an object is stored inline as an engine LOB.
    /// Objects at or above this threshold go through the presigned-PUT path.
    fn inline_threshold(&self) -> usize;
}
