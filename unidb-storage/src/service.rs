//! [`StorageService`] — the front door: create buckets, put/get/delete objects,
//! and drive the large-object presigned-upload flow. Small objects go inline as
//! engine LOBs (ACID, one transaction); large objects go to the object store via
//! the outbox (pending row + atomic event), confirmed by [`finish_upload`] or the
//! [`Reconciler`](crate::Reconciler).
//!
//! ## Item 120, Workstream F1 — per-object authorization
//!
//! Every method that touches an existing object (`list_objects`, `get_object`,
//! `presign_get`) or may overwrite one (`put_object`/`begin_upload` via
//! `put_inline`) takes a `&`[`StorageCaller`] and enforces:
//!
//! - **Reads** ([`can_read`]): allowed if the bucket is public, the caller
//!   owns the object (`ObjectRow::created_by == caller.subject`), or the
//!   caller bypasses like a superuser/`service_role`
//!   (`StorageCaller::is_superuser`, resolved by
//!   `EngineHandle::storage_caller`). A private bucket with no matching rule
//!   is denied — fail closed.
//! - **Writes/deletes** ([`can_write`]): allowed only for the owner or a
//!   bypass caller; public-bucket status does **not** exempt writes.
//!   Creating a brand-new object (no existing row) is allowed for any
//!   authenticated caller, who becomes its owner.
//!
//! This reuses the engine's own identity/role/superuser/`service_role`/audit
//! machinery end to end (`AuthPrincipal` → `authz::RoleStore::effective_roles`
//! → `StorageCaller`, built once per request by `EngineHandle::storage_caller`
//! — see `src/server/engine_handle.rs`); it does not hand-roll a second
//! evaluator. It intentionally does **not** reuse the SQL RLS
//! predicate/`apply_rls` executor machinery: `objects`/`buckets` metadata is
//! read here through direct table helpers (`metadata.rs`), not through
//! `execute_sql_as_principal`'s RLS-aware plan path, so an owner+public-bucket
//! Rust-level gate is the simple, solid mechanism for F1 (see
//! `docs/backlog/125_storage_per_object_authz.md` for the full design note and
//! the policy-DDL follow-up this intentionally defers).

use std::hash::{Hash, Hasher};
use std::sync::Arc;

use unidb::storage_api::StorageCaller;
use unidb::Engine;

use crate::metadata::{self, status, tier, BucketRow, ObjectRow};
use crate::spawn_engine;
use crate::store::ObjectStore;
use crate::{Result, StorageConfig, StorageError};

/// F1 read gate: a public bucket is readable by anyone; a private bucket
/// requires ownership or a bypass caller. Fails closed (returns `false`) for
/// any case not explicitly allowed.
fn can_read(caller: &StorageCaller, bucket_public: bool, owner: Option<&str>) -> bool {
    if bucket_public || caller.is_superuser {
        return true;
    }
    matches!((caller.subject.as_deref(), owner), (Some(s), Some(o)) if s == o)
}

/// F1 write/delete gate: owner or a bypass caller only — public-bucket status
/// does not exempt writes. Fails closed.
fn can_write(caller: &StorageCaller, owner: Option<&str>) -> bool {
    if caller.is_superuser {
        return true;
    }
    matches!((caller.subject.as_deref(), owner), (Some(s), Some(o)) if s == o)
}

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

/// Result of [`StorageService::list_objects`]: direct-child objects plus any
/// virtual-folder prefixes produced by the delimiter split.
pub struct ListObjectsResult {
    pub objects: Vec<ObjectRow>,
    pub prefixes: Vec<String>,
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

    /// Create a bucket. A no-op if it already exists (an existing bucket's
    /// `is_public` flag is left untouched by a repeat call — idempotent
    /// creation, not an implicit flip). `is_public` is the F1 read-gate
    /// exemption (item 120, Workstream F).
    pub async fn create_bucket(
        &self,
        name: &str,
        caller: &StorageCaller,
        is_public: bool,
    ) -> Result<()> {
        let engine = self.engine.clone();
        let name = name.to_string();
        let created_by = caller.subject.clone();
        spawn_engine(move || {
            let xid = engine.begin()?;
            let res = (|| {
                if metadata::bucket_exists(&engine, xid, &name)? {
                    return Ok(());
                }
                metadata::insert_bucket(&engine, xid, &name, created_by.as_deref(), is_public)
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

    /// List all buckets.
    pub async fn list_buckets(&self) -> Result<Vec<crate::metadata::BucketRow>> {
        let engine = self.engine.clone();
        spawn_engine(move || {
            let xid = engine.begin()?;
            let rows = metadata::list_buckets(&engine, xid);
            let _ = engine.commit(xid);
            rows
        })
        .await
    }

    /// List objects in `bucket`, applying an optional S3-style prefix filter and
    /// delimiter split. When `delimiter` is `Some("/")` and `prefix` is
    /// `"photos/"`, direct-child keys become `objects` and keys that contain
    /// another `/` after the prefix become `prefixes` (virtual folders).
    ///
    /// F1 (item 120, Workstream F): **filters** to only the objects `caller`
    /// may read — never errors on an unreadable object, it is simply absent.
    /// A nonexistent bucket behaves as private-and-empty (fails closed, but
    /// there is nothing to leak either way).
    pub async fn list_objects(
        &self,
        bucket: &str,
        prefix: Option<&str>,
        delimiter: Option<&str>,
        caller: &StorageCaller,
    ) -> Result<ListObjectsResult> {
        let engine = self.engine.clone();
        let bucket_str = bucket.to_string();
        let (bucket_row, all) = spawn_engine(move || {
            let xid = engine.begin()?;
            let b = metadata::get_bucket(&engine, xid, &bucket_str);
            let rows = metadata::list_objects_in_bucket(&engine, xid, &bucket_str);
            let _ = engine.commit(xid);
            Ok((b?, rows?))
        })
        .await?;
        let bucket_public = bucket_row.map(|b| b.is_public).unwrap_or(false);

        let prefix = prefix.unwrap_or("");
        let filtered: Vec<ObjectRow> = all
            .into_iter()
            .filter(|o| o.object_key.starts_with(prefix))
            .filter(|o| can_read(caller, bucket_public, o.created_by.as_deref()))
            .collect();

        let Some(delim) = delimiter else {
            return Ok(ListObjectsResult {
                objects: filtered,
                prefixes: vec![],
            });
        };

        let mut objects = Vec::new();
        let mut prefix_set = std::collections::BTreeSet::new();
        for obj in filtered {
            let suffix = &obj.object_key[prefix.len()..];
            if let Some(pos) = suffix.find(delim) {
                let vfolder = format!("{}{}", prefix, &suffix[..pos + delim.len()]);
                prefix_set.insert(vfolder);
            } else {
                objects.push(obj);
            }
        }

        Ok(ListObjectsResult {
            objects,
            prefixes: prefix_set.into_iter().collect(),
        })
    }

    /// Delete a bucket. Returns `Err(StorageError::BucketNotEmpty)` (→ HTTP
    /// 409) if the bucket still has object rows. Deleting a non-existent
    /// bucket is a no-op (idempotent).
    pub async fn delete_bucket(&self, name: &str) -> Result<()> {
        let engine = self.engine.clone();
        let name_str = name.to_string();
        let has_objects = spawn_engine(move || {
            let xid = engine.begin()?;
            let rows = metadata::list_objects_in_bucket(&engine, xid, &name_str);
            let _ = engine.commit(xid);
            rows.map(|v| !v.is_empty())
        })
        .await?;

        if has_objects {
            return Err(crate::StorageError::BucketNotEmpty(name.to_string()));
        }

        let engine = self.engine.clone();
        let name_str = name.to_string();
        spawn_engine(move || {
            let xid = engine.begin()?;
            match metadata::delete_bucket_row(&engine, xid, &name_str) {
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
    /// (ACID-inline), else → object store via the outbox path. F1: gated by
    /// [`Self::check_write_allowed`].
    pub async fn put_object(
        &self,
        bucket: &str,
        object_key: &str,
        bytes: Vec<u8>,
        content_type: Option<&str>,
        caller: &StorageCaller,
    ) -> Result<PutOutcome> {
        if bytes.len() < self.config.inline_threshold {
            self.put_inline(bucket, object_key, bytes, content_type, caller)
                .await
        } else {
            self.put_s3(bucket, object_key, bytes, content_type, caller)
                .await
        }
    }

    /// F1 write gate (item 120, Workstream F): overwriting an existing object
    /// is allowed only for its owner or a bypass caller; creating a brand-new
    /// object (no existing row) is allowed for any caller reaching this
    /// method (every `/storage/*` route requires a verified JWT, so "no
    /// subject" only ever means the implicit embedded/superuser caller).
    async fn check_write_allowed(
        &self,
        bucket: &str,
        object_key: &str,
        caller: &StorageCaller,
    ) -> Result<()> {
        if let Some(existing) = self.lookup(bucket, object_key).await? {
            if !can_write(caller, existing.created_by.as_deref()) {
                return Err(StorageError::Forbidden(format!("{bucket}/{object_key}")));
            }
        }
        Ok(())
    }

    /// Store `bytes` and its metadata in **one transaction** (commit/rollback
    /// atomic). This is the LOB edge Supabase Storage lacks.
    async fn put_inline(
        &self,
        bucket: &str,
        object_key: &str,
        bytes: Vec<u8>,
        content_type: Option<&str>,
        caller: &StorageCaller,
    ) -> Result<PutOutcome> {
        self.check_write_allowed(bucket, object_key, caller).await?;
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
            created_by: caller.subject.clone(),
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
        caller: &StorageCaller,
    ) -> Result<PutOutcome> {
        let ticket = self
            .begin_upload(bucket, object_key, content_type, caller)
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
    /// F1: gated by [`Self::check_write_allowed`] — this is a direct route
    /// entry point (the large-object HTTP `PUT` path calls it without going
    /// through [`Self::put_object`]), so it carries its own gate.
    pub async fn begin_upload(
        &self,
        bucket: &str,
        object_key: &str,
        content_type: Option<&str>,
        caller: &StorageCaller,
    ) -> Result<UploadTicket> {
        self.check_write_allowed(bucket, object_key, caller).await?;
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
            created_by: caller.subject.clone(),
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

    /// F1 read gate (item 120, Workstream F): look up the object row and the
    /// owning bucket's `is_public` flag together, and deny
    /// (`StorageError::Forbidden`) unless `can_read` allows it. Shared by
    /// `get_object`, `delete_object`'s read-before-write, and `presign_get`
    /// so issuance and direct reads enforce the identical rule.
    async fn lookup_readable(
        &self,
        bucket: &str,
        object_key: &str,
        caller: &StorageCaller,
    ) -> Result<ObjectRow> {
        let engine = self.engine.clone();
        let (b, k) = (bucket.to_string(), object_key.to_string());
        let (bucket_row, row) = spawn_engine(move || {
            let xid = engine.begin()?;
            let bkt = metadata::get_bucket(&engine, xid, &b);
            let obj = metadata::lookup_object(&engine, xid, &b, &k);
            let _ = engine.commit(xid);
            Ok::<(Option<BucketRow>, Option<ObjectRow>), unidb::DbError>((bkt?, obj?))
        })
        .await?;
        let row = row.ok_or_else(|| StorageError::NotFound(format!("{bucket}/{object_key}")))?;
        let bucket_public = bucket_row.map(|b| b.is_public).unwrap_or(false);
        if !can_read(caller, bucket_public, row.created_by.as_deref()) {
            return Err(StorageError::Forbidden(format!("{bucket}/{object_key}")));
        }
        Ok(row)
    }

    /// Fetch an object's bytes (server-side). Browsers should use
    /// [`presign_get`](Self::presign_get) for the S3 tier instead. F1: gated
    /// by [`Self::lookup_readable`].
    pub async fn get_object(
        &self,
        bucket: &str,
        object_key: &str,
        caller: &StorageCaller,
    ) -> Result<Vec<u8>> {
        let row = self.lookup_readable(bucket, object_key, caller).await?;
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
    /// F1: gated to the object's owner or a bypass caller
    /// (`StorageError::Forbidden` otherwise).
    pub async fn delete_object(
        &self,
        bucket: &str,
        object_key: &str,
        caller: &StorageCaller,
    ) -> Result<()> {
        let row = match self.lookup(bucket, object_key).await? {
            Some(r) => r,
            None => return Ok(()), // idempotent
        };
        if !can_write(caller, row.created_by.as_deref()) {
            return Err(StorageError::Forbidden(format!("{bucket}/{object_key}")));
        }
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

    /// A presigned GET URL for direct browser download (S3 tier). F1 (item
    /// 120, Workstream F): issuance is gated on the exact same read rule as
    /// [`Self::get_object`] via [`Self::lookup_readable`] — a caller cannot
    /// mint a presigned URL (which grants bearer access to anyone holding it)
    /// for an object they could not otherwise read.
    pub async fn presign_get(
        &self,
        bucket: &str,
        object_key: &str,
        caller: &StorageCaller,
    ) -> Result<String> {
        self.lookup_readable(bucket, object_key, caller).await?;
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
