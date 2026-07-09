//! Per-user authorization over HTTP (P6.e): a superuser bootstrap creates a
//! restricted user; that user's JWT is enforced on `/sql`.

#[path = "server_common/mod.rs"]
mod server_common;

use serde_json::json;
use server_common::{token_for, valid_token, TestServer};

async fn sql(client: &reqwest::Client, url: String, token: &str, sql: &str) -> reqwest::Response {
    client
        .post(url)
        .header("Authorization", format!("Bearer {token}"))
        .json(&json!({ "sql": sql }))
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn per_user_privileges_enforced() {
    let server = TestServer::spawn().await;
    let client = reqwest::Client::new();
    let admin = valid_token(); // open-mode superuser until users exist

    // Bootstrap: create a superuser, a table, and a restricted user.
    assert_eq!(
        sql(
            &client,
            server.url("/sql"),
            &admin,
            "CREATE USER root SUPERUSER"
        )
        .await
        .status(),
        200
    );
    let root = token_for("root");
    assert_eq!(
        sql(
            &client,
            server.url("/sql"),
            &root,
            "CREATE TABLE t (id INT)"
        )
        .await
        .status(),
        200
    );
    assert_eq!(
        sql(
            &client,
            server.url("/sql"),
            &root,
            "INSERT INTO t (id) VALUES (1)"
        )
        .await
        .status(),
        200
    );
    assert_eq!(
        sql(&client, server.url("/sql"), &root, "CREATE USER bob")
            .await
            .status(),
        200
    );

    // bob has no privileges → SELECT is 403.
    let bob = token_for("bob");
    assert_eq!(
        sql(&client, server.url("/sql"), &bob, "SELECT id FROM t")
            .await
            .status(),
        403
    );

    // Grant SELECT → bob can now read.
    assert_eq!(
        sql(
            &client,
            server.url("/sql"),
            &root,
            "GRANT SELECT ON t TO bob"
        )
        .await
        .status(),
        200
    );
    assert_eq!(
        sql(&client, server.url("/sql"), &bob, "SELECT id FROM t")
            .await
            .status(),
        200
    );

    // bob still can't write or run DDL.
    assert_eq!(
        sql(
            &client,
            server.url("/sql"),
            &bob,
            "INSERT INTO t (id) VALUES (2)"
        )
        .await
        .status(),
        403
    );
    assert_eq!(
        sql(&client, server.url("/sql"), &bob, "CREATE USER carol")
            .await
            .status(),
        403
    );
}
