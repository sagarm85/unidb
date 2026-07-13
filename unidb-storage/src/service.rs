//! [`StorageService`] — the front door: create buckets, put/get/delete objects,
//! and drive the large-object presigned-upload flow. Small objects go inline as
//! engine LOBs (ACID, one transaction); large objects go to the object store via
//! the outbox (pending row + atomic event), confirmed by [`finish_upload`] or the
//! [`Reconciler`](crate::Reconciler).

use std::hash::{Hash, Hasher};
use std::sync::Arc;

use unidb::Engine;

use crate::metadata::{self, status, tier, ObjectRow};
use crate::spawn_engine;
use crate::store::ObjectStore;
use crate::{Result, StorageConfig, StorageError};

/// Physical storage key for `(bucket, object_key)` within the one store bucket.
pub fn storage_key(bucket: &str, object_key: &str) -> String {
    format!("{bucket}/{object_key}")
}

fn content_etag(bytes: &[u8]) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// What a `put_object` did.
#[derive(Debug, Clone)]
pub struct PutOutcome {
    /// `"inline"` (engine LOB) or `"s3"` (object store).
    pub tier: &'static str,
    pub size: u64,
    pub etag: Option<String>,
}

/// A presigned upload handout for a large object: the browser PUTs bytes to
/// `presigned_put_url`, then calls `finish_upload` (or the reconciler confirms).
#[derive(Debug, Clone)]
pub struct UploadTicket {
    pub storage_key: String,
    pub presigned_put_url: String,
}

/// The storage service. Cheap to clone-share (`Arc` inside).
pub struct StorageService {
    engine: Arc<Engine>,
    store: Arc<dyn ObjectStore>,
    config: StorageConfig,
}

impl StorageService {
    /// Create the service: ensure the metadata schema exists and enable events
    /// on `objects` (so every metadata write emits the atomic outbox event).
    pub async fn new(
        engine: Arc<Engine>,
        store: Arc<dyn ObjectStore>,
        config: StorageConfig,
    ) -> Result<Self> {
        let e = engine.clone();
        spawn_engine(move || {
            let xid = e.begin()?;
            match metadata::ensure_schema(&e, xid) {
                Ok(()) => e.commit(xid)?,
                Err(err) => {
                    let _ = e.abort(xid);
                    return Err(err);
                }
            }
            // Idempotent: enabling events on an already-enabled table is a no-op.
            e.enable_events(metadata::OBJECTS_TABLE)?;
            Ok(())
        })
        .await?;
        Ok(Self {
            engine,
            store,
            config,
        })
    }

    pub fn config(&self) -> &StorageConfig {
        &self.config
    }

    // ── buckets ─────────────────────────────────────────────────────────────

    /// Create a bucket. A no-op if it already exists.
    pub async fn create_bucket(&self, name: &str, created_by: Option<&str>) -> Result<()> {
        let engine = self.engine.clone();
        let name = name.to_string();
        let created_by = created_by.map(str::to_string);
        spawn_engine(move || {
            let xid = engine.begin()?;
            let res = (|| {
                if metadata::bucket_exists(&engine, xid, &name)? {
                    return Ok(());
                }
                metadata::insert_bucket(&engine, xid, &name, created_by.as_deref())
            })();
            match res {
                Ok(()) => engine.commit(xid),
                Err(e) => {
                    let _ = engine.abort(xid);
                    Err(e)
                }
            }
        })
        .await
    }

    // ── objects: put ────────────────────────────────────────────────────────

    /// Store an object, routing by size: `< inline_threshold` → engine LOB
    /// (ACID-inline), else → object store via the outbox path.
    pub async fn put_object(
        &self,
        bucket: &str,
        object_key: &str,
        bytes: Vec<u8>,
        content_type: Option<&str>,
        created_by: Option<&str>,
    ) -> Result<PutOutcome> {
        if bytes.len() < self.config.inline_threshold {
            self.put_inline(bucket, object_key, bytes, content_type, created_by)
                .await
        } else {
            self.put_s3(bucket, object_key, bytes, content_type, created_by)
                .await
        }
    }

    /// Store `bytes` and its metadata in **one transaction** (commit/rollback
    /// atomic). This is the LOB edge Supabase Storage lacks.
    async fn put_inline(
        &self,
        bucket: &str,
        object_key: &str,
        bytes: Vec<u8>,
        content_type: Option<&str>,
        created_by: Option<&str>,
    ) -> Result<PutOutcome> {
        let engine = self.engine.clone();
        let size = bytes.len() as u64;
        let etag = content_etag(&bytes);
        let row = ObjectRow {
            bucket: bucket.to_string(),
            object_key: object_key.to_string(),
            size: size as i64,
            etag: Some(etag.clone()),
            content_type: content_type.map(str::to_string),
            tier: tier::INLINE.to_string(),
            status: status::READY.to_string(),
            lob_id: None,
            created_by: created_by.map(str::to_string),
            created_at_ms: metadata::now_ms(),
        };
        spawn_engine(move || {
            let xid = engine.begin()?;
            let res = (|| {
                let lob_id = engine.put_large_object(xid, std::io::Cursor::new(bytes))?;
                let mut r = row;
                r.lob_id = Some(lob_id);
                metadata::insert_object(&engine, xid, &r)
            })();
            match res {
                Ok(()) => engine.commit(xid),
                Err(e) => {
                    let _ = engine.abort(xid);
                    Err(e)
                }
            }
        })
        .await?;
        Ok(PutOutcome {
            tier: tier::INLINE,
            size,
            etag: Some(etag),
        })
    }

    /// Server-side large-object path: outbox (pending row) → put bytes → confirm.
    /// A crash between steps is caught by the reconciler (compensate/orphan-sweep).
    async fn put_s3(
        &self,
        bucket: &str,
        object_key: &str,
        bytes: Vec<u8>,
        content_type: Option<&str>,
        created_by: Option<&str>,
    ) -> Result<PutOutcome> {
        let ticket = self
            .begin_upload(bucket, object_key, content_type, created_by)
            .await?;
        let meta = self
            .store
            .put(&ticket.storage_key, bytes, content_type)
            .await?;
        self.finish_upload(bucket, object_key).await?;
        Ok(PutOutcome {
            tier: tier::S3,
            size: meta.size,
            etag: meta.etag,
        })
    }

    /// Begin a presigned large-object upload: write the `pending` metadata row
    /// (atomic outbox event) and return a presigned PUT URL for direct upload.
    pub async fn begin_upload(
        &self,
        bucket: &str,
        object_key: &str,
        content_type: Option<&str>,
        created_by: Option<&str>,
    ) -> Result<UploadTicket> {
        let skey = storage_key(bucket, object_key);
        let engine = self.engine.clone();
        let row = ObjectRow {
            bucket: bucket.to_string(),
            object_key: object_key.to_string(),
            size: 0,
            etag: None,
            content_type: content_type.map(str::to_string),
            tier: tier::S3.to_string(),
            status: status::PENDING.to_string(),
            lob_id: None,
            created_by: created_by.map(str::to_string),
            created_at_ms: metadata::now_ms(),
        };
        spawn_engine(move || {
            let xid = engine.begin()?;
            match metadata::insert_object(&engine, xid, &row) {
                Ok(()) => engine.commit(xid),
                Err(e) => {
                    let _ = engine.abort(xid);
                    Err(e)
                }
            }
        })
        .await?;
        let url = self
            .store
            .presign_put(&skey, self.config.presign_ttl)
            .await?;
        Ok(UploadTicket {
            storage_key: skey,
            presigned_put_url: url,
        })
    }

    /// Confirm a pending upload: HEAD the store and flip `pending → ready`.
    /// Errors with `NotFound` if the bytes are not present (the caller retries,
    /// or the reconciler eventually compensates).
    pub async fn finish_upload(&self, bucket: &str, object_key: &str) -> Result<()> {
        let _row = self
            .lookup(bucket, object_key)
            .await?
            .ok_or_else(|| StorageError::NotFound(format!("{bucket}/{object_key}")))?;
        let skey = storage_key(bucket, object_key);
        let meta = self
            .store
            .head(&skey)
            .await?
            .ok_or_else(|| StorageError::NotFound(format!("bytes absent for {skey}")))?;

        let engine = self.engine.clone();
        let (bucket, object_key) = (bucket.to_string(), object_key.to_string());
        let etag = meta.etag.clone();
        let size = meta.size as i64;
        spawn_engine(move || {
            let xid = engine.begin()?;
            match metadata::mark_ready(&engine, xid, &bucket, &object_key, etag.as_deref(), size) {
                Ok(()) => engine.commit(xid),
                Err(e) => {
                    let _ = engine.abort(xid);
                    Err(e)
                }
            }
        })
        .await
    }

    // ── objects: get / delete / presign ──────────────────────────────────────

    /// Fetch an object's bytes (server-side). Browsers should use
    /// [`presign_get`](Self::presign_get) for the S3 tier instead.
    pub async fn get_object(&self, bucket: &str, object_key: &str) -> Result<Vec<u8>> {
        let row = self
            .lookup(bucket, object_key)
            .await?
            .ok_or_else(|| StorageError::NotFound(format!("{bucket}/{object_key}")))?;
        if row.status != status::READY {
            return Err(StorageError::NotFound(format!(
                "{bucket}/{object_key} is '{}', not ready",
                row.status
            )));
        }
        if row.tier == tier::INLINE {
            let lob_id = row
                .lob_id
                .ok_or_else(|| StorageError::NotFound(format!("{bucket}/{object_key} lob")))?;
            let engine = self.engine.clone();
            spawn_engine(move || {
                let xid = engine.begin()?;
                let mut buf = Vec::new();
                let res = engine.read_large_object(xid, lob_id, &mut buf);
                let _ = engine.commit(xid);
                res.map(|_| buf)
            })
            .await
        } else {
            let skey = storage_key(bucket, object_key);
            Ok(self.store.get(&skey).await?)
        }
    }

    /// Delete an object. Inline: LOB bytes + metadata row drop in one
    /// transaction. S3: metadata row is deleted first (so it is unreferenced),
    /// then the bytes — a crash between leaves an orphan the reconciler sweeps.
    pub async fn delete_object(&self, bucket: &str, object_key: &str) -> Result<()> {
        let row = match self.lookup(bucket, object_key).await? {
            Some(r) => r,
            None => return Ok(()), // idempotent
        };
        if row.tier == tier::INLINE {
            let engine = self.engine.clone();
            let (b, k) = (bucket.to_string(), object_key.to_string());
            let lob_id = row.lob_id;
            spawn_engine(move || {
                let xid = engine.begin()?;
                let res = (|| {
                    if let Some(id) = lob_id {
                        engine.delete_large_object(xid, id)?;
                    }
                    metadata::delete_object_row(&engine, xid, &b, &k)
                })();
                match res {
                    Ok(()) => engine.commit(xid),
                    Err(e) => {
                        let _ = engine.abort(xid);
                        Err(e)
                    }
                }
            })
            .await?;
        } else {
            let skey = storage_key(bucket, object_key);
            let engine = self.engine.clone();
            let (b, k) = (bucket.to_string(), object_key.to_string());
            spawn_engine(move || {
                let xid = engine.begin()?;
                match metadata::delete_object_row(&engine, xid, &b, &k) {
                    Ok(()) => engine.commit(xid),
                    Err(e) => {
                        let _ = engine.abort(xid);
                        Err(e)
                    }
                }
            })
            .await?;
            self.store.delete(&skey).await?;
        }
        Ok(())
    }

    /// A presigned GET URL for direct browser download (S3 tier).
    pub async fn presign_get(&self, bucket: &str, object_key: &str) -> Result<String> {
        let skey = storage_key(bucket, object_key);
        Ok(self
            .store
            .presign_get(&skey, self.config.presign_ttl)
            .await?)
    }

    /// Look up an object's metadata row.
    pub async fn lookup(&self, bucket: &str, object_key: &str) -> Result<Option<ObjectRow>> {
        let engine = self.engine.clone();
        let (b, k) = (bucket.to_string(), object_key.to_string());
        spawn_engine(move || {
            let xid = engine.begin()?;
            let out = metadata::lookup_object(&engine, xid, &b, &k);
            let _ = engine.commit(xid);
            out
        })
        .await
    }
}
