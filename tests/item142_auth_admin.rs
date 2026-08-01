#![cfg(feature = "server")]
//! Item 142 — Auth admin API (user management).
//!
//! Follows the `tests/item141_webhooks.rs`/`tests/item140_realtime_authz.rs`
//! harness pattern: a real `unidb-server` (`TestServer`, dev-login enabled so
//! `POST /auth/login`/`/auth/refresh` are live) plus plain `reqwest` calls —
//! no mocked engine, no real network beyond loopback.
//!
//! Test matrix (mirrors the backlog doc's acceptance list):
//!   (a) superuser_list_with_limit_offset_and_total
//!   (b) non_superuser_gets_403_on_every_admin_route
//!   (c) create_user_then_login_works
//!   (d) patch_banned_true_blocks_login_and_revokes_sessions
//!   (e) patch_banned_false_restores_login
//!   (f) patch_password_old_fails_new_works_and_revokes_sessions
//!   (g) app_and_user_metadata_round_trip_through_create_get_patch
//!   (h) list_and_get_never_include_hash_or_token
//!   (i) delete_removes_the_user
//!   (j) delete_last_superuser_is_blocked
//!   (k) get_unknown_user_returns_404
//!   (l) patch_superuser_demoting_last_superuser_is_blocked (bonus safety net
//!       shared with (j) — see `RoleStore::set_superuser`'s doc comment)

use reqwest::StatusCode;
use serde_json::{json, Value};

#[path = "server_common/mod.rs"]
mod server_common;
use server_common::TestServer;

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

async fn sql(server: &TestServer, token: &str, sql_text: &str) -> (u16, Value) {
    let resp = client()
        .post(server.url("/sql"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&json!({ "sql": sql_text }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap();
    (status, body)
}

/// Bootstraps a `root` superuser via the open-mode bootstrap token — the
/// standard cast for the admin-route tests below (mirrors `item141_
/// webhooks.rs::setup_root`).
async fn setup_root(server: &TestServer) -> String {
    let bootstrap = server_common::valid_token();
    assert_eq!(
        sql(server, &bootstrap, "CREATE USER root SUPERUSER")
            .await
            .0,
        200
    );
    server_common::token_for("root")
}

async fn login(server: &TestServer, username: &str, password: &str) -> reqwest::Response {
    client()
        .post(server.url("/auth/login"))
        .json(&json!({ "username": username, "password": password }))
        .send()
        .await
        .unwrap()
}

async fn refresh(server: &TestServer, refresh_token: &str) -> reqwest::Response {
    client()
        .post(server.url("/auth/refresh"))
        .json(&json!({ "refresh_token": refresh_token }))
        .send()
        .await
        .unwrap()
}

async fn admin_list(
    server: &TestServer,
    token: &str,
    limit: Option<usize>,
    offset: Option<usize>,
) -> (u16, Value) {
    let mut query = Vec::new();
    if let Some(l) = limit {
        query.push(format!("limit={l}"));
    }
    if let Some(o) = offset {
        query.push(format!("offset={o}"));
    }
    let mut url = server.url("/auth/admin/users");
    if !query.is_empty() {
        url = format!("{url}?{}", query.join("&"));
    }
    let resp = client()
        .get(url)
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap();
    (status, body)
}

async fn admin_get(server: &TestServer, token: &str, username: &str) -> (u16, Value) {
    let resp = client()
        .get(server.url(&format!("/auth/admin/users/{username}")))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap();
    (status, body)
}

async fn admin_create(server: &TestServer, token: &str, body: Value) -> (u16, Value) {
    let resp = client()
        .post(server.url("/auth/admin/users"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap();
    (status, body)
}

async fn admin_patch(
    server: &TestServer,
    token: &str,
    username: &str,
    body: Value,
) -> (u16, Value) {
    let resp = client()
        .patch(server.url(&format!("/auth/admin/users/{username}")))
        .header("Authorization", format!("Bearer {token}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap();
    (status, body)
}

async fn admin_delete(server: &TestServer, token: &str, username: &str) -> u16 {
    client()
        .delete(server.url(&format!("/auth/admin/users/{username}")))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

// ── (a) superuser lists users with limit/offset and sees a total ───────────

#[tokio::test]
async fn superuser_list_with_limit_offset_and_total() {
    let server = TestServer::spawn_with_dev_login().await;
    let root = setup_root(&server).await;

    for name in ["alice", "bob", "carol"] {
        assert_eq!(
            admin_create(&server, &root, json!({ "username": name }))
                .await
                .0,
            201
        );
    }
    // root + alice + bob + carol = 4 total.
    let (status, body) = admin_list(&server, &root, None, None).await;
    assert_eq!(status, 200);
    assert_eq!(body["total"], 4);
    assert_eq!(body["users"].as_array().unwrap().len(), 4);

    let (status, body) = admin_list(&server, &root, Some(2), Some(1)).await;
    assert_eq!(status, 200);
    assert_eq!(body["total"], 4, "total is always the unpaginated count");
    assert_eq!(body["users"].as_array().unwrap().len(), 2);

    let (status, body) = admin_list(&server, &root, Some(50), Some(10)).await;
    assert_eq!(status, 200);
    assert_eq!(body["total"], 4);
    assert!(body["users"].as_array().unwrap().is_empty());
}

// ── (b) a non-superuser gets 403 on every /auth/admin/* route ──────────────

#[tokio::test]
async fn non_superuser_gets_403_on_every_admin_route() {
    let server = TestServer::spawn_with_dev_login().await;
    let root = setup_root(&server).await;
    assert_eq!(
        admin_create(
            &server,
            &root,
            json!({ "username": "plain", "password": "pw12345" })
        )
        .await
        .0,
        201
    );
    let plain = server_common::token_for("plain");

    assert_eq!(admin_list(&server, &plain, None, None).await.0, 403);
    assert_eq!(admin_get(&server, &plain, "root").await.0, 403);
    assert_eq!(
        admin_create(&server, &plain, json!({ "username": "nope" }))
            .await
            .0,
        403
    );
    assert_eq!(
        admin_patch(&server, &plain, "root", json!({ "banned": true }))
            .await
            .0,
        403
    );
    assert_eq!(admin_delete(&server, &plain, "root").await, 403);
}

// ── (c) create → the user can log in ────────────────────────────────────────

#[tokio::test]
async fn create_user_then_login_works() {
    let server = TestServer::spawn_with_dev_login().await;
    let root = setup_root(&server).await;

    let (status, body) = admin_create(
        &server,
        &root,
        json!({ "username": "dana", "password": "danas-pw-123" }),
    )
    .await;
    assert_eq!(status, 201);
    assert_eq!(body["username"], "dana");
    assert_eq!(body["is_superuser"], false);
    assert_eq!(body["banned"], false);

    let resp = login(&server, "dana", "danas-pw-123").await;
    assert_eq!(resp.status(), StatusCode::OK);
}

// ── (d) PATCH banned=true → login 403 USER_BANNED + sessions revoked ───────

#[tokio::test]
async fn patch_banned_true_blocks_login_and_revokes_sessions() {
    let server = TestServer::spawn_with_dev_login().await;
    let root = setup_root(&server).await;
    assert_eq!(
        admin_create(
            &server,
            &root,
            json!({ "username": "erin", "password": "erins-pw-123" }),
        )
        .await
        .0,
        201
    );

    // Log in first, while unbanned, and keep the refresh token — banning
    // must revoke it.
    let login_body: Value = login(&server, "erin", "erins-pw-123")
        .await
        .json()
        .await
        .unwrap();
    let refresh_token = login_body["refresh_token"].as_str().unwrap().to_string();

    let (status, body) = admin_patch(&server, &root, "erin", json!({ "banned": true })).await;
    assert_eq!(status, 200);
    assert_eq!(body["banned"], true);

    let resp = login(&server, "erin", "erins-pw-123").await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let err: Value = resp.json().await.unwrap();
    assert_eq!(err["code"], "USER_BANNED");

    // The pre-ban refresh token no longer works (banning revoked it).
    let refresh_resp = refresh(&server, &refresh_token).await;
    assert_eq!(refresh_resp.status(), StatusCode::UNAUTHORIZED);
}

// ── (e) PATCH banned=false → login works again ──────────────────────────────

#[tokio::test]
async fn patch_banned_false_restores_login() {
    let server = TestServer::spawn_with_dev_login().await;
    let root = setup_root(&server).await;
    assert_eq!(
        admin_create(
            &server,
            &root,
            json!({ "username": "frank", "password": "franks-pw-123", "banned": true }),
        )
        .await
        .0,
        201
    );

    let resp = login(&server, "frank", "franks-pw-123").await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let (status, body) = admin_patch(&server, &root, "frank", json!({ "banned": false })).await;
    assert_eq!(status, 200);
    assert_eq!(body["banned"], false);

    let resp = login(&server, "frank", "franks-pw-123").await;
    assert_eq!(resp.status(), StatusCode::OK);
}

// ── (f) PATCH password → old fails, new works, sessions revoked ────────────

#[tokio::test]
async fn patch_password_old_fails_new_works_and_revokes_sessions() {
    let server = TestServer::spawn_with_dev_login().await;
    let root = setup_root(&server).await;
    assert_eq!(
        admin_create(
            &server,
            &root,
            json!({ "username": "gina", "password": "old-password-123" }),
        )
        .await
        .0,
        201
    );

    let login_body: Value = login(&server, "gina", "old-password-123")
        .await
        .json()
        .await
        .unwrap();
    let refresh_token = login_body["refresh_token"].as_str().unwrap().to_string();

    let (status, _) = admin_patch(
        &server,
        &root,
        "gina",
        json!({ "password": "new-password-456" }),
    )
    .await;
    assert_eq!(status, 200);

    let resp = login(&server, "gina", "old-password-123").await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let resp = login(&server, "gina", "new-password-456").await;
    assert_eq!(resp.status(), StatusCode::OK);

    // The pre-change refresh token was revoked by the password change.
    let refresh_resp = refresh(&server, &refresh_token).await;
    assert_eq!(refresh_resp.status(), StatusCode::UNAUTHORIZED);
}

// ── (g) app_metadata/user_metadata round-trip through create/get/patch ─────

#[tokio::test]
async fn app_and_user_metadata_round_trip_through_create_get_patch() {
    let server = TestServer::spawn_with_dev_login().await;
    let root = setup_root(&server).await;

    let (status, body) = admin_create(
        &server,
        &root,
        json!({
            "username": "hank",
            "app_metadata": {"plan": "pro"},
            "user_metadata": {"nickname": "hanky"},
        }),
    )
    .await;
    assert_eq!(status, 201);
    assert_eq!(body["app_metadata"]["plan"], "pro");
    assert_eq!(body["user_metadata"]["nickname"], "hanky");

    let (status, body) = admin_get(&server, &root, "hank").await;
    assert_eq!(status, 200);
    assert_eq!(body["app_metadata"]["plan"], "pro");
    assert_eq!(body["user_metadata"]["nickname"], "hanky");

    let (status, body) = admin_patch(
        &server,
        &root,
        "hank",
        json!({
            "app_metadata": {"plan": "enterprise"},
            "user_metadata": {"nickname": "hank-the-tank"},
        }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["app_metadata"]["plan"], "enterprise");
    assert_eq!(body["user_metadata"]["nickname"], "hank-the-tank");

    let (status, body) = admin_get(&server, &root, "hank").await;
    assert_eq!(status, 200);
    assert_eq!(body["app_metadata"]["plan"], "enterprise");
    assert_eq!(body["user_metadata"]["nickname"], "hank-the-tank");
}

// ── (h) list/get never include a hash or a token ────────────────────────────

#[tokio::test]
async fn list_and_get_never_include_hash_or_token() {
    let server = TestServer::spawn_with_dev_login().await;
    let root = setup_root(&server).await;
    assert_eq!(
        admin_create(
            &server,
            &root,
            json!({ "username": "ivan", "password": "ivans-super-secret-pw" }),
        )
        .await
        .0,
        201
    );

    let (_, list_body) = admin_list(&server, &root, None, None).await;
    let list_text = list_body.to_string();
    assert!(!list_text.contains("ivans-super-secret-pw"));
    assert!(!list_text.contains("$argon2"));
    assert!(list_body["users"][0].get("password").is_none());
    assert!(list_body["users"][0].get("password_hash").is_none());
    assert!(list_body["users"][0].get("refresh_token").is_none());
    assert!(list_body["users"][0].get("token").is_none());

    let (_, get_body) = admin_get(&server, &root, "ivan").await;
    let get_text = get_body.to_string();
    assert!(!get_text.contains("ivans-super-secret-pw"));
    assert!(!get_text.contains("$argon2"));
    assert!(get_body.get("password").is_none());
    assert!(get_body.get("password_hash").is_none());
    assert!(get_body.get("refresh_token").is_none());
    assert!(get_body.get("token").is_none());
}

// ── (i) DELETE removes the user ─────────────────────────────────────────────

#[tokio::test]
async fn delete_removes_the_user() {
    let server = TestServer::spawn_with_dev_login().await;
    let root = setup_root(&server).await;
    assert_eq!(
        admin_create(
            &server,
            &root,
            json!({ "username": "jill", "password": "jills-pw-123" }),
        )
        .await
        .0,
        201
    );
    assert_eq!(admin_get(&server, &root, "jill").await.0, 200);

    assert_eq!(admin_delete(&server, &root, "jill").await, 204);

    assert_eq!(admin_get(&server, &root, "jill").await.0, 404);
    let resp = login(&server, "jill", "jills-pw-123").await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// ── (j) DELETE the last superuser is blocked (self-lockout guard) ──────────

#[tokio::test]
async fn delete_last_superuser_is_blocked() {
    let server = TestServer::spawn_with_dev_login().await;
    let root = setup_root(&server).await;

    let status = admin_delete(&server, &root, "root").await;
    assert_eq!(status, 403);

    // root must still exist and still be able to administer.
    let (status, body) = admin_get(&server, &root, "root").await;
    assert_eq!(status, 200);
    assert_eq!(body["is_superuser"], true);

    // With a second superuser present, dropping the first now succeeds.
    assert_eq!(
        sql(&server, &root, "CREATE USER root2 SUPERUSER").await.0,
        200
    );
    assert_eq!(admin_delete(&server, &root, "root").await, 204);
}

// ── (k) GET an unknown user returns 404 ─────────────────────────────────────

#[tokio::test]
async fn get_unknown_user_returns_404() {
    let server = TestServer::spawn_with_dev_login().await;
    let root = setup_root(&server).await;

    let (status, _) = admin_get(&server, &root, "nobody-here").await;
    assert_eq!(status, 404);
}

// ── (l) PATCH superuser=false demoting the last superuser is blocked ───────

#[tokio::test]
async fn patch_superuser_demoting_last_superuser_is_blocked() {
    let server = TestServer::spawn_with_dev_login().await;
    let root = setup_root(&server).await;

    let (status, _) = admin_patch(&server, &root, "root", json!({ "superuser": false })).await;
    assert_eq!(status, 403);

    let (status, body) = admin_get(&server, &root, "root").await;
    assert_eq!(status, 200);
    assert_eq!(body["is_superuser"], true);
}
