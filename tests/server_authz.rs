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

// ── POST /auth/preview (item-24 Z6) ─────────────────────────────────────────

/// `POST /auth/preview` requires a superuser JWT. A non-superuser caller
/// must receive 403 Forbidden.
#[tokio::test]
async fn post_auth_preview_requires_superuser() {
    let server = TestServer::spawn().await;
    let client = reqwest::Client::new();
    let admin = valid_token();

    // Bootstrap: create a superuser, a table, and a restricted user.
    sql(
        &client,
        server.url("/sql"),
        &admin,
        "CREATE USER root SUPERUSER",
    )
    .await;
    let root = token_for("root");
    sql(
        &client,
        server.url("/sql"),
        &root,
        "CREATE TABLE t (id INT)",
    )
    .await;
    sql(&client, server.url("/sql"), &root, "CREATE USER analyst").await;
    sql(
        &client,
        server.url("/sql"),
        &root,
        "GRANT SELECT ON t TO analyst",
    )
    .await;

    // analyst (non-superuser) calls /auth/preview → 403.
    let analyst = token_for("analyst");
    let resp = client
        .post(server.url("/auth/preview"))
        .header("Authorization", format!("Bearer {analyst}"))
        .json(&serde_json::json!({ "as_role": "analyst", "sql": "SELECT id FROM t" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        403,
        "non-superuser must get 403 for /auth/preview"
    );
}

/// Superuser can call `POST /auth/preview` and sees results filtered by the
/// named role's RLS policy (including `current_user()` substitution).
#[tokio::test]
async fn post_auth_preview_as_role() {
    let server = TestServer::spawn().await;
    let client = reqwest::Client::new();
    let admin = valid_token();

    // Bootstrap superuser.
    sql(
        &client,
        server.url("/sql"),
        &admin,
        "CREATE USER root SUPERUSER",
    )
    .await;
    let root = token_for("root");

    // Table with an owner column.
    sql(
        &client,
        server.url("/sql"),
        &root,
        "CREATE TABLE posts (id INT, owner TEXT)",
    )
    .await;

    // Roles and grants.
    sql(&client, server.url("/sql"), &root, "CREATE USER alice").await;
    sql(
        &client,
        server.url("/sql"),
        &root,
        "GRANT SELECT ON posts TO alice",
    )
    .await;

    // Insert rows as root.
    sql(
        &client,
        server.url("/sql"),
        &root,
        "INSERT INTO posts (id, owner) VALUES (1, 'alice'), (2, 'root')",
    )
    .await;

    // CREATE POLICY with current_user (bare keyword — parens form is not valid SQL here).
    let policy_resp = sql(
        &client,
        server.url("/sql"),
        &root,
        "CREATE POLICY p ON posts FOR SELECT USING (owner = current_user)",
    )
    .await;
    assert_eq!(policy_resp.status(), 200, "CREATE POLICY must succeed");

    // Superuser calls /auth/preview as "alice" — should see only alice's row.
    let resp = client
        .post(server.url("/auth/preview"))
        .header("Authorization", format!("Bearer {root}"))
        .json(&serde_json::json!({ "as_role": "alice", "sql": "SELECT id, owner FROM posts" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "preview must succeed for superuser");
    let body: serde_json::Value = resp.json().await.unwrap();
    let rows = body["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "alice should see only 1 row: {body}");
    assert_eq!(rows[0][1], "alice", "the visible row must belong to alice");
}
