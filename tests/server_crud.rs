//! Raw row CRUD over HTTP (M5.d). A real server on an ephemeral port, real
//! `reqwest` calls — proves the REST layer's `begin -> execute ->
//! commit-or-abort` wrapping actually round-trips data correctly, not just
//! that it compiles.

#[path = "server_common/mod.rs"]
mod server_common;

use serde_json::Value;
use server_common::{valid_token, TestServer};

#[tokio::test]
async fn insert_get_update_delete_round_trip() {
    let server = TestServer::spawn().await;
    let client = reqwest::Client::new();
    let auth = format!("Bearer {}", valid_token());

    let insert_resp = client
        .post(server.url("/rows"))
        .header("Authorization", &auth)
        .body("hello world")
        .send()
        .await
        .unwrap();
    assert_eq!(insert_resp.status(), 201);
    let body: Value = insert_resp.json().await.unwrap();
    let page_id = body["row_id"]["page_id"].as_u64().unwrap();
    let slot = body["row_id"]["slot"].as_u64().unwrap();

    let get_resp = client
        .get(server.url(&format!("/rows/{page_id}/{slot}")))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(get_resp.status(), 200);
    assert_eq!(get_resp.bytes().await.unwrap(), "hello world".as_bytes());

    let put_resp = client
        .put(server.url(&format!("/rows/{page_id}/{slot}")))
        .header("Authorization", &auth)
        .body("updated")
        .send()
        .await
        .unwrap();
    assert_eq!(put_resp.status(), 200);
    let new_id: Value = put_resp.json().await.unwrap();
    let new_page = new_id["row_id"]["page_id"].as_u64().unwrap();
    let new_slot = new_id["row_id"]["slot"].as_u64().unwrap();

    let get_updated = client
        .get(server.url(&format!("/rows/{new_page}/{new_slot}")))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(get_updated.bytes().await.unwrap(), "updated".as_bytes());

    let delete_resp = client
        .delete(server.url(&format!("/rows/{new_page}/{new_slot}")))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(delete_resp.status(), 204);
}

#[tokio::test]
async fn get_on_deleted_row_returns_404() {
    let server = TestServer::spawn().await;
    let client = reqwest::Client::new();
    let auth = format!("Bearer {}", valid_token());

    let insert_resp = client
        .post(server.url("/rows"))
        .header("Authorization", &auth)
        .body("transient")
        .send()
        .await
        .unwrap();
    let body: Value = insert_resp.json().await.unwrap();
    let page_id = body["row_id"]["page_id"].as_u64().unwrap();
    let slot = body["row_id"]["slot"].as_u64().unwrap();

    client
        .delete(server.url(&format!("/rows/{page_id}/{slot}")))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();

    let get_resp = client
        .get(server.url(&format!("/rows/{page_id}/{slot}")))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(get_resp.status(), 404);
    let body: Value = get_resp.json().await.unwrap();
    assert_eq!(body["code"], "NOT_FOUND");
}
