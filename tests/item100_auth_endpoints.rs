// Item 100 — GET /auth/meta + POST /auth/login + GET /auth/whoami
//
// Test matrix:
//   1. auth_meta_returns_static_fields  — GET /auth/meta works without JWT
//   2. auth_meta_open_mode_true_when_no_users — open_mode before CREATE USER
//   3. auth_meta_open_mode_false_after_user_created
//   4. auth_meta_dev_login_flag_reflects_config
//   5. auth_login_disabled_when_flag_off — returns 4xx without UNIDB_DEV_LOGIN
//   6. auth_login_issues_valid_token — POST /auth/login → token verifiable
//   7. auth_login_unknown_user_returns_4xx
//   8. auth_whoami_returns_user_and_grants
//   9. auth_whoami_implicit_superuser_has_no_sub

#![cfg(feature = "server")]

use reqwest::StatusCode;
use serde_json::Value;

#[path = "server_common/mod.rs"]
mod server_common;
use server_common::TestServer;

// ── test 1: GET /auth/meta is reachable without authentication ────────────────

#[tokio::test]
async fn auth_meta_returns_static_fields() {
    let server = TestServer::spawn().await;
    let client = reqwest::Client::new();

    let resp = client
        .get(server.url("/auth/meta"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: Value = resp.json().await.unwrap();
    // Static fields always present.
    let pt = body["privilege_types"].as_array().unwrap();
    assert!(pt.iter().any(|v| v == "SELECT"), "missing SELECT privilege");
    assert!(pt.iter().any(|v| v == "INSERT"), "missing INSERT privilege");
    assert!(pt.iter().any(|v| v == "UPDATE"), "missing UPDATE privilege");
    assert!(pt.iter().any(|v| v == "DELETE"), "missing DELETE privilege");

    let po = body["policy_operations"].as_array().unwrap();
    assert!(po.iter().any(|v| v == "ALL"), "missing ALL policy op");

    let ct = body["catalog_tables"].as_array().unwrap();
    assert!(
        ct.iter()
            .any(|v| v == "information_schema.tables"),
        "information_schema.tables missing from catalog_tables"
    );
    assert!(
        ct.iter().any(|v| v == "unidb_catalog.policies"),
        "unidb_catalog.policies missing from catalog_tables"
    );
}

// ── test 2: open_mode = true before any user is created ───────────────────────

#[tokio::test]
async fn auth_meta_open_mode_true_when_no_users() {
    let server = TestServer::spawn().await;
    let client = reqwest::Client::new();

    let resp = client
        .get(server.url("/auth/meta"))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body["open_mode"].as_bool(),
        Some(true),
        "open_mode should be true before any user exists"
    );
}

// ── test 3: open_mode = false after CREATE USER ────────────────────────────────

#[tokio::test]
async fn auth_meta_open_mode_false_after_user_created() {
    let server = TestServer::spawn().await;
    let client = reqwest::Client::new();
    let tok = server_common::valid_token();

    // Create a user.
    client
        .post(server.url("/sql"))
        .header("Authorization", format!("Bearer {tok}"))
        .json(&serde_json::json!({"sql": "CREATE USER alice SUPERUSER"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    let resp = client
        .get(server.url("/auth/meta"))
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body["open_mode"].as_bool(),
        Some(false),
        "open_mode should be false once a user exists"
    );
}

// ── test 4: dev_login_enabled reflects the server config ─────────────────────

#[tokio::test]
async fn auth_meta_dev_login_flag_reflects_config() {
    // Default server: dev_login_enabled = false.
    let server_off = TestServer::spawn().await;
    let client = reqwest::Client::new();
    let body: Value = client
        .get(server_off.url("/auth/meta"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        body["dev_login_enabled"].as_bool(),
        Some(false),
        "dev_login_enabled should be false in default config"
    );

    // Dev-login server: dev_login_enabled = true.
    let server_on = TestServer::spawn_with_dev_login().await;
    let body: Value = client
        .get(server_on.url("/auth/meta"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        body["dev_login_enabled"].as_bool(),
        Some(true),
        "dev_login_enabled should be true when spawned with dev login"
    );
}

// ── test 5: POST /auth/login returns 4xx when flag is off ─────────────────────

#[tokio::test]
async fn auth_login_disabled_when_flag_off() {
    let server = TestServer::spawn().await;
    let client = reqwest::Client::new();

    let resp = client
        .post(server.url("/auth/login"))
        .json(&serde_json::json!({"username": "alice"}))
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_client_error(),
        "login should return 4xx when UNIDB_DEV_LOGIN is off, got {}",
        resp.status()
    );
}

// ── test 6: POST /auth/login issues a token the server accepts ────────────────

#[tokio::test]
async fn auth_login_issues_valid_token() {
    let server = TestServer::spawn_with_dev_login().await;
    let client = reqwest::Client::new();
    let tok = server_common::valid_token();

    // Create the user first.
    client
        .post(server.url("/sql"))
        .header("Authorization", format!("Bearer {tok}"))
        .json(&serde_json::json!({"sql": "CREATE USER alice"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    // Login.
    let resp: Value = client
        .post(server.url("/auth/login"))
        .json(&serde_json::json!({"username": "alice"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();

    let token = resp["token"].as_str().expect("response must have a token");
    assert!(!token.is_empty());
    assert_eq!(resp["expires_in"].as_u64(), Some(3600));

    // The issued token must work as a bearer token on the protected routes.
    let whoami_resp = client
        .get(server.url("/auth/whoami"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        whoami_resp.status(),
        StatusCode::OK,
        "issued token must be accepted by the server"
    );
    let whoami: Value = whoami_resp.json().await.unwrap();
    assert_eq!(
        whoami["user"].as_str(),
        Some("alice"),
        "whoami user should be alice"
    );
}

// ── test 7: POST /auth/login with unknown user returns 4xx ────────────────────

#[tokio::test]
async fn auth_login_unknown_user_returns_4xx() {
    let server = TestServer::spawn_with_dev_login().await;
    let client = reqwest::Client::new();

    let resp = client
        .post(server.url("/auth/login"))
        .json(&serde_json::json!({"username": "nobody"}))
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_client_error(),
        "login with unknown user should be 4xx, got {}",
        resp.status()
    );
}

// ── test 8: GET /auth/whoami returns user identity + grants ───────────────────

#[tokio::test]
async fn auth_whoami_returns_user_and_grants() {
    let server = TestServer::spawn().await;
    let client = reqwest::Client::new();
    let tok = server_common::valid_token();

    // Set up: create `test-user` as a superuser first (while still in open
    // mode), then create the table and alice, then grant SELECT to alice.
    // Creating test-user first ensures that after alice is created (which
    // ends open mode), the bearer token used for subsequent admin calls still
    // has superuser privileges.
    for sql in [
        "CREATE USER \"test-user\" SUPERUSER",
        "CREATE TABLE t (id INT)",
        "CREATE USER alice",
        "GRANT SELECT ON t TO alice",
    ] {
        client
            .post(server.url("/sql"))
            .header("Authorization", format!("Bearer {tok}"))
            .json(&serde_json::json!({"sql": sql}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
    }

    let alice_tok = server_common::token_for("alice");
    let resp: Value = client
        .get(server.url("/auth/whoami"))
        .header("Authorization", format!("Bearer {alice_tok}"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(resp["user"].as_str(), Some("alice"));
    assert_eq!(resp["is_superuser"].as_bool(), Some(false));
    assert_eq!(resp["open_mode"].as_bool(), Some(false));

    let privs = resp["privileges"].as_array().unwrap();
    let t_entry = privs.iter().find(|p| p["table"] == "t");
    assert!(t_entry.is_some(), "should have privilege entry for table t");
    let ops = t_entry.unwrap()["ops"].as_array().unwrap();
    assert!(
        ops.iter().any(|v| v == "SELECT"),
        "alice should have SELECT on t"
    );
}

// ── test 9: GET /auth/whoami with implicit superuser (no sub) ─────────────────

#[tokio::test]
async fn auth_whoami_implicit_superuser_has_no_sub() {
    let server = TestServer::spawn().await;
    let client = reqwest::Client::new();

    // valid_token() uses `sub = "test-user"` but no CREATE USER — before
    // any user exists, the server is in open mode and we get the token's sub.
    let resp: Value = client
        .get(server.url("/auth/whoami"))
        .header("Authorization", format!("Bearer {}", server_common::valid_token()))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();

    // In open mode the token sub is returned as-is.
    assert_eq!(resp["open_mode"].as_bool(), Some(true));
}
