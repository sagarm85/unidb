//! Acceptance (item 23): "kill the service mid-upload" — proven in **both**
//! directions with the deterministic memory store (no Docker):
//!
//! 1. metadata row exists but bytes never arrived  → outbox compensation
//!    (pending → failed + dead-letter; never a dangling pending).
//! 2. bytes exist but no committed metadata row     → reconciler orphan sweep.
//!
//! Plus the happy path: a pending row whose bytes *did* arrive is confirmed and
//! its (referenced) bytes are NOT swept.

mod common;

use std::time::Duration;

use unidb_storage::Reconciler;

/// Direction 1: killed before/at the client PUT — pending metadata, no bytes.
/// The reconciler compensates it to `failed` and dead-letters it. There is never
/// a `ready` row without bytes, and never a dangling `pending`.
#[tokio::test]
async fn pending_without_bytes_is_compensated_and_dead_lettered() {
    let h = common::harness(4).await;
    h.svc.create_bucket("b", None).await.unwrap();

    // Outbox row written; the upload never happened.
    let _ticket = h
        .svc
        .begin_upload("b", "lost.bin", None, None)
        .await
        .unwrap();
    assert!(h.store.is_empty(), "no bytes were uploaded");
    assert_eq!(
        h.svc.lookup("b", "lost.bin").await.unwrap().unwrap().status,
        "pending"
    );

    let mut cfg = h.svc.config().clone();
    cfg.pending_grace = Duration::ZERO; // stale immediately
    let recon = Reconciler::new(h.engine.clone(), h.store.clone(), cfg);
    let report = recon.run_once().await.unwrap();
    assert_eq!(report.compensated, 1);
    assert_eq!(report.confirmed, 0);

    let row = h.svc.lookup("b", "lost.bin").await.unwrap().unwrap();
    assert_eq!(row.status, "failed", "must not stay dangling 'pending'");

    // Dead-lettered (dogfood): one row in the compact DLQ table.
    assert_eq!(
        common::count_rows(&h.engine, "SELECT * FROM object_dlq"),
        1,
        "compensation must dead-letter the failed upload"
    );
}

/// Direction 2: bytes reached the store but the metadata transaction never
/// committed (rolled back on crash) — an orphan. The reconciler sweeps it.
#[tokio::test]
async fn orphan_bytes_without_metadata_are_swept() {
    let h = common::harness(4).await;

    // Bytes in the store, no metadata row references them.
    h.store.seed("b/orphan.bin", b"unreferenced bytes");
    assert_eq!(h.store.len(), 1);

    let mut cfg = h.svc.config().clone();
    cfg.orphan_grace = Duration::ZERO;
    let recon = Reconciler::new(h.engine.clone(), h.store.clone(), cfg);
    let report = recon.run_once().await.unwrap();
    assert_eq!(report.orphans_swept, 1);
    assert!(h.store.is_empty(), "orphan bytes must be swept");
}

/// The reconciler must NOT sweep bytes a live row references, and must confirm a
/// pending row whose bytes arrived — the false-positive guard.
#[tokio::test]
async fn pending_with_bytes_is_confirmed_not_compensated_or_swept() {
    let h = common::harness(4).await;
    h.svc.create_bucket("b", None).await.unwrap();

    let ticket = h.svc.begin_upload("b", "ok.bin", None, None).await.unwrap();
    h.store.seed(&ticket.storage_key, b"the bytes"); // upload landed

    let mut cfg = h.svc.config().clone();
    cfg.pending_grace = Duration::ZERO;
    cfg.orphan_grace = Duration::ZERO;
    let recon = Reconciler::new(h.engine.clone(), h.store.clone(), cfg);
    let report = recon.run_once().await.unwrap();

    assert_eq!(report.confirmed, 1);
    assert_eq!(report.compensated, 0);
    assert_eq!(report.orphans_swept, 0, "referenced bytes must survive");

    let row = h.svc.lookup("b", "ok.bin").await.unwrap().unwrap();
    assert_eq!(row.status, "ready");
    assert!(h.store.contains(&ticket.storage_key));
}
