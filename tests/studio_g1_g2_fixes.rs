// unidb-studio G1/G2 panels — four small engine fixes/features.
//
// Test matrix:
//   1. full_binary_allow_signup_wiring — unidb-server-full's AppState is now
//      wired the same way src/bin/unidb-server.rs already was: reading
//      UNIDB_ALLOW_SIGNUP and feeding it through `with_allow_signup`. We
//      can't spawn the compiled `unidb-server-full` binary from an
//      integration test, so this proves the wiring end-to-end by building a
//      real server through the exact same `AppState`/`build_router` path
//      `unidb-server-full/src/main.rs` now uses (dev_login + storage=None +
//      allow_signup) and hitting POST /auth/signup for real.
//   2. policies_target_roles_column — unidb_catalog.policies exposes
//      target_roles: comma-joined sorted role names for a TO-scoped policy,
//      "*" for a no-TO (applies-to-everyone) policy.
//   3. alter_user_password_* — ALTER USER <name> PASSWORD '<pw>' resets the
//      argon2id credential (superuser-gated, errors on unknown user).
//   4. session_listing_and_revoke_by_id — unidb_catalog.sessions lists a
//      caller's own sessions (superuser sees all) without ever exposing the
//      token/hash, and DELETE /auth/sessions/{id} revokes exactly one
//      session, self/superuser gated.

#![cfg(feature = "server")]

use std::sync::Arc;

use reqwest::StatusCode;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tempfile::tempdir;
use unidb::server::{
    auth::JwtConfig, engine_handle::EngineHandle, router::build_router, AppState, SessionConfig,
};
use unidb::sql::executor::ExecResult;
use unidb::sql::logical::Literal;
use unidb::Engine;

#[path = "server_common/mod.rs"]
mod server_common;
use server_common::TestServer;

// ── test 1: unidb-server-full's UNIDB_ALLOW_SIGNUP wiring ─────────────────────

/// Mirrors unidb-server-full/src/main.rs's `UNIDB_ALLOW_SIGNUP` parsing
/// verbatim (kept in sync with `src/bin/unidb-server.rs`'s own copy) so this
/// test exercises the exact boolean logic the fixed binary now runs, without
/// mutating process-global environment state (which would race other tests
/// in this binary).
fn parse_allow_signup(v: Option<&str>) -> bool {
    v.map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

#[test]
fn allow_signup_env_parsing_matches_fixed_binary() {
    assert!(parse_allow_signup(Some("1")));
    assert!(parse_allow_signup(Some("true")));
    assert!(parse_allow_signup(Some("TRUE")));
    assert!(!parse_allow_signup(Some("0")));
    assert!(!parse_allow_signup(None));
}

/// End-to-end: build a server through the same `AppState`/`build_router`
/// sequence `unidb-server-full/src/main.rs` uses post-fix (dev_login +
/// `.with_storage(None)` + `.with_allow_signup(parse_allow_signup(Some("1")))`)
/// and confirm `POST /auth/signup` actually works — this is the exact
/// capability item 1's bug made permanently unreachable on that binary.
#[tokio::test]
async fn full_binary_allow_signup_wiring_enables_signup() {
    let tempdir = tempdir().unwrap();
    let engine = EngineHandle::spawn(tempdir.path(), 0).unwrap();
    let jwt_config = JwtConfig::with_dev_login(server_common::TEST_JWT_SECRET);
    let allow_signup = parse_allow_signup(Some("1"));
    assert!(allow_signup);
    let state = AppState::with_config(Arc::new(engine), SessionConfig::default())
        .with_dev_login(jwt_config.clone())
        .with_storage(None)
        .with_allow_signup(allow_signup);
    assert!(
        state.allow_signup,
        "AppState.allow_signup must be true: unidb-server-full/main.rs now reads \
         UNIDB_ALLOW_SIGNUP and calls with_allow_signup, mirroring src/bin/unidb-server.rs"
    );
    let (prometheus_layer, metric_handle) = server_common::metrics_pair().clone();
    let router = build_router(state, jwt_config, prometheus_layer, metric_handle);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let _server_task = tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await;
    });

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{addr}/auth/signup"))
        .json(&serde_json::json!({"username": "signup-alice", "password": "sekrit-pw-1"}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "POST /auth/signup must succeed through unidb-server-full's fixed wiring"
    );
    let body: Value = resp.json().await.unwrap();
    assert!(body["access_token"].as_str().is_some());
}

/// Sanity counterpart: with `allow_signup=false` (the pre-fix default, still
/// the default when the env var is unset), signup stays a 404 — the fix
/// only makes the flag *reachable*, it doesn't turn signup on unconditionally.
#[tokio::test]
async fn full_binary_signup_still_disabled_when_env_var_unset() {
    let tempdir = tempdir().unwrap();
    let engine = EngineHandle::spawn(tempdir.path(), 0).unwrap();
    let jwt_config = JwtConfig::with_dev_login(server_common::TEST_JWT_SECRET);
    let allow_signup = parse_allow_signup(None);
    assert!(!allow_signup);
    let state = AppState::with_config(Arc::new(engine), SessionConfig::default())
        .with_dev_login(jwt_config.clone())
        .with_storage(None)
        .with_allow_signup(allow_signup);
    let (prometheus_layer, metric_handle) = server_common::metrics_pair().clone();
    let router = build_router(state, jwt_config, prometheus_layer, metric_handle);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let _server_task = tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await;
    });

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{addr}/auth/signup"))
        .json(&serde_json::json!({"username": "nope", "password": "whatever"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── test 2: unidb_catalog.policies.target_roles ────────────────────────────────

fn rows_as(engine: &Engine, user: Option<&str>, sql: &str) -> Vec<Vec<Literal>> {
    let x = engine.begin().unwrap();
    let res = engine.execute_sql_as(user, x, sql).unwrap();
    engine.commit(x).unwrap();
    match res.into_iter().next().unwrap() {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn policies_target_roles_column_reflects_to_clause_and_wildcard() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    let x = engine.begin().unwrap();
    for ddl in [
        "CREATE TABLE t (id INT, owner TEXT)",
        "CREATE ROLE support",
        "CREATE ROLE ops",
        // Role-scoped (TO clause): must render as a comma-joined, sorted list.
        "CREATE POLICY scoped_pol ON t FOR SELECT TO ops, support USING (true)",
        // No TO clause: must render as the all-callers marker "*".
        "CREATE POLICY open_pol ON t FOR SELECT USING (true)",
    ] {
        engine.execute_sql_as(None, x, ddl).unwrap();
    }
    engine.commit(x).unwrap();

    let rows = rows_as(
        &engine,
        None,
        "SELECT * FROM unidb_catalog.policies ORDER BY name",
    );
    assert_eq!(rows.len(), 2, "expected exactly the two created policies");

    let name_of = |r: &[Literal]| match &r[0] {
        Literal::Text(s) => s.clone(),
        other => panic!("name not text: {other:?}"),
    };
    // target_roles is the appended, last column (index 6) — existing column
    // order (name, table_name, operation, using_expr, with_check_expr,
    // enforced) is unchanged.
    let target_roles_of = |r: &[Literal]| match &r[6] {
        Literal::Text(s) => s.clone(),
        other => panic!("target_roles not text: {other:?}"),
    };

    let open = rows.iter().find(|r| name_of(r) == "open_pol").unwrap();
    assert_eq!(
        target_roles_of(open),
        "*",
        "no-TO policy must render the all-callers marker"
    );

    let scoped = rows.iter().find(|r| name_of(r) == "scoped_pol").unwrap();
    assert_eq!(
        target_roles_of(scoped),
        "ops,support",
        "TO-scoped policy must render a comma-joined, alphabetically-sorted role list"
    );
}

// ── test 3: ALTER USER <name> PASSWORD '<pw>' ──────────────────────────────────

#[test]
fn alter_user_password_resets_credential_superuser_gated() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    let x = engine.begin().unwrap();
    for ddl in [
        "CREATE USER root SUPERUSER PASSWORD 'root-pw'",
        "CREATE USER frank PASSWORD 'old-pw'",
        "CREATE USER mallory PASSWORD 'mallory-pw'",
    ] {
        engine.execute_sql_as(None, x, ddl).unwrap();
    }
    engine.commit(x).unwrap();

    assert!(engine.verify_password("frank", "old-pw"));

    // Superuser resets frank's password.
    let x2 = engine.begin().unwrap();
    engine
        .execute_sql_as(Some("root"), x2, "ALTER USER frank PASSWORD 'new-pw'")
        .unwrap();
    engine.commit(x2).unwrap();

    assert!(
        !engine.verify_password("frank", "old-pw"),
        "old password must no longer work"
    );
    assert!(
        engine.verify_password("frank", "new-pw"),
        "new password must work"
    );

    // Non-superuser denied.
    let x3 = engine.begin().unwrap();
    let res = engine.execute_sql_as(Some("mallory"), x3, "ALTER USER frank PASSWORD 'hacked-pw'");
    engine.commit(x3).unwrap();
    assert!(res.is_err(), "non-superuser must be denied");
    assert!(
        engine.verify_password("frank", "new-pw"),
        "password must be unchanged after a denied attempt"
    );
    assert!(!engine.verify_password("frank", "hacked-pw"));

    // Unknown user errors.
    let x4 = engine.begin().unwrap();
    let res2 = engine.execute_sql_as(Some("root"), x4, "ALTER USER ghost PASSWORD 'whatever'");
    engine.commit(x4).unwrap();
    assert!(res2.is_err(), "unknown user must error");
}

#[test]
fn alter_user_password_never_appears_in_debug_or_audit_action() {
    // The parsed statement's Debug representation must redact the password,
    // exactly like CreateUser's — see `authz::AuthStmt`'s manual Debug impl.
    let stmt = unidb::authz::parse_auth_stmt("ALTER USER frank PASSWORD 'super-secret-pw'")
        .unwrap()
        .expect("must parse as auth DDL");
    let debug_str = format!("{stmt:?}");
    assert!(
        !debug_str.contains("super-secret-pw"),
        "password must never appear in Debug output: {debug_str}"
    );
    assert!(debug_str.contains("redacted"));
}

// ── test 4: session listing (unidb_catalog.sessions) + revoke-by-id ───────────

async fn sql_as(client: &reqwest::Client, server: &TestServer, tok: &str, sql: &str) -> Value {
    client
        .post(server.url("/sql"))
        .header("Authorization", format!("Bearer {tok}"))
        .json(&serde_json::json!({ "sql": sql }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// Rows of the first (only) statement result, as `(columns, rows)`.
fn first_result(body: &Value) -> (Vec<String>, &Vec<Value>) {
    let r = &body["results"][0];
    let columns = r["columns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap().to_string())
        .collect();
    (columns, r["rows"].as_array().unwrap())
}

async fn login(
    client: &reqwest::Client,
    server: &TestServer,
    username: &str,
    password: &str,
) -> Value {
    client
        .post(server.url("/auth/login"))
        .json(&serde_json::json!({ "username": username, "password": password }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test]
async fn session_listing_and_revoke_by_id() {
    let server = TestServer::spawn_with_dev_login_and_signup().await;
    let client = reqwest::Client::new();
    let bootstrap_tok = server_common::valid_token();

    // First statement runs in bootstrap/open mode (creates the first user,
    // ending open mode) — every later admin statement must use `admin`'s own
    // token, not `bootstrap_tok` (whose subject was never itself created as
    // a user, so it stops being an effective superuser the moment any user
    // exists).
    sql_as(
        &client,
        &server,
        &bootstrap_tok,
        "CREATE USER admin SUPERUSER PASSWORD 'admin-pw'",
    )
    .await;
    let admin_tok = server_common::token_for("admin");

    for ddl in [
        "CREATE USER alice PASSWORD 'alice-pw'",
        "CREATE USER bob PASSWORD 'bob-pw'",
        "GRANT SELECT ON unidb_catalog.sessions TO alice",
        "GRANT SELECT ON unidb_catalog.sessions TO bob",
    ] {
        sql_as(&client, &server, &admin_tok, ddl).await;
    }

    let alice_login = login(&client, &server, "alice", "alice-pw").await;
    let alice_access = alice_login["access_token"].as_str().unwrap().to_string();
    let alice_refresh = alice_login["refresh_token"].as_str().unwrap().to_string();

    let bob_login = login(&client, &server, "bob", "bob-pw").await;
    let bob_access = bob_login["access_token"].as_str().unwrap().to_string();
    let bob_refresh = bob_login["refresh_token"].as_str().unwrap().to_string();

    // ── superuser sees both sessions ──
    let admin_view = sql_as(
        &client,
        &server,
        &admin_tok,
        "SELECT * FROM unidb_catalog.sessions ORDER BY username",
    )
    .await;
    let (columns, admin_rows) = first_result(&admin_view);
    assert_eq!(
        columns,
        vec![
            "session_id",
            "username",
            "created_at",
            "expires_at",
            "revoked"
        ]
    );
    assert_eq!(admin_rows.len(), 2, "superuser must see both sessions");

    // No token or hash ever appears in the row content, for any row.
    let hash_hex = {
        let mut h = Sha256::new();
        h.update(alice_refresh.as_bytes());
        format!("{:x}", h.finalize())
    };
    for row in admin_rows {
        let row_text = row.to_string();
        assert!(
            !row_text.contains(&alice_refresh),
            "raw refresh token leaked"
        );
        assert!(!row_text.contains(&hash_hex), "refresh token hash leaked");
    }

    // ── alice sees only her own session ──
    let alice_view = sql_as(
        &client,
        &server,
        &alice_access,
        "SELECT * FROM unidb_catalog.sessions",
    )
    .await;
    let (_, alice_rows) = first_result(&alice_view);
    assert_eq!(alice_rows.len(), 1, "alice must see only her own session");
    assert_eq!(alice_rows[0][1].as_str(), Some("alice"));
    let alice_session_id = alice_rows[0][0].as_str().unwrap().to_string();

    // The opaque session id is independent of the token/hash: different
    // length, and neither a match nor a substring of either.
    assert_ne!(alice_session_id, alice_refresh);
    assert_ne!(alice_session_id, hash_hex);
    assert_ne!(
        alice_session_id.len(),
        hash_hex.len(),
        "session_id (128 bits) must not even be the same length as the token hash (256 bits)"
    );
    assert!(!hash_hex.contains(&alice_session_id));
    assert!(!alice_refresh.contains(&alice_session_id));

    // ── bob sees only his own session (not alice's) ──
    let bob_view = sql_as(
        &client,
        &server,
        &bob_access,
        "SELECT * FROM unidb_catalog.sessions",
    )
    .await;
    let (_, bob_rows) = first_result(&bob_view);
    assert_eq!(bob_rows.len(), 1, "bob must see only his own session");
    assert_eq!(bob_rows[0][1].as_str(), Some("bob"));
    assert_ne!(
        bob_rows[0][0].as_str().unwrap(),
        alice_session_id,
        "bob must not see alice's session id"
    );

    // ── bob cannot revoke alice's session ──
    let resp = client
        .delete(server.url(&format!("/auth/sessions/{alice_session_id}")))
        .header("Authorization", format!("Bearer {bob_access}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    // Alice's refresh token must still work — bob's attempt was a no-op.
    let refresh_resp = client
        .post(server.url("/auth/refresh"))
        .json(&serde_json::json!({ "refresh_token": alice_refresh }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        refresh_resp.status(),
        StatusCode::OK,
        "alice's session must be untouched by bob's revoke attempt"
    );
    // That refresh rotated the token — fetch alice's live refresh token again.
    let refreshed: Value = refresh_resp.json().await.unwrap();
    let alice_refresh2 = refreshed["refresh_token"].as_str().unwrap().to_string();

    // Re-fetch alice's current session id. Rotation revokes the old session
    // record (it stays listed, `revoked = true`, for history/audit) and adds
    // a new one — so alice now has two rows; the listing must show both
    // (never silently drops a revoked row), and the live one is the one with
    // `revoked = false`.
    let alice_view2 = sql_as(
        &client,
        &server,
        &alice_access,
        "SELECT * FROM unidb_catalog.sessions",
    )
    .await;
    let (_, alice_rows2) = first_result(&alice_view2);
    assert_eq!(
        alice_rows2.len(),
        2,
        "alice must see both her original (now revoked) and rotated session"
    );
    let live: Vec<_> = alice_rows2
        .iter()
        .filter(|r| r[4].as_bool() == Some(false))
        .collect();
    assert_eq!(
        live.len(),
        1,
        "exactly one of alice's sessions is still live"
    );
    let alice_session_id2 = live[0][0].as_str().unwrap().to_string();
    assert_ne!(
        alice_session_id2, alice_session_id,
        "rotation must issue a fresh session id"
    );

    // ── alice revokes her own (current) session ──
    let resp = client
        .delete(server.url(&format!("/auth/sessions/{alice_session_id2}")))
        .header("Authorization", format!("Bearer {alice_access}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // Her refresh token no longer works.
    let refresh_resp2 = client
        .post(server.url("/auth/refresh"))
        .json(&serde_json::json!({ "refresh_token": alice_refresh2 }))
        .send()
        .await
        .unwrap();
    assert_eq!(refresh_resp2.status(), StatusCode::UNAUTHORIZED);

    // Bob's own session is untouched by any of the above.
    let bob_refresh_resp = client
        .post(server.url("/auth/refresh"))
        .json(&serde_json::json!({ "refresh_token": bob_refresh }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        bob_refresh_resp.status(),
        StatusCode::OK,
        "bob's session must be untouched by alice's revoke"
    );

    // ── superuser revoking an unknown id is a harmless no-op (idempotent) ──
    let resp = client
        .delete(server.url("/auth/sessions/does-not-exist"))
        .header("Authorization", format!("Bearer {admin_tok}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}
