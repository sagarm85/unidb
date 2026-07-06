//! `POST /cypher` (M5.d): a MATCH query against seeded edges, asserting the
//! JSON shape of `Rows` results matches `/sql`'s (same `exec_result_to_json`
//! conversion path).

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
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap();
    (status, body)
}

#[tokio::test]
async fn match_where_return_finds_seeded_edges() {
    let server = TestServer::spawn().await;

    for (from, to) in [(1, 2), (1, 3), (99, 100)] {
        let (status, _) = post_json(
            &server,
            "/edges",
            serde_json::json!({
                "from_id": from,
                "to_id": to,
                "edge_type": "KNOWS",
                "props": {}
            }),
        )
        .await;
        assert_eq!(status, 201);
    }

    let (status, body) = post_json(
        &server,
        "/cypher",
        serde_json::json!({ "query": "MATCH (a)-[:KNOWS]->(b) WHERE a = 1 RETURN b" }),
    )
    .await;
    assert_eq!(status, 200);
    let mut to_ids: Vec<i64> = body["results"][0]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row[0].as_i64().unwrap())
        .collect();
    to_ids.sort();
    assert_eq!(to_ids, vec![2, 3]);
}
