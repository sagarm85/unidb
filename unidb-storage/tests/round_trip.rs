//! Acceptance (item 23): upload/download/delete round-trip on both tiers, and
//! the sub-threshold LOB commit **and** rollback proof.

mod common;

use unidb::Engine;
use unidb_storage::metadata::{self, status, tier, ObjectRow};

#[tokio::test]
async fn inline_object_round_trips_and_stays_out_of_the_store() {
    let h = common::harness(1024).await;
    h.svc.create_bucket("b", Some("alice")).await.unwrap();

    let out = h
        .svc
        .put_object(
            "b",
            "hello.txt",
            b"hello world".to_vec(),
            Some("text/plain"),
            Some("alice"),
        )
        .await
        .unwrap();
    assert_eq!(out.tier, "inline");
    assert_eq!(out.size, 11);
    // Inline bytes live as an engine LOB — nothing goes to the object store.
    assert!(h.store.is_empty(), "inline object must not touch the store");

    let got = h.svc.get_object("b", "hello.txt").await.unwrap();
    assert_eq!(got, b"hello world");

    h.svc.delete_object("b", "hello.txt").await.unwrap();
    assert!(
        h.svc.get_object("b", "hello.txt").await.is_err(),
        "deleted object must not be retrievable"
    );
}

#[tokio::test]
async fn large_object_round_trips_via_store_with_presigned_urls() {
    let h = common::harness(4).await; // 4-byte threshold → the payload is "large"
    h.svc.create_bucket("b", None).await.unwrap();

    // 1. begin_upload: pending row (atomic outbox event) + presigned PUT URL.
    let ticket = h
        .svc
        .begin_upload("b", "big.bin", Some("application/octet-stream"), None)
        .await
        .unwrap();
    assert!(ticket.presigned_put_url.contains("stub-presign=put"));
    let row = h.svc.lookup("b", "big.bin").await.unwrap().unwrap();
    assert_eq!(row.status, "pending");
    assert_eq!(row.tier, "s3");

    // 2. The browser uploads bytes DIRECTLY to the store via the presigned URL.
    h.store.seed(&ticket.storage_key, b"a big payload");

    // 3. Confirm: pending -> ready.
    h.svc.finish_upload("b", "big.bin").await.unwrap();
    let row = h.svc.lookup("b", "big.bin").await.unwrap().unwrap();
    assert_eq!(row.status, "ready");

    // Download (server-side) and via a presigned GET URL.
    assert_eq!(
        h.svc.get_object("b", "big.bin").await.unwrap(),
        b"a big payload"
    );
    let get_url = h.svc.presign_get("b", "big.bin").await.unwrap();
    assert!(get_url.contains("stub-presign=get"));

    // Delete removes both metadata and bytes.
    h.svc.delete_object("b", "big.bin").await.unwrap();
    assert!(!h.store.contains(&ticket.storage_key), "bytes must be gone");
    assert!(h.svc.get_object("b", "big.bin").await.is_err());
}

#[tokio::test]
async fn put_object_routes_large_payloads_to_the_store() {
    let h = common::harness(4).await;
    h.svc.create_bucket("b", None).await.unwrap();

    let out = h
        .svc
        .put_object("b", "k", b"0123456789".to_vec(), None, None)
        .await
        .unwrap();
    assert_eq!(out.tier, "s3");
    assert_eq!(h.store.len(), 1, "large bytes go to the store");
    assert_eq!(h.svc.get_object("b", "k").await.unwrap(), b"0123456789");
}

/// The sub-threshold ACID guarantee: composing the inline object write (LOB +
/// metadata row) inside a user transaction and **aborting** it leaves no object
/// row and no readable bytes. This is exactly the transaction the service's
/// inline path runs — proven here at the engine level so the abort is explicit.
#[test]
fn inline_write_rolls_back_leaving_no_object_and_no_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();

    // Schema committed up front.
    let xid = engine.begin().unwrap();
    metadata::ensure_schema(&engine, xid).unwrap();
    engine.commit(xid).unwrap();

    // Write LOB + metadata under one user txn, then ABORT.
    let xid = engine.begin().unwrap();
    let lob_id = engine
        .put_large_object(xid, std::io::Cursor::new(b"secret".to_vec()))
        .unwrap();
    let row = ObjectRow {
        bucket: "b".into(),
        object_key: "k".into(),
        size: 6,
        etag: None,
        content_type: None,
        tier: tier::INLINE.into(),
        status: status::READY.into(),
        lob_id: Some(lob_id),
        created_by: None,
        created_at_ms: metadata::now_ms(),
    };
    metadata::insert_object(&engine, xid, &row).unwrap();
    engine.abort(xid).unwrap();

    // After rollback: no metadata row, no visible LOB bytes.
    let xid = engine.begin().unwrap();
    assert!(
        metadata::lookup_object(&engine, xid, "b", "k")
            .unwrap()
            .is_none(),
        "rolled-back object row must not persist"
    );
    let mut buf = Vec::new();
    let n = engine.read_large_object(xid, lob_id, &mut buf).unwrap();
    engine.commit(xid).unwrap();
    assert_eq!(n, 0, "aborted LOB bytes must not be readable");
    assert!(buf.is_empty());

    // And a committed inline write DOES persist (commit side of the proof).
    let xid = engine.begin().unwrap();
    let lob_id = engine
        .put_large_object(xid, std::io::Cursor::new(b"kept".to_vec()))
        .unwrap();
    let mut row2 = row;
    row2.lob_id = Some(lob_id);
    row2.size = 4;
    metadata::insert_object(&engine, xid, &row2).unwrap();
    engine.commit(xid).unwrap();

    let xid = engine.begin().unwrap();
    let found = metadata::lookup_object(&engine, xid, "b", "k").unwrap();
    let mut buf = Vec::new();
    engine
        .read_large_object(xid, found.unwrap().lob_id.unwrap(), &mut buf)
        .unwrap();
    engine.commit(xid).unwrap();
    assert_eq!(buf, b"kept", "committed inline write must persist");
}
