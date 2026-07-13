//! Scale/adversarial check (CLAUDE.md §0.6): the single-page catalog blob is a
//! hard ceiling, so prove the storage schema + reconciler survive real row
//! volume. Many pending objects (half with bytes, half without) are reconciled
//! in one pass — confirming the first half, compensating+dead-lettering the
//! second — with **no** `HeapFull`/catalog overflow, then the engine reopens and
//! still reads its metadata.

mod common;

use std::sync::Arc;
use std::time::Duration;

use unidb::Engine;
use unidb_storage::{MemoryObjectStore, Reconciler, StorageConfig, StorageService};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_objects_reconcile_without_catalog_overflow() {
    const N: usize = 1_000;

    let dir = tempfile::tempdir().unwrap();
    let engine = Arc::new(Engine::open(dir.path(), 0).unwrap());
    let store = Arc::new(MemoryObjectStore::new("unidb"));
    let mut cfg = StorageConfig::memory();
    cfg.inline_threshold = 4; // everything is "large" (S3 tier)
    cfg.pending_grace = Duration::ZERO;
    cfg.orphan_grace = Duration::ZERO;
    let svc = StorageService::new(engine.clone(), store.clone(), cfg.clone())
        .await
        .unwrap();
    svc.create_bucket("b", None).await.unwrap();

    // N pending uploads; the even half actually lands its bytes.
    for i in 0..N {
        let ticket = svc
            .begin_upload("b", &format!("obj{i}"), None, None)
            .await
            .unwrap();
        if i % 2 == 0 {
            store.seed(&ticket.storage_key, format!("payload-{i}").as_bytes());
        }
    }

    let recon = Reconciler::new(engine.clone(), store.clone(), cfg);
    let report = recon.run_once().await.unwrap();

    assert_eq!(report.confirmed, N / 2, "half had bytes → confirmed");
    assert_eq!(report.compensated, N / 2, "half had none → compensated");
    assert_eq!(report.orphans_swept, 0, "confirmed bytes stay referenced");

    // Metadata is intact and correctly split.
    assert_eq!(
        common::count_rows(&engine, "SELECT * FROM objects WHERE status = 'ready'"),
        N / 2
    );
    assert_eq!(
        common::count_rows(&engine, "SELECT * FROM objects WHERE status = 'failed'"),
        N / 2
    );
    assert_eq!(
        common::count_rows(&engine, "SELECT * FROM object_dlq"),
        N / 2,
        "every compensation is dead-lettered"
    );

    // Reopen: recovery reads the (small, creation-time) catalog blob fine.
    drop(svc);
    drop(recon);
    drop(store);
    drop(engine);
    let reopened = Engine::open(dir.path(), 0).unwrap();
    assert_eq!(
        common::count_rows(&reopened, "SELECT * FROM objects WHERE status = 'ready'"),
        N / 2,
        "metadata survives reopen"
    );
}
