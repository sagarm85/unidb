//! `GET /stats` observability endpoint (P6.g).

#[path = "server_common/mod.rs"]
mod server_common;

use serde_json::{json, Value};
use server_common::{valid_token, TestServer};

#[tokio::test]
async fn stats_endpoint_reports_activity() {
    let server = TestServer::spawn().await;
    let client = reqwest::Client::new();
    let auth = format!("Bearer {}", valid_token());

    // Do some work so counters move.
    let resp = client
        .post(server.url("/sql"))
        .header("Authorization", &auth)
        .json(&json!({ "sql": "CREATE TABLE t (id INT); INSERT INTO t (id) VALUES (1)" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let resp = client
        .get(server.url("/stats"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert!(body["commits"].as_u64().unwrap() >= 1, "commits reported");
    assert_eq!(body["active_transactions"].as_u64().unwrap(), 0);
    assert!(body["data_pages"].as_u64().unwrap() > 0);
    assert!(body["recent_slow_queries"].is_array());
}
