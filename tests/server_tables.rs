//! `GET /tables` schema introspection (S1, studio UI). A real server on an
//! ephemeral port, real `reqwest` calls — proves the endpoint enumerates user
//! tables with their columns, hides internal `__…__` tables, and is auth-gated
//! exactly like every other data-plane route.

#[path = "server_common/mod.rs"]
mod server_common;

use serde_json::Value;
use server_common::{valid_token, TestServer};

/// Create a couple of tables via `/sql`, then assert `GET /tables` reports them
/// with the right columns / types / nullability / index, sorted by name.
#[tokio::test]
async fn lists_user_tables_with_columns() {
    let server = TestServer::spawn().await;
    let client = reqwest::Client::new();
    let auth = format!("Bearer {}", valid_token());

    for sql in [
        "CREATE TABLE users (id INT PRIMARY KEY, email TEXT NOT NULL, bio TEXT)",
        "CREATE TABLE docs (id INT, embedding VECTOR(4))",
        "CREATE INDEX docs_emb ON docs USING HNSW (embedding)",
    ] {
        let resp = client
            .post(server.url("/sql"))
            .header("Authorization", &auth)
            .json(&serde_json::json!({ "sql": sql }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "setup SQL failed: {sql}");
    }

    let resp = client
        .get(server.url("/tables"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    let tables = body.as_array().expect("response is a JSON array");

    // Only the two user tables — internal `__edges__`/`__lobs__`/… are hidden.
    assert_eq!(tables.len(), 2, "unexpected tables: {tables:?}");
    // Deterministic sort by name: docs before users.
    assert_eq!(tables[0]["name"], "docs");
    assert_eq!(tables[1]["name"], "users");

    // `docs`: id (int, nullable, no index), embedding (vector(4), hnsw index).
    let docs_cols = tables[0]["columns"].as_array().unwrap();
    assert_eq!(docs_cols.len(), 2);
    assert_eq!(docs_cols[0]["name"], "id");
    assert_eq!(docs_cols[0]["type"], "int");
    assert_eq!(docs_cols[0]["nullable"], true);
    assert_eq!(docs_cols[0]["index"], Value::Null);
    assert_eq!(docs_cols[1]["name"], "embedding");
    assert_eq!(docs_cols[1]["type"], "vector(4)");
    assert_eq!(docs_cols[1]["index"], "hnsw");

    // `users`: PRIMARY KEY and NOT NULL columns are non-nullable.
    let users_cols = tables[1]["columns"].as_array().unwrap();
    assert_eq!(users_cols.len(), 3);
    assert_eq!(users_cols[0]["name"], "id");
    assert_eq!(users_cols[0]["type"], "int");
    assert_eq!(users_cols[0]["nullable"], false); // PRIMARY KEY
    assert_eq!(users_cols[1]["name"], "email");
    assert_eq!(users_cols[1]["nullable"], false); // NOT NULL
    assert_eq!(users_cols[2]["name"], "bio");
    assert_eq!(users_cols[2]["nullable"], true);
}

/// A fresh database exposes no user tables — the internal engine tables that
/// always exist (`__edges__`, `__lobs__`, …) must not leak into the response.
#[tokio::test]
async fn hides_internal_tables_on_fresh_db() {
    let server = TestServer::spawn().await;
    let client = reqwest::Client::new();
    let auth = format!("Bearer {}", valid_token());

    let resp = client
        .get(server.url("/tables"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    let tables = body.as_array().unwrap();
    assert!(
        tables.is_empty(),
        "fresh DB should expose no user tables, got: {tables:?}"
    );
}

/// The route is auth-gated: a request with no bearer token is rejected with
/// `401 UNAUTHORIZED`, exactly like every other data-plane route.
#[tokio::test]
async fn requires_auth() {
    let server = TestServer::spawn().await;
    let client = reqwest::Client::new();

    let resp = client.get(server.url("/tables")).send().await.unwrap();
    assert_eq!(resp.status(), 401);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["code"], "UNAUTHORIZED");
}
