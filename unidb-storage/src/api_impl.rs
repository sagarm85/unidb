//! Implements `unidb::storage_api::StorageApi` for `StorageService` (item 31).
//!
//! This glue lives in `unidb-storage` (which already depends on `unidb`) so
//! the `unidb` crate itself never imports `unidb-storage` — avoiding the
//! circular dependency `unidb → unidb-storage → unidb`.
//!
//! The trait uses `BoxFuture` return types for dyn-object compatibility; each
//! method wraps the inherent async method in `Box::pin(async move { … })`.

use unidb::storage_api::{
    BoxFuture, BucketInfo, ListObjectsResult, ObjectInfo, PutObjectResult, StorageApi,
    StorageApiError, UploadTicket,
};

use crate::{StorageError, StorageService};

// ── Error conversion ─────────────────────────────────────────────────────────

impl From<StorageError> for StorageApiError {
    fn from(e: StorageError) -> Self {
        match e {
            StorageError::Engine(db_err) => StorageApiError::Engine(db_err.to_string()),
            StorageError::Store(store_err) => StorageApiError::Store(store_err.to_string()),
            StorageError::NotFound(s) => StorageApiError::NotFound(s),
            StorageError::BucketNotEmpty(s) => StorageApiError::BucketNotEmpty(s),
            StorageError::Config(s) => StorageApiError::Config(s),
            StorageError::Join => StorageApiError::Join,
        }
    }
}

// ── StorageApi impl ──────────────────────────────────────────────────────────

impl StorageApi for StorageService {
    fn list_buckets<'a>(&'a self) -> BoxFuture<'a, Result<Vec<BucketInfo>, StorageApiError>> {
        Box::pin(async move {
            let rows = self.list_buckets().await.map_err(StorageApiError::from)?;
            Ok(rows
                .into_iter()
                .map(|r| BucketInfo {
                    name: r.name,
                    created_by: r.created_by,
                    created_at_ms: r.created_at_ms,
                })
                .collect())
        })
    }

    fn create_bucket<'a>(
        &'a self,
        name: &'a str,
        created_by: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), StorageApiError>> {
        Box::pin(async move {
            self.create_bucket(name, created_by)
                .await
                .map_err(StorageApiError::from)
        })
    }

    fn delete_bucket<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<(), StorageApiError>> {
        Box::pin(async move {
            self.delete_bucket(name)
                .await
                .map_err(StorageApiError::from)
        })
    }

    fn list_objects<'a>(
        &'a self,
        bucket: &'a str,
        prefix: Option<&'a str>,
        delimiter: Option<&'a str>,
    ) -> BoxFuture<'a, Result<ListObjectsResult, StorageApiError>> {
        Box::pin(async move {
            let r = self
                .list_objects(bucket, prefix, delimiter)
                .await
                .map_err(StorageApiError::from)?;
            Ok(ListObjectsResult {
                objects: r
                    .objects
                    .into_iter()
                    .map(|o| ObjectInfo {
                        object_key: o.object_key,
                        size: o.size,
                        etag: o.etag,
                        content_type: o.content_type,
                        status: o.status,
                        tier: o.tier,
                        created_at_ms: o.created_at_ms,
                    })
                    .collect(),
                prefixes: r.prefixes,
            })
        })
    }

    fn put_object<'a>(
        &'a self,
        bucket: &'a str,
        key: &'a str,
        bytes: Vec<u8>,
        content_type: Option<&'a str>,
        created_by: Option<&'a str>,
    ) -> BoxFuture<'a, Result<PutObjectResult, StorageApiError>> {
        Box::pin(async move {
            let o = self
                .put_object(bucket, key, bytes, content_type, created_by)
                .await
                .map_err(StorageApiError::from)?;
            Ok(PutObjectResult {
                tier: o.tier,
                size: o.size,
                etag: o.etag,
            })
        })
    }

    fn begin_upload<'a>(
        &'a self,
        bucket: &'a str,
        key: &'a str,
        content_type: Option<&'a str>,
        created_by: Option<&'a str>,
    ) -> BoxFuture<'a, Result<UploadTicket, StorageApiError>> {
        Box::pin(async move {
            let t = self
                .begin_upload(bucket, key, content_type, created_by)
                .await
                .map_err(StorageApiError::from)?;
            Ok(UploadTicket {
                presigned_put_url: t.presigned_put_url,
                storage_key: t.storage_key,
            })
        })
    }

    fn delete_object<'a>(
        &'a self,
        bucket: &'a str,
        key: &'a str,
    ) -> BoxFuture<'a, Result<(), StorageApiError>> {
        Box::pin(async move {
            self.delete_object(bucket, key)
                .await
                .map_err(StorageApiError::from)
        })
    }

    fn presign_get<'a>(
        &'a self,
        bucket: &'a str,
        key: &'a str,
    ) -> BoxFuture<'a, Result<String, StorageApiError>> {
        Box::pin(async move {
            self.presign_get(bucket, key)
                .await
                .map_err(StorageApiError::from)
        })
    }

    fn inline_threshold(&self) -> usize {
        self.config().inline_threshold
    }
}
