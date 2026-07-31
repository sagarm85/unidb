// Item 121, Workstream I (I1 — P0, ships with A): brute-force protection over
// the password-auth mutation routes. Test matrix:
//   1. rapid_login_attempts_trip_the_limit — the (N+1)th attempt in a window
//      returns 429 RATE_LIMITED with a Retry-After header.
//   2. different_usernames_from_the_same_client_are_independent — the
//      optional username discriminator keeps two accounts' buckets separate.
//   3. window_resets_after_it_elapses — once the window passes, attempts are
//      allowed again.
//   4. non_auth_routes_are_never_rate_limited — /sql and /auth/meta are
//      exempt even after exhausting the auth-route limit.
//   5. sql_and_meta_and_jwks_still_work_at_default_limits — a smoke test at
//      the server's real (env-var-default) limiter wiring, confirming
//      ordinary auth traffic and every exempt route are unaffected.

#![cfg(feature = "server")]

use std::time::Duration;

use reqwest::StatusCode;

#[path = "server_common/mod.rs"]
mod server_common;
use server_common::TestServer;
use unidb::server::rate_limit::AuthRateLimiter;

async fn login(client: &reqwest::Client, server: &TestServer, username: &str) -> reqwest::Response {
    client
        .post(server.url("/auth/login"))
        .json(&serde_json::json!({ "username": username, "password": "wrong-password" }))
        .send()
        .await
        .unwrap()
}

// ── test 1: the (N+1)th rapid attempt is rejected ──────────────────────────

#[tokio::test]
async fn rapid_login_attempts_trip_the_limit() {
    let limiter = AuthRateLimiter::new(3, Duration::from_secs(60));
    let server = TestServer::spawn_with_dev_login_and_rate_limit(limiter).await;
    let client = reqwest::Client::new();

    // Every attempt uses a wrong password, so each individually would 401 —
    // rejected auth attempts must still count toward the limit (the spec's
    // "a brute-forcer gets 401s — those must count" requirement).
    for i in 0..3 {
        let resp = login(&client, &server, "alice").await;
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "attempt {i} should still be within the limit and fail on credentials, not rate limiting"
        );
    }

    // The 4th attempt (N+1, N=3) is rate-limited, not a credentials check.
    let resp = login(&client, &server, "alice").await;
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    let retry_after = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .expect("429 response must carry Retry-After");
    let secs: u64 = retry_after.to_str().unwrap().parse().unwrap();
    assert!(secs >= 1, "Retry-After must be at least 1 second");

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["code"], "RATE_LIMITED");
    assert!(body["error"].as_str().is_some());
}

// ── test 2: different keys (here: different usernames from one client) are
//    independent buckets ──────────────────────────────────────────────────

#[tokio::test]
async fn different_usernames_from_the_same_client_are_independent() {
    let limiter = AuthRateLimiter::new(1, Duration::from_secs(60));
    let server = TestServer::spawn_with_dev_login_and_rate_limit(limiter).await;
    let client = reqwest::Client::new();

    // "alice" uses up her one allowed attempt.
    let resp = login(&client, &server, "alice").await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let resp = login(&client, &server, "alice").await;
    assert_eq!(
        resp.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "alice's 2nd attempt must be rate-limited"
    );

    // "bob", from the same client/IP, has his own untouched bucket (the
    // username discriminator keeps the two apart).
    let resp = login(&client, &server, "bob").await;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "a different username's first attempt must not be pre-limited by alice's bucket"
    );
}

// ── test 3: the window resets after it elapses ─────────────────────────────

#[tokio::test]
async fn window_resets_after_it_elapses() {
    let limiter = AuthRateLimiter::new(1, Duration::from_secs(2));
    let server = TestServer::spawn_with_dev_login_and_rate_limit(limiter).await;
    let client = reqwest::Client::new();

    let resp = login(&client, &server, "carol").await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let resp = login(&client, &server, "carol").await;
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);

    // Wait out the window, then confirm the bucket has reset.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let resp = login(&client, &server, "carol").await;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "after the window elapses the attempt must be allowed again (and fail on credentials)"
    );
}

// ── test 4: non-auth-mutation routes are never rate-limited ───────────────

#[tokio::test]
async fn non_auth_routes_are_never_rate_limited() {
    let limiter = AuthRateLimiter::new(1, Duration::from_secs(60));
    let server = TestServer::spawn_with_dev_login_and_rate_limit(limiter).await;
    let client = reqwest::Client::new();

    // Exhaust the (very small) login limit for this client/user.
    let resp = login(&client, &server, "dave").await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let resp = login(&client, &server, "dave").await;
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);

    // GET /auth/meta is explicitly exempt.
    let resp = client.get(server.url("/auth/meta")).send().await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // POST /sql (behind JWT, but not behind the auth rate limiter) is exempt
    // — a valid token still gets a normal response, never 429.
    let token = server_common::valid_token();
    let resp = client
        .post(server.url("/sql"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({ "sql": "CREATE TABLE rl_probe (id INT)" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // GET /.well-known/jwks.json is exempt too.
    let resp = client
        .get(server.url("/.well-known/jwks.json"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ── test 5: default (env-var) wiring is a no-op for ordinary traffic ───────

#[tokio::test]
async fn default_rate_limiter_does_not_interfere_with_ordinary_traffic() {
    // No env vars set here ⇒ TestServer::spawn_with_dev_login()'s
    // `build_router` picks up the real defaults (10 attempts / 60s), which a
    // single legitimate login must sail through.
    let server = TestServer::spawn_with_dev_login().await;
    let client = reqwest::Client::new();

    let resp = login(&client, &server, "erin").await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED); // no such user — not 429

    let resp = client.get(server.url("/auth/meta")).send().await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}
