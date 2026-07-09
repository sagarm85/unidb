//! Replication slots + WAL shipping over HTTP (P6.b). A real server, real
//! `reqwest` calls: create a slot, list it, ship the WAL stream, advance the
//! slot, and drop it.

#[path = "server_common/mod.rs"]
mod server_common;

use serde_json::{json, Value};
use server_common::{valid_token, TestServer};

#[tokio::test]
async fn slot_lifecycle_and_wal_shipping() {
    let server = TestServer::spawn().await;
    let client = reqwest::Client::new();
    let auth = format!("Bearer {}", valid_token());

    // Create a slot.
    let create = client
        .post(server.url("/replication/slots"))
        .header("Authorization", &auth)
        .json(&json!({ "name": "replica_1", "sync": false }))
        .send()
        .await
        .unwrap();
    assert_eq!(create.status(), 201);
    let body: Value = create.json().await.unwrap();
    assert_eq!(body["name"], "replica_1");
    assert_eq!(body["kind"], "async");

    // Duplicate create → 400 REPLICATION_ERROR.
    let dup = client
        .post(server.url("/replication/slots"))
        .header("Authorization", &auth)
        .json(&json!({ "name": "replica_1" }))
        .send()
        .await
        .unwrap();
    assert_eq!(dup.status(), 400);

    // List shows the slot.
    let list = client
        .get(server.url("/replication/slots"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(list.status(), 200);
    let list_body: Value = list.json().await.unwrap();
    assert_eq!(list_body["slots"].as_array().unwrap().len(), 1);

    // Write some data so the WAL has records to ship.
    let sql = client
        .post(server.url("/sql"))
        .header("Authorization", &auth)
        .json(&json!({ "sql": "CREATE TABLE t (id INT); INSERT INTO t (id) VALUES (1)" }))
        .send()
        .await
        .unwrap();
    assert_eq!(sql.status(), 200);

    // Ship the WAL from the start: octet-stream body + tail LSN header.
    let stream = client
        .get(server.url("/replication/stream?from_lsn=0"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(stream.status(), 200);
    let tail_lsn: u64 = stream
        .headers()
        .get("x-unidb-tail-lsn")
        .expect("tail LSN header present")
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(tail_lsn > 0, "tail LSN must have advanced past 0");
    let bytes = stream.bytes().await.unwrap();
    assert!(!bytes.is_empty(), "shipped stream must carry records");

    // Advance the slot to the tail LSN.
    let advance = client
        .post(server.url("/replication/slots/replica_1/advance"))
        .header("Authorization", &auth)
        .json(&json!({ "lsn": tail_lsn }))
        .send()
        .await
        .unwrap();
    assert_eq!(advance.status(), 204);

    // Drop the slot.
    let drop = client
        .delete(server.url("/replication/slots/replica_1"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(drop.status(), 204);

    // Dropping an unknown slot → 400.
    let drop_missing = client
        .delete(server.url("/replication/slots/nope"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(drop_missing.status(), 400);
}
