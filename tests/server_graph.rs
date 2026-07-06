//! Graph edges over HTTP (M5.d): `POST /edges`, `GET /edges/from/:id`,
//! `DELETE /edges/:page_id/:slot`. A dedicated file since graph's
//! from_id-carrying delete is a real quirk worth isolating, matching how
//! `tests/graph_locking.rs`/`graph_rebuild.rs`/`graph_mvcc.rs` are already
//! split by concern rather than one giant `tests/graph.rs`.

#[path = "server_common/mod.rs"]
mod server_common;

use serde_json::Value;
use server_common::{valid_token, TestServer};

async fn post_json(server: &TestServer, path: &str, body: Value) -> (u16, Value) {
    let client = reqwest::Client::new();
    let resp = client
        .post(server.url(path))
        .header("Authorization", format!("Bearer {}", valid_token()))
        .json(&body)
        .send()
        .await
        .unwrap();
    (resp.status().as_u16(), resp.json().await.unwrap())
}

#[tokio::test]
async fn create_then_traverse_edge_round_trips() {
    let server = TestServer::spawn().await;

    let (status, body) = post_json(
        &server,
        "/edges",
        serde_json::json!({"from_id": 1, "to_id": 2, "edge_type": "KNOWS", "props": {"since": 2020}}),
    )
    .await;
    assert_eq!(status, 201);
    let page_id = body["row_id"]["page_id"].as_u64().unwrap();
    let slot = body["row_id"]["slot"].as_u64().unwrap();

    let client = reqwest::Client::new();
    let resp = client
        .get(server.url("/edges/from/1"))
        .header("Authorization", format!("Bearer {}", valid_token()))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    let edges = body["edges"].as_array().unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0]["to_id"], 2);
    assert_eq!(edges[0]["edge_type"], "KNOWS");

    let delete_resp = client
        .delete(server.url(&format!("/edges/{page_id}/{slot}")))
        .header("Authorization", format!("Bearer {}", valid_token()))
        .json(&serde_json::json!({ "from_id": 1 }))
        .send()
        .await
        .unwrap();
    assert_eq!(delete_resp.status(), 204);

    let resp = client
        .get(server.url("/edges/from/1"))
        .header("Authorization", format!("Bearer {}", valid_token()))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert!(body["edges"].as_array().unwrap().is_empty());
}
