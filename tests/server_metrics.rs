//! `GET /metrics` (M5.d): 200, Prometheus content-type, and a non-empty
//! body containing real metrics after other routes have been hit — proving
//! `axum-prometheus`'s layer is actually wired into the router, not just
//! present but unused.

#[path = "server_common/mod.rs"]
mod server_common;

use server_common::{valid_token, TestServer};

#[tokio::test]
async fn metrics_endpoint_returns_prometheus_text_after_traffic() {
    let server = TestServer::spawn().await;
    let client = reqwest::Client::new();

    // Generate some traffic so there's something to observe.
    for _ in 0..3 {
        client
            .post(server.url("/sql"))
            .header("Authorization", format!("Bearer {}", valid_token()))
            .json(&serde_json::json!({"sql": "CREATE TABLE t (id INT)"}))
            .send()
            .await
            .ok();
    }

    let resp = client.get(server.url("/metrics")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let content_type = resp
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        content_type.starts_with("text/plain"),
        "expected Prometheus text/plain content type, got {content_type}"
    );

    let body = resp.text().await.unwrap();
    assert!(!body.is_empty());
    assert!(
        body.contains("axum_http_requests_total"),
        "expected the auto-instrumented HTTP counter, got:\n{body}"
    );
}
