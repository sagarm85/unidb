//! The one S3-wire [`ObjectStore`] impl, serving **both** MinIO and S3.
//!
//! The backend is chosen entirely by config: MinIO sets a custom `endpoint_url`
//! plus `force_path_style(true)`; S3 uses a regional endpoint with virtual-host
//! addressing. Credentials come from env
//! (`STORAGE_ACCESS_KEY`/`STORAGE_SECRET_KEY`) as static keys — the documented
//! "creds via env" contract. Presigning is local SigV4 (no network), so URL
//! generation is unit-testable offline.

use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use aws_sdk_s3::presigning::PresigningConfig;
use aws_sdk_s3::primitives::{ByteStream, DateTime};
use aws_sdk_s3::Client;

use super::{ObjectEntry, ObjectMeta, ObjectStore, StoreError, StoreResult};
use crate::config::StorageConfig;

/// An S3-compatible object store (MinIO or AWS S3).
pub struct S3ObjectStore {
    client: Client,
    bucket: String,
}

impl S3ObjectStore {
    /// Build a client from a [`StorageConfig`]. Does not touch the network — the
    /// client is lazy, so this also succeeds offline (as the presign unit test
    /// relies on).
    pub fn from_config(cfg: &StorageConfig) -> StoreResult<Self> {
        let mut builder = aws_sdk_s3::config::Builder::new()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new(cfg.region.clone()))
            .force_path_style(cfg.force_path_style);

        if let Some(endpoint) = &cfg.endpoint {
            builder = builder.endpoint_url(endpoint);
        }
        match (&cfg.access_key, &cfg.secret_key) {
            (Some(ak), Some(sk)) => {
                builder = builder.credentials_provider(Credentials::new(
                    ak,
                    sk,
                    None,
                    None,
                    "unidb-storage-static",
                ));
            }
            _ => {
                return Err(StoreError::Config(
                    "S3/MinIO backend requires STORAGE_ACCESS_KEY and STORAGE_SECRET_KEY".into(),
                ))
            }
        }

        Ok(Self {
            client: Client::from_conf(builder.build()),
            bucket: cfg.bucket.clone(),
        })
    }

    /// Expose the client for tests that want to drive the store directly.
    pub fn client(&self) -> &Client {
        &self.client
    }
}

fn to_system_time(dt: Option<&DateTime>) -> Option<SystemTime> {
    dt.and_then(|d| {
        let secs = d.secs();
        if secs < 0 {
            return None;
        }
        Some(std::time::UNIX_EPOCH + Duration::new(secs as u64, d.subsec_nanos()))
    })
}

#[async_trait]
impl ObjectStore for S3ObjectStore {
    async fn put(
        &self,
        key: &str,
        bytes: Vec<u8>,
        content_type: Option<&str>,
    ) -> StoreResult<ObjectMeta> {
        let size = bytes.len() as u64;
        let mut req = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(ByteStream::from(bytes));
        if let Some(ct) = content_type {
            req = req.content_type(ct);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| StoreError::Backend(format!("put {key}: {e}")))?;
        Ok(ObjectMeta {
            size,
            etag: resp.e_tag().map(str::to_string),
            last_modified: Some(SystemTime::now()),
        })
    }

    async fn get(&self, key: &str) -> StoreResult<Vec<u8>> {
        let resp = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| {
                if e.as_service_error().map(|se| se.is_no_such_key()) == Some(true) {
                    StoreError::NotFound(key.to_string())
                } else {
                    StoreError::Backend(format!("get {key}: {e}"))
                }
            })?;
        let data = resp
            .body
            .collect()
            .await
            .map_err(|e| StoreError::Backend(format!("read body {key}: {e}")))?;
        Ok(data.into_bytes().to_vec())
    }

    async fn head(&self, key: &str) -> StoreResult<Option<ObjectMeta>> {
        match self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
        {
            Ok(resp) => Ok(Some(ObjectMeta {
                size: resp.content_length().unwrap_or(0).max(0) as u64,
                etag: resp.e_tag().map(str::to_string),
                last_modified: to_system_time(resp.last_modified()),
            })),
            Err(e) => {
                if e.as_service_error().map(|se| se.is_not_found()) == Some(true) {
                    Ok(None)
                } else {
                    Err(StoreError::Backend(format!("head {key}: {e}")))
                }
            }
        }
    }

    async fn delete(&self, key: &str) -> StoreResult<()> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| StoreError::Backend(format!("delete {key}: {e}")))?;
        Ok(())
    }

    async fn list(&self, prefix: &str) -> StoreResult<Vec<ObjectEntry>> {
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut req = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(prefix);
            if let Some(t) = &token {
                req = req.continuation_token(t);
            }
            let resp = req
                .send()
                .await
                .map_err(|e| StoreError::Backend(format!("list {prefix}: {e}")))?;
            for obj in resp.contents() {
                if let Some(k) = obj.key() {
                    out.push(ObjectEntry {
                        key: k.to_string(),
                        size: obj.size().unwrap_or(0).max(0) as u64,
                        last_modified: to_system_time(obj.last_modified()),
                    });
                }
            }
            if resp.is_truncated() == Some(true) {
                token = resp.next_continuation_token().map(str::to_string);
                if token.is_none() {
                    break;
                }
            } else {
                break;
            }
        }
        Ok(out)
    }

    async fn presign_put(&self, key: &str, ttl: Duration) -> StoreResult<String> {
        let cfg =
            PresigningConfig::expires_in(ttl).map_err(|e| StoreError::Presign(format!("{e}")))?;
        let req = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .presigned(cfg)
            .await
            .map_err(|e| StoreError::Presign(format!("presign put {key}: {e}")))?;
        Ok(req.uri().to_string())
    }

    async fn presign_get(&self, key: &str, ttl: Duration) -> StoreResult<String> {
        let cfg =
            PresigningConfig::expires_in(ttl).map_err(|e| StoreError::Presign(format!("{e}")))?;
        let req = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .presigned(cfg)
            .await
            .map_err(|e| StoreError::Presign(format!("presign get {key}: {e}")))?;
        Ok(req.uri().to_string())
    }
}
