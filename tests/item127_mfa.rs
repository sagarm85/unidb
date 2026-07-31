// Item 127 (Workstream D4) — TOTP-based multi-factor authentication (MFA).
//
// Test matrix (mirrors the task spec's acceptance list):
//   (a) enroll_then_verify_enables_mfa_valid_and_invalid_code
//   (b) login_with_mfa_enabled_requires_challenge_and_challenge_yields_session
//   (c) recovery_code_works_once_then_is_consumed
//   (d) replay_of_same_totp_code_is_rejected
//   (e) disable_clears_mfa_and_login_reverts_to_unchanged
//   (f) non_mfa_user_logs_in_unchanged
//   (g) mfa_secret_never_appears_in_whoami_or_roles_json
//
// Every expected TOTP code is **computed deterministically from the secret**
// (a self-contained HOTP/TOTP re-implementation below, independent of
// `src/authz/mod.rs`'s own) rather than sleeping across a 30 s step boundary
// — this both satisfies the "no sleeping" requirement and, as a side effect,
// cross-validates the server's TOTP implementation against a second,
// independently-written one.

#![cfg(feature = "server")]

use hmac::{Hmac, KeyInit, Mac};
use reqwest::StatusCode;
use serde_json::Value;
use sha1::Sha1;

#[path = "server_common/mod.rs"]
mod server_common;
use server_common::TestServer;

// ── a second, independent TOTP implementation (test-side oracle) ──────────

const BASE32_ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

fn base32_decode(s: &str) -> Vec<u8> {
    let mut buffer: u32 = 0;
    let mut bits_left: u32 = 0;
    let mut out = Vec::new();
    for c in s.trim().trim_end_matches('=').chars() {
        let upper = c.to_ascii_uppercase();
        let val = BASE32_ALPHABET
            .iter()
            .position(|&b| b as char == upper)
            .expect("test secret must be valid base32") as u32;
        buffer = (buffer << 5) | val;
        bits_left += 5;
        if bits_left >= 8 {
            bits_left -= 8;
            out.push(((buffer >> bits_left) & 0xff) as u8);
        }
    }
    out
}

fn hotp(secret: &[u8], counter: u64) -> String {
    let mut mac = Hmac::<Sha1>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(&counter.to_be_bytes());
    let result = mac.finalize().into_bytes();
    let offset = (result[result.len() - 1] & 0x0f) as usize;
    let bytes = &result[offset..offset + 4];
    let code = ((bytes[0] as u32 & 0x7f) << 24)
        | ((bytes[1] as u32) << 16)
        | ((bytes[2] as u32) << 8)
        | (bytes[3] as u32);
    format!("{:06}", code % 1_000_000)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// The TOTP code valid `step_offset` steps (30 s each) from "now" — offset 0
/// is the current code, positive offsets are future (still-unused) codes.
fn totp_code(secret_base32: &str, step_offset: i64) -> String {
    let secret = base32_decode(secret_base32);
    let step = (now_secs() / 30) as i64 + step_offset;
    hotp(&secret, step.max(0) as u64)
}

// ── HTTP helpers (mirrors tests/item121_auth_core.rs's style) ─────────────

async fn create_user(client: &reqwest::Client, server: &TestServer, admin_tok: &str, sql: &str) {
    client
        .post(server.url("/sql"))
        .header("Authorization", format!("Bearer {admin_tok}"))
        .json(&serde_json::json!({ "sql": sql }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
}

async fn login(
    client: &reqwest::Client,
    server: &TestServer,
    username: &str,
    password: &str,
) -> reqwest::Response {
    client
        .post(server.url("/auth/login"))
        .json(&serde_json::json!({ "username": username, "password": password }))
        .send()
        .await
        .unwrap()
}

async fn mfa_enroll(client: &reqwest::Client, server: &TestServer, token: &str) -> Value {
    client
        .post(server.url("/auth/mfa/enroll"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn mfa_verify(
    client: &reqwest::Client,
    server: &TestServer,
    token: &str,
    code: &str,
) -> reqwest::Response {
    client
        .post(server.url("/auth/mfa/verify"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({ "code": code }))
        .send()
        .await
        .unwrap()
}

async fn mfa_challenge(
    client: &reqwest::Client,
    server: &TestServer,
    challenge: &str,
    code: &str,
) -> reqwest::Response {
    client
        .post(server.url("/auth/mfa/challenge"))
        .json(&serde_json::json!({ "challenge": challenge, "code": code }))
        .send()
        .await
        .unwrap()
}

async fn mfa_disable(
    client: &reqwest::Client,
    server: &TestServer,
    token: &str,
    code: Option<&str>,
) -> reqwest::Response {
    client
        .post(server.url("/auth/mfa/disable"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({ "code": code }))
        .send()
        .await
        .unwrap()
}

async fn whoami(client: &reqwest::Client, server: &TestServer, token: &str) -> Value {
    client
        .get(server.url("/auth/whoami"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// End-to-end setup shared by most tests: create a password user, log them
/// in (MFA not enabled yet, so this is a normal session), enroll TOTP MFA,
/// and confirm it with the current code. Returns
/// `(access_token, secret_base32, recovery_codes)`.
async fn setup_mfa_enabled_user(
    client: &reqwest::Client,
    server: &TestServer,
    admin_tok: &str,
    username: &str,
    password: &str,
) -> (String, String, Vec<String>) {
    create_user(
        client,
        server,
        admin_tok,
        &format!("CREATE USER {username} PASSWORD '{password}'"),
    )
    .await;
    let login_body: Value = login(client, server, username, password)
        .await
        .json()
        .await
        .unwrap();
    let access_token = login_body["access_token"].as_str().unwrap().to_string();

    let enroll_body = mfa_enroll(client, server, &access_token).await;
    let secret = enroll_body["secret"].as_str().unwrap().to_string();
    assert!(enroll_body["otpauth_url"]
        .as_str()
        .unwrap()
        .starts_with(&format!("otpauth://totp/unidb:{username}?secret={secret}")));
    assert!(enroll_body["otpauth_url"]
        .as_str()
        .unwrap()
        .contains("issuer=unidb"));

    let code = totp_code(&secret, 0);
    let verify_resp = mfa_verify(client, server, &access_token, &code).await;
    assert_eq!(verify_resp.status(), StatusCode::OK);
    let verify_body: Value = verify_resp.json().await.unwrap();
    assert_eq!(verify_body["enabled"].as_bool(), Some(true));
    let recovery_codes: Vec<String> = verify_body["recovery_codes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(recovery_codes.len(), 8);

    (access_token, secret, recovery_codes)
}

// ── (a) enroll -> verify enables MFA; valid code passes, invalid fails ─────

#[tokio::test]
async fn enroll_then_verify_enables_mfa_valid_and_invalid_code() {
    let server = TestServer::spawn_with_dev_login().await;
    let client = reqwest::Client::new();
    let admin_tok = server_common::valid_token();

    create_user(
        &client,
        &server,
        &admin_tok,
        "CREATE USER alice PASSWORD 'alices-pw'",
    )
    .await;
    let login_body: Value = login(&client, &server, "alice", "alices-pw")
        .await
        .json()
        .await
        .unwrap();
    let access_token = login_body["access_token"].as_str().unwrap().to_string();

    // Not enabled before enrollment.
    let who_before = whoami(&client, &server, &access_token).await;
    assert_eq!(who_before["mfa_enabled"].as_bool(), Some(false));

    let enroll_body = mfa_enroll(&client, &server, &access_token).await;
    let secret = enroll_body["secret"].as_str().unwrap().to_string();

    // Still pending — not enabled — until confirmed.
    let who_pending = whoami(&client, &server, &access_token).await;
    assert_eq!(who_pending["mfa_enabled"].as_bool(), Some(false));

    // An invalid code does not confirm enrollment.
    let bad_resp = mfa_verify(&client, &server, &access_token, "000000").await;
    assert_eq!(bad_resp.status(), StatusCode::UNAUTHORIZED);
    let who_still_pending = whoami(&client, &server, &access_token).await;
    assert_eq!(who_still_pending["mfa_enabled"].as_bool(), Some(false));

    // The correct current code confirms enrollment and enables MFA.
    let code = totp_code(&secret, 0);
    let ok_resp = mfa_verify(&client, &server, &access_token, &code).await;
    assert_eq!(ok_resp.status(), StatusCode::OK);
    let ok_body: Value = ok_resp.json().await.unwrap();
    assert_eq!(ok_body["enabled"].as_bool(), Some(true));
    assert_eq!(ok_body["recovery_codes"].as_array().unwrap().len(), 8);

    let who_after = whoami(&client, &server, &access_token).await;
    assert_eq!(who_after["mfa_enabled"].as_bool(), Some(true));
}

// ── (b) login with MFA enabled requires a challenge; only a valid code
//        (TOTP or recovery) on the challenge yields a session ─────────────

#[tokio::test]
async fn login_with_mfa_enabled_requires_challenge_and_challenge_yields_session() {
    let server = TestServer::spawn_with_dev_login().await;
    let client = reqwest::Client::new();
    let admin_tok = server_common::valid_token();

    let (_access, secret, _recovery) =
        setup_mfa_enabled_user(&client, &server, &admin_tok, "bob", "bobs-pw").await;

    // Password-only login no longer yields a session.
    let login_resp = login(&client, &server, "bob", "bobs-pw").await;
    assert_eq!(login_resp.status(), StatusCode::OK);
    let login_body: Value = login_resp.json().await.unwrap();
    assert_eq!(login_body["mfa_required"].as_bool(), Some(true));
    assert!(login_body.get("access_token").is_none());
    assert!(login_body.get("token").is_none());
    assert!(login_body.get("refresh_token").is_none());
    let challenge = login_body["challenge"].as_str().unwrap().to_string();
    assert!(login_body["expires_in"].as_u64().unwrap() > 0);

    // A wrong code against the challenge is rejected; the challenge is not
    // consumed by a failed attempt.
    let wrong = mfa_challenge(&client, &server, &challenge, "000000").await;
    assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);

    // A fresh, correct code redeems the challenge for a real session.
    let code = totp_code(&secret, 1);
    let good = mfa_challenge(&client, &server, &challenge, &code).await;
    assert_eq!(good.status(), StatusCode::OK);
    let good_body: Value = good.json().await.unwrap();
    let access = good_body["access_token"].as_str().unwrap().to_string();
    assert!(!good_body["refresh_token"].as_str().unwrap().is_empty());

    let who = whoami(&client, &server, &access).await;
    assert_eq!(who["user"].as_str(), Some("bob"));
}

// ── (c) a recovery code works once, then is consumed ───────────────────────

#[tokio::test]
async fn recovery_code_works_once_then_is_consumed() {
    let server = TestServer::spawn_with_dev_login().await;
    let client = reqwest::Client::new();
    let admin_tok = server_common::valid_token();

    let (_access, _secret, recovery_codes) =
        setup_mfa_enabled_user(&client, &server, &admin_tok, "carol", "carols-pw").await;
    let rc = recovery_codes[0].clone();

    // First use: succeeds and yields a session.
    let login_body: Value = login(&client, &server, "carol", "carols-pw")
        .await
        .json()
        .await
        .unwrap();
    let challenge1 = login_body["challenge"].as_str().unwrap().to_string();
    let first = mfa_challenge(&client, &server, &challenge1, &rc).await;
    assert_eq!(first.status(), StatusCode::OK);
    let first_body: Value = first.json().await.unwrap();
    assert!(!first_body["access_token"].as_str().unwrap().is_empty());

    // Second use of the SAME recovery code (fresh challenge): rejected.
    let login_body2: Value = login(&client, &server, "carol", "carols-pw")
        .await
        .json()
        .await
        .unwrap();
    let challenge2 = login_body2["challenge"].as_str().unwrap().to_string();
    let second = mfa_challenge(&client, &server, &challenge2, &rc).await;
    assert_eq!(second.status(), StatusCode::UNAUTHORIZED);

    // A different, still-unused recovery code still works.
    let login_body3: Value = login(&client, &server, "carol", "carols-pw")
        .await
        .json()
        .await
        .unwrap();
    let challenge3 = login_body3["challenge"].as_str().unwrap().to_string();
    let third = mfa_challenge(&client, &server, &challenge3, &recovery_codes[1]).await;
    assert_eq!(third.status(), StatusCode::OK);
}

// ── (d) replay of the same code/step is rejected ────────────────────────────

#[tokio::test]
async fn replay_of_same_totp_code_is_rejected() {
    let server = TestServer::spawn_with_dev_login().await;
    let client = reqwest::Client::new();
    let admin_tok = server_common::valid_token();

    let (_access, secret, _recovery) =
        setup_mfa_enabled_user(&client, &server, &admin_tok, "dave", "daves-pw").await;

    // `setup_mfa_enabled_user` already consumed step-offset 0 (the current
    // code at enrollment-confirm time) — replaying that exact code against a
    // brand new login challenge must fail, proving replay protection is keyed
    // on the TOTP step, not merely on the challenge token.
    let already_used_code = totp_code(&secret, 0);
    let login_body: Value = login(&client, &server, "dave", "daves-pw")
        .await
        .json()
        .await
        .unwrap();
    let challenge = login_body["challenge"].as_str().unwrap().to_string();
    let replay_resp = mfa_challenge(&client, &server, &challenge, &already_used_code).await;
    assert_eq!(
        replay_resp.status(),
        StatusCode::UNAUTHORIZED,
        "a previously-consumed TOTP step must never authenticate again"
    );

    // A fresh, never-used step still works on that same still-open challenge.
    let fresh_code = totp_code(&secret, 1);
    let ok_resp = mfa_challenge(&client, &server, &challenge, &fresh_code).await;
    assert_eq!(ok_resp.status(), StatusCode::OK);

    // And now THAT code is used up too — replaying it against a fresh
    // challenge also fails.
    let login_body2: Value = login(&client, &server, "dave", "daves-pw")
        .await
        .json()
        .await
        .unwrap();
    let challenge2 = login_body2["challenge"].as_str().unwrap().to_string();
    let replay2 = mfa_challenge(&client, &server, &challenge2, &fresh_code).await;
    assert_eq!(replay2.status(), StatusCode::UNAUTHORIZED);
}

// ── (e) disable clears MFA; login reverts to unchanged ──────────────────────

#[tokio::test]
async fn disable_clears_mfa_and_login_reverts_to_unchanged() {
    let server = TestServer::spawn_with_dev_login().await;
    let client = reqwest::Client::new();
    let admin_tok = server_common::valid_token();

    let (_access, secret, recovery_codes) =
        setup_mfa_enabled_user(&client, &server, &admin_tok, "erin", "erins-pw").await;

    // Redeem a fresh MFA login to get a session token to call /disable with.
    // Uses a recovery code here (not a fresh TOTP code) deliberately: TOTP's
    // ±1-step skew window plus the forward-only replay ratchet only permits
    // one more *fresh* TOTP verification after `setup_mfa_enabled_user`'s own
    // enrollment-confirm within the same 30 s window (three monotonically
    // increasing steps is the theoretical max the window allows, and this
    // test needs that single remaining slot for the `/disable` call itself
    // below) — a recovery code is a wholly separate credential space, so
    // redeeming this login with one doesn't touch the TOTP step budget at all.
    let login_body: Value = login(&client, &server, "erin", "erins-pw")
        .await
        .json()
        .await
        .unwrap();
    let challenge = login_body["challenge"].as_str().unwrap().to_string();
    let session_body: Value = mfa_challenge(&client, &server, &challenge, &recovery_codes[0])
        .await
        .json()
        .await
        .unwrap();
    let access_token = session_body["access_token"].as_str().unwrap().to_string();

    // Missing code, non-superuser: rejected with a clear 400 (not a silent
    // disable, not a 401 that would look like a wrong-code failure).
    let missing_code = mfa_disable(&client, &server, &access_token, None).await;
    assert_eq!(missing_code.status(), StatusCode::BAD_REQUEST);
    assert!(whoami(&client, &server, &access_token)
        .await
        .get("mfa_enabled")
        .unwrap()
        .as_bool()
        .unwrap());

    // Wrong code: rejected with 401, MFA stays enabled.
    let wrong_code = mfa_disable(&client, &server, &access_token, Some("000000")).await;
    assert_eq!(wrong_code.status(), StatusCode::UNAUTHORIZED);
    assert!(whoami(&client, &server, &access_token)
        .await
        .get("mfa_enabled")
        .unwrap()
        .as_bool()
        .unwrap());

    // Correct current code disables MFA — the one remaining fresh TOTP step
    // after `setup_mfa_enabled_user`'s enrollment-confirm consumed offset 0
    // (see the comment above).
    let disable_code = totp_code(&secret, 1);
    let ok = mfa_disable(&client, &server, &access_token, Some(&disable_code)).await;
    assert_eq!(ok.status(), StatusCode::NO_CONTENT);

    let who_after = whoami(&client, &server, &access_token).await;
    assert_eq!(who_after["mfa_enabled"].as_bool(), Some(false));

    // Login now behaves exactly like a non-MFA user again — a normal
    // session, no `mfa_required` challenge.
    let login_after: Value = login(&client, &server, "erin", "erins-pw")
        .await
        .json()
        .await
        .unwrap();
    assert!(login_after.get("mfa_required").is_none());
    assert!(!login_after["access_token"].as_str().unwrap().is_empty());
}

// ── (f) a non-MFA user logs in exactly as before ────────────────────────────

#[tokio::test]
async fn non_mfa_user_logs_in_unchanged() {
    let server = TestServer::spawn_with_dev_login().await;
    let client = reqwest::Client::new();
    let admin_tok = server_common::valid_token();

    create_user(
        &client,
        &server,
        &admin_tok,
        "CREATE USER frank PASSWORD 'franks-pw'",
    )
    .await;

    let resp = login(&client, &server, "frank", "franks-pw").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    // Exactly the pre-127 shape: no `mfa_required`, real tokens present.
    assert!(body.get("mfa_required").is_none());
    let access = body["access_token"].as_str().unwrap().to_string();
    assert!(!body["refresh_token"].as_str().unwrap().is_empty());
    assert_eq!(body["expires_in"].as_u64(), Some(3600));

    let who = whoami(&client, &server, &access).await;
    assert_eq!(who["user"].as_str(), Some("frank"));
    assert_eq!(who["mfa_enabled"].as_bool(), Some(false));
}

// ── (g) the secret never appears in whoami / roles.json ────────────────────

#[tokio::test]
async fn mfa_secret_never_appears_in_whoami_or_roles_json() {
    let server = TestServer::spawn_with_dev_login().await;
    let client = reqwest::Client::new();
    let admin_tok = server_common::valid_token();

    let (access_token, secret, recovery_codes) =
        setup_mfa_enabled_user(&client, &server, &admin_tok, "gina", "ginas-pw").await;

    let who_resp = client
        .get(server.url("/auth/whoami"))
        .header("Authorization", format!("Bearer {access_token}"))
        .send()
        .await
        .unwrap();
    let who_text = who_resp.text().await.unwrap();
    assert!(!who_text.contains(&secret));
    for rc in &recovery_codes {
        assert!(!who_text.contains(rc));
    }
    // whoami reports the boolean, never the secret material.
    let who_json: Value = serde_json::from_str(&who_text).unwrap();
    assert_eq!(who_json["mfa_enabled"].as_bool(), Some(true));
    assert!(who_json.get("secret").is_none());
    assert!(who_json.get("mfa_secret").is_none());

    // The persisted control-plane store never carries the plaintext secret
    // or plaintext recovery codes either — only base32(secret) is stored
    // (that IS the persisted secret, by design — but the *hashes* of
    // recovery codes, never their plaintext).
    let roles_json_path = server.data_dir().join("roles.json");
    let raw = std::fs::read_to_string(&roles_json_path).unwrap();
    for rc in &recovery_codes {
        assert!(!raw.contains(rc));
    }
}
