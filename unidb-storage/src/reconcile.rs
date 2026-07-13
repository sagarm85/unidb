//! The [`Reconciler`] — the authority that resolves the S3-tier outbox.
//!
//! On each pass it: (1) **confirms** pending rows whose bytes have appeared
//! (`pending → ready`); (2) **compensates** pending rows whose bytes never
//! arrived within `pending_grace` (`pending → failed` + a dead-letter row in the
//! compact `object_dlq` table — never a dangling pending); and (3) **sweeps
//! orphaned bytes** — store keys unreferenced by any live (`ready`/`pending`)
//! metadata row and older than `orphan_grace`.
//!
//! The dead-letter table is pre-created by
//! [`metadata::ensure_schema`](crate::metadata::ensure_schema), so the
//! reconciler performs **no DDL at runtime** — see
//! `docs/design/storage_service.md` §4 (the single-page catalog ceiling).
//!
//! Why a reconciler and not purely a Dispatcher `Sink`: the item-20 Dispatcher's
//! retry is a tight in-cycle (millisecond) loop; an upload grace window is
//! wall-clock seconds-to-minutes. The correct "late upload" signal is the
//! pending row's age, which is a sweep concern. See
//! `docs/design/storage_service.md` §4.2.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use unidb::Engine;

use crate::metadata::{self, status, ObjectRow};
use crate::service::storage_key;
use crate::spawn_engine;
use crate::store::ObjectStore;
use crate::{Result, StorageConfig};

/// Counts from one [`Reconciler::run_once`] pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Pending rows confirmed to `ready` (bytes had arrived).
    pub confirmed: usize,
    /// Pending rows compensated to `failed` + dead-lettered (bytes never came).
    pub compensated: usize,
    /// Orphaned store objects deleted (bytes with no live metadata row).
    pub orphans_swept: usize,
}

/// Resolves the S3-tier outbox on an interval (or once, in tests).
pub struct Reconciler {
    engine: Arc<Engine>,
    store: Arc<dyn ObjectStore>,
    config: StorageConfig,
}

impl Reconciler {
    pub fn new(engine: Arc<Engine>, store: Arc<dyn ObjectStore>, config: StorageConfig) -> Self {
        Self {
            engine,
            store,
            config,
        }
    }

    /// One reconciliation pass.
    pub async fn run_once(&self) -> Result<ReconcileReport> {
        let mut report = ReconcileReport::default();
        self.confirm_and_compensate(&mut report).await?;
        self.sweep_orphans(&mut report).await?;
        Ok(report)
    }

    /// Drive [`run_once`](Self::run_once) on `interval` until `shutdown`
    /// resolves. Engine/store errors are logged and retried next tick.
    pub async fn run(&self, interval: Duration, shutdown: impl std::future::Future<Output = ()>) {
        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                _ = &mut shutdown => break,
                _ = tokio::time::sleep(interval) => {
                    if let Err(e) = self.run_once().await {
                        tracing::warn!(error = %e, "reconcile pass failed; retrying next tick");
                    }
                }
            }
        }
    }

    async fn confirm_and_compensate(&self, report: &mut ReconcileReport) -> Result<()> {
        let pending = self.list_pending().await?;
        let grace_ms = self.config.pending_grace.as_millis() as i64;
        for row in pending {
            let skey = storage_key(&row.bucket, &row.object_key);
            match self.store.head(&skey).await? {
                Some(meta) => {
                    self.mark_ready(&row, meta.etag, meta.size as i64).await?;
                    report.confirmed += 1;
                }
                None => {
                    let age = metadata::now_ms() - row.created_at_ms;
                    if age >= grace_ms {
                        self.compensate(&row).await?;
                        report.compensated += 1;
                    }
                }
            }
        }
        Ok(())
    }

    async fn sweep_orphans(&self, report: &mut ReconcileReport) -> Result<()> {
        // Reference set: bytes we must keep = ready or pending rows' storage keys.
        let rows = self.list_s3_objects().await?;
        let referenced: HashSet<String> = rows
            .iter()
            .filter(|r| r.status == status::READY || r.status == status::PENDING)
            .map(|r| storage_key(&r.bucket, &r.object_key))
            .collect();

        let now = SystemTime::now();
        for entry in self.store.list("").await? {
            if referenced.contains(&entry.key) {
                continue;
            }
            let old_enough = match entry.last_modified {
                Some(t) => now.duration_since(t).unwrap_or_default() >= self.config.orphan_grace,
                None => true,
            };
            if old_enough {
                self.store.delete(&entry.key).await?;
                report.orphans_swept += 1;
            }
        }
        Ok(())
    }

    // ── engine helpers (each on the blocking pool) ───────────────────────────

    async fn list_pending(&self) -> Result<Vec<ObjectRow>> {
        let engine = self.engine.clone();
        spawn_engine(move || {
            let xid = engine.begin()?;
            let out = metadata::list_pending(&engine, xid);
            let _ = engine.commit(xid);
            out
        })
        .await
    }

    async fn list_s3_objects(&self) -> Result<Vec<ObjectRow>> {
        let engine = self.engine.clone();
        spawn_engine(move || {
            let xid = engine.begin()?;
            let out = metadata::list_s3_objects(&engine, xid);
            let _ = engine.commit(xid);
            out
        })
        .await
    }

    async fn mark_ready(&self, row: &ObjectRow, etag: Option<String>, size: i64) -> Result<()> {
        let engine = self.engine.clone();
        let (b, k) = (row.bucket.clone(), row.object_key.clone());
        spawn_engine(move || {
            let xid = engine.begin()?;
            match metadata::mark_ready(&engine, xid, &b, &k, etag.as_deref(), size) {
                Ok(()) => engine.commit(xid),
                Err(e) => {
                    let _ = engine.abort(xid);
                    Err(e)
                }
            }
        })
        .await
    }

    /// Compensate a stale pending row: mark it `failed`, then dead-letter it into
    /// the pre-created `object_dlq` table (dogfood). Both writes are ordinary DML
    /// — no DDL — so this never rewrites the catalog. Never leaves a dangling
    /// pending.
    async fn compensate(&self, row: &ObjectRow) -> Result<()> {
        let engine = self.engine.clone();
        let (b, k) = (row.bucket.clone(), row.object_key.clone());
        spawn_engine(move || {
            let xid = engine.begin()?;
            match metadata::mark_failed(&engine, xid, &b, &k) {
                Ok(()) => engine.commit(xid),
                Err(e) => {
                    let _ = engine.abort(xid);
                    Err(e)
                }
            }
        })
        .await?;

        let engine = self.engine.clone();
        let (b, k) = (row.bucket.clone(), row.object_key.clone());
        spawn_engine(move || {
            let xid = engine.begin()?;
            let res = metadata::insert_dead_letter(
                &engine,
                xid,
                &b,
                &k,
                "upload never completed within grace; compensated to failed",
            );
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
}
