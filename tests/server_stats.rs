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
    // Autovacuum observability (A4): the fields exist and are sane. The served
    // instance starts the launcher; one INSERT of one row leaves ~1 live tuple.
    assert!(body["autovacuums"].is_u64(), "autovacuums reported");
    assert!(body["dead_tuple_estimate"].is_u64());
    assert_eq!(body["live_tuple_estimate"].as_u64().unwrap(), 1);
    assert!(body["last_autovacuum_epoch_secs"].is_u64());

    // Item 21: the enriched engine metrics + server-session gauges are present.
    assert!(body["statement_latency"]["insert"]["count"].is_u64());
    assert!(body["bufferpool"]["hits"].is_u64());
    assert!(body["wal_fsyncs"].is_u64());
    assert!(body["locks"]["waits"].is_u64());
    assert!(body["horizon_age_secs"].is_number());
    assert!(body["parallel_workers"]["global_max"].is_u64());
    assert!(body["tables"].is_array(), "per-table stats array present");
    // Server-layer session panel (merged in the handler, not the engine).
    assert!(body["open_txn_sessions"].is_u64());
    assert!(body["open_cursors"].is_u64());
    assert!(body["idle_reaper_aborts"].is_u64());
}
