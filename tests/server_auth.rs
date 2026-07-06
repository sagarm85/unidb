//! JWT auth negative-case matrix (M5.d) — five distinct cases, each its own
//! test, since it's easy to accidentally conflate several failure modes
//! into one shallow "auth works" test: (1) no header, (2) malformed
//! bearer token, (3) wrong-signature token, (4) expired token, (5) a
//! valid, unexpired, correctly-signed token succeeds.

#[path = "server_common/mod.rs"]
mod server_common;

use server_common::{expired_token, valid_token, wrong_signature_token, TestServer};

async fn sql_status(server: &TestServer, auth_header: Option<String>) -> u16 {
    let client = reqwest::Client::new();
    let mut req = client
        .post(server.url("/sql"))
        .json(&serde_json::json!({ "sql": "CREATE TABLE t (id INT)" }));
    if let Some(h) = auth_header {
        req = req.header("Authorization", h);
    }
    req.send().await.unwrap().status().as_u16()
}

#[tokio::test]
async fn no_authorization_header_is_rejected() {
    let server = TestServer::spawn().await;
    assert_eq!(sql_status(&server, None).await, 401);
}

#[tokio::test]
async fn malformed_bearer_token_is_rejected() {
    let server = TestServer::spawn().await;
    assert_eq!(
        sql_status(&server, Some("Bearer not-a-real-jwt".to_string())).await,
        401
    );
}

#[tokio::test]
async fn non_bearer_authorization_header_is_rejected() {
    let server = TestServer::spawn().await;
    assert_eq!(
        sql_status(&server, Some("Basic dXNlcjpwYXNz".to_string())).await,
        401
    );
}

#[tokio::test]
async fn wrong_signature_token_is_rejected() {
    let server = TestServer::spawn().await;
    let header = format!("Bearer {}", wrong_signature_token());
    assert_eq!(sql_status(&server, Some(header)).await, 401);
}

#[tokio::test]
async fn expired_token_is_rejected() {
    let server = TestServer::spawn().await;
    let header = format!("Bearer {}", expired_token());
    assert_eq!(sql_status(&server, Some(header)).await, 401);
}

#[tokio::test]
async fn valid_unexpired_token_is_accepted() {
    let server = TestServer::spawn().await;
    let header = format!("Bearer {}", valid_token());
    assert_eq!(sql_status(&server, Some(header)).await, 200);
}

#[tokio::test]
async fn metrics_endpoint_requires_no_auth() {
    let server = TestServer::spawn().await;
    let client = reqwest::Client::new();
    let resp = client.get(server.url("/metrics")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
}
