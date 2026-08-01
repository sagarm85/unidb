// Item 146 — JWT signing-key rotation (kid + previous-key grace window).
//
// Test matrix:
//   1. a_signed_token_verifies_during_grace_after_rotation_to_b — the core
//      acceptance scenario: a token issued under key A keeps verifying,
//      end-to-end over HTTP, after the server rotates to current=B,
//      previous=A.
//   2. new_token_after_rotation_is_b_signed_with_a_different_kid — a token
//      minted post-rotation is signed with B and carries a different `kid`
//      than a token minted under A.
//   3. unrelated_key_c_returns_401_even_during_rotation — a token signed
//      with a wholly unrelated key is rejected the same way pre- and
//      post-rotation (no downgrade of the check).
//   4. dropping_previous_key_ends_the_grace_window — once `..._PREVIOUS` is
//      no longer configured, the old A-signed token stops verifying.
//   5. missing_or_unknown_kid_still_resolves_by_trying_keys_in_order — `kid`
//      is a hint, not a trust boundary: a token with no `kid`, or a bogus
//      unrecognized one, still verifies as long as its signature matches one
//      of the configured keys.
//   6. issued_tokens_carry_a_kid — every token `POST /auth/login` et al.
//      would issue (here, minted directly via `JwtConfig::issue_token`)
//      carries a non-empty `kid` header.
//   7. jwks_lists_every_configured_public_key_with_its_own_kid — asymmetric
//      mode: both the current and previous public keys appear in the JWKS
//      document, each under its own `kid`, and each round-trips against a
//      token signed by its matching private key.
//   8. no_secret_material_anywhere — the token header and the JWKS document
//      never contain the raw HS256 secrets or PEM private-key material.

#![cfg(feature = "server")]

use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use reqwest::StatusCode;
use serde::Serialize;
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};
use unidb::server::auth::JwtConfig;

#[path = "server_common/mod.rs"]
mod server_common;
use server_common::TestServer;

// ── HS256 rotation keys (arbitrary, fixed strings — not secret in the
// "real deployment" sense, just distinct test fixtures) ────────────────────

const KEY_A: &str = "item146-rotation-key-a-old-secret";
const KEY_B: &str = "item146-rotation-key-b-new-secret";
const KEY_C: &str = "item146-rotation-key-c-unrelated-secret";

// ── asymmetric test keypairs (same fixed pairs item121_a5_a6 uses) ─────────

const RSA_PRIV_PEM: &str = "-----BEGIN PRIVATE KEY-----
MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQC8HwlozdwyJKJY
K0NVUS+1hXPKTHzgmMZyj2ItWrHVMPrYek0kbFokci7pj1KswzF5MT9T9W7rFXb7
Y7+v1XP61bQiapvAshX8taiOmz1B80KwsoJKkG+dA9meCdElU8hKxQ86lDN40I8S
ElH7CcuLGu6webhUXcqVSPE5r4P9i8kya94Q6kWBCm77x1o/Liks59HvS27zXmT/
/xEQyeQlNmW/5UtDtc5J0RZmy7ELlrpuO81ZT3JFHMdDtgthfnTGBEqY24m98boI
G2yN0h8sJpVi7jzuRvJqS0MbErV94xwhvIeY3oFUHmqvAfx1e7Py+yuO0QjyWN+e
OF1xLdDNAgMBAAECggEAALECCLRUkAuecmXthvCF2qPkYD6HfKsbFXKDwB01MQNr
Y5FIHosp4y013yB7Iityfsn8K1pMUDXodDAnWq3o8FFgAN7bz6FGi/Tbc4nGr0mZ
RNZn0v60q+F+felw3mgthmeGQXfPIeI7UGOXp06n8SM9twZnKIdxVOHalsbUu0Qg
h7pjSdPOT3tIGznAp6tXGFpEYove/2HnAUAuVoDx+HlClw/Fu4ZXBDJ04fnKBg8d
FBqlq3KpPAE5fejuqiFiOMAy4MUt2pKPkUgLL5dK2Fqqngd3Ts4cS0/Xqd67Yi4s
gMGoNltKKVwB95yxxIPjSyzpIc1olONl5/h9//mAeQKBgQDjNgOqz9YXtY9Qsd3i
5oHcwi1HPUnAUoVA0C+mA3rp6AuJc1CmF9tWVQTd6OgTdtpWkExQuvKCZ8XkRcfh
OIqVcZZlnosCP+yQDGwbHkY/2KG5SOIu/tTh5SBxJ76PPW1YD+XEegylGK6/TZHJ
wYxOu8CDzGmhTDYI7J94I+RCpQKBgQDT9RPvbJkqzDnuKusParLy1eQkPnhpSl+k
vAliKzFLJEg9BvcfspYSm38HBJXoNJUy4BY9r+2pMANh06CYjUllxvgmAFd3mGE4
zGx0ypYYJOUg27SqMo5XgfvjV1n6s8FukVzasdZzPauxMOMviM8CxetrA1EVPwvN
pkfSVZ1FCQKBgGZjZ+GoiQTkJ3IoxSHD2E+AHWFWCA8n4K6lYmOAe/o+PDyzf2tp
osjTxT6u/y4OSDPsEMfshu4nD3Ff1MP0c9cGeczPVjssTVFYl7rcuLF60N4rLuoY
ohwt4aG8VE4+UzD08QjKKzqW1eCVdxYhJzYvu4BpNEygiFUbNH2yRuGVAoGBALT+
U16hEo4cRN+e4IiSqWp5wU491h669r86Hp0omvg6bEFIoF/95O7Qv4EjpkraFAmU
lwloIH7X1BuGVl3OUD3L0PzKT+Z9RY/16Cs3D0JgxxPu6PBpKWmKQqjYX6qYMvYS
xQKu15wirmkpgOaHYZZRof0IoQWOh6q9chknKJvZAoGBAMnR14TokK5mNFyWky88
JHINeNljZKJfIqO7eYWEf/D7i4VbVxXLKatE3HpXNiFGq7eay1yNOmud2jjQgzUT
ZQL5CMEYQbf0bMd/J1i92xQG3VaPW/fd/XZFzbW2V/v+aYCrSb58YCCFuPl59EpC
7gFVDUur9d/dEh4kRjHCn4Nv
-----END PRIVATE KEY-----
";

const RSA_PUB_PEM: &str = "-----BEGIN PUBLIC KEY-----
MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAvB8JaM3cMiSiWCtDVVEv
tYVzykx84JjGco9iLVqx1TD62HpNJGxaJHIu6Y9SrMMxeTE/U/Vu6xV2+2O/r9Vz
+tW0ImqbwLIV/LWojps9QfNCsLKCSpBvnQPZngnRJVPISsUPOpQzeNCPEhJR+wnL
ixrusHm4VF3KlUjxOa+D/YvJMmveEOpFgQpu+8daPy4pLOfR70tu815k//8REMnk
JTZlv+VLQ7XOSdEWZsuxC5a6bjvNWU9yRRzHQ7YLYX50xgRKmNuJvfG6CBtsjdIf
LCaVYu487kbyaktDGxK1feMcIbyHmN6BVB5qrwH8dXuz8vsrjtEI8ljfnjhdcS3Q
zQIDAQAB
-----END PUBLIC KEY-----
";

const EC_PRIV_PEM: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgy1H9ijWXfwsCPHPY
FTO8q1ugrT15OUkxDlZXwxyyvb+hRANCAAQMljsgBO1f1zLX/uBD/eoGeAzaiXfQ
JWi1Mx+YWxsgxtFHwThFzrCEo+JSYtmwvfnjp4kL1vh7DwFRalaiOdsX
-----END PRIVATE KEY-----
";

const EC_PUB_PEM: &str = "-----BEGIN PUBLIC KEY-----
MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEDJY7IATtX9cy1/7gQ/3qBngM2ol3
0CVotTMfmFsbIMbRR8E4Rc6whKPiUmLZsL3546eJC9b4ew8BUWpWojnbFw==
-----END PUBLIC KEY-----
";

#[derive(Serialize)]
struct Claims {
    sub: String,
    exp: usize,
}

fn now_secs() -> usize {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as usize
}

/// Hand-sign an HS256 token for `secret` — stands in for a token a server
/// previously issued under that secret (mirrors how item121_a5_a6's own
/// tests hand-sign RSA/EC tokens rather than going through `/auth/login`).
fn sign_hs256(secret: &str, sub: &str) -> String {
    let claims = Claims {
        sub: sub.to_string(),
        exp: now_secs() + 3600,
    };
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .unwrap()
}

async fn whoami(client: &reqwest::Client, server: &TestServer, token: &str) -> reqwest::Response {
    client
        .get(server.url("/auth/whoami"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap()
}

// ── test 1: grace window — an A-signed token keeps verifying after B/A rotation ─

#[tokio::test]
async fn a_signed_token_verifies_during_grace_after_rotation_to_b() {
    let client = reqwest::Client::new();

    // Before rotation: only key A is configured, and an A-signed token works.
    let server_only_a = TestServer::spawn_with_signing_key(KEY_A).await;
    let token_a = sign_hs256(KEY_A, "alice");
    let resp = whoami(&client, &server_only_a, &token_a).await;
    assert_eq!(resp.status(), StatusCode::OK);
    drop(server_only_a);

    // After rotation: current = B, previous = A. The same token, signed
    // under the now-previous key A, must still verify (the grace window).
    let server_rotated = TestServer::spawn_with_signing_key_and_previous(KEY_B, Some(KEY_A)).await;
    let resp = whoami(&client, &server_rotated, &token_a).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "an A-signed token must still verify during the B/A grace window"
    );
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["user"].as_str(), Some("alice"));
}

// ── test 2: post-rotation tokens are B-signed with a different kid ────────

#[tokio::test]
async fn new_token_after_rotation_is_b_signed_with_a_different_kid() {
    let client = reqwest::Client::new();

    let cfg_a = JwtConfig::with_signing_key(KEY_A);
    let token_a = cfg_a.issue_token("alice").unwrap();

    let cfg_rotated = JwtConfig::with_signing_key(KEY_B).with_previous_hs256_key(KEY_A);
    let token_b = cfg_rotated.issue_token("alice").unwrap();

    let kid_a = jsonwebtoken::decode_header(&token_a)
        .unwrap()
        .kid
        .expect("A-signed token must carry a kid");
    let kid_b = jsonwebtoken::decode_header(&token_b)
        .unwrap()
        .kid
        .expect("B-signed token must carry a kid");
    assert_ne!(
        kid_a, kid_b,
        "tokens signed with different keys must carry different kids"
    );

    // The new token is genuinely B-signed: it must NOT verify against a
    // server that only knows key A...
    let server_only_a = TestServer::spawn_with_signing_key(KEY_A).await;
    let resp = whoami(&client, &server_only_a, &token_b).await;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "a B-signed token must not verify against key A alone"
    );
    drop(server_only_a);

    // ...but it does verify against the post-rotation server (current=B).
    let server_rotated = TestServer::spawn_with_signing_key_and_previous(KEY_B, Some(KEY_A)).await;
    let resp = whoami(&client, &server_rotated, &token_b).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

// ── test 3: an unrelated key's token is rejected before and after rotation ─

#[tokio::test]
async fn unrelated_key_c_returns_401_even_during_rotation() {
    let client = reqwest::Client::new();
    let server_rotated = TestServer::spawn_with_signing_key_and_previous(KEY_B, Some(KEY_A)).await;

    let token_c = sign_hs256(KEY_C, "mallory");
    let resp = whoami(&client, &server_rotated, &token_c).await;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "a token signed by a wholly unrelated key must never verify, rotation or not"
    );
}

// ── test 4: dropping the previous key ends the grace window ───────────────

#[tokio::test]
async fn dropping_previous_key_ends_the_grace_window() {
    let client = reqwest::Client::new();
    let token_a = sign_hs256(KEY_A, "alice");

    // Grace window still open (previous = A).
    let server_grace = TestServer::spawn_with_signing_key_and_previous(KEY_B, Some(KEY_A)).await;
    let resp = whoami(&client, &server_grace, &token_a).await;
    assert_eq!(resp.status(), StatusCode::OK);
    drop(server_grace);

    // Operator drops `..._PREVIOUS` after the max token TTL — same token,
    // same current key B, but no previous key configured anymore.
    let server_dropped = TestServer::spawn_with_signing_key_and_previous(KEY_B, None).await;
    let resp = whoami(&client, &server_dropped, &token_a).await;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "once the previous key is dropped, an old-key token must stop verifying"
    );
}

// ── test 5: kid is a hint, not a trust boundary ────────────────────────────

#[tokio::test]
async fn missing_or_unknown_kid_still_resolves_by_trying_keys_in_order() {
    let client = reqwest::Client::new();
    let server_rotated = TestServer::spawn_with_signing_key_and_previous(KEY_B, Some(KEY_A)).await;

    // No kid at all (Header::default()) — already exercised by every other
    // test via `sign_hs256`, restated explicitly here.
    let token_no_kid = sign_hs256(KEY_A, "alice");
    let resp = whoami(&client, &server_rotated, &token_no_kid).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // A bogus, unrecognized kid on an otherwise validly-signed (key A) token
    // must not cause a rejection — kid is informational only.
    let mut header = Header::new(Algorithm::HS256);
    header.kid = Some("totally-unknown-kid-value".to_string());
    let claims = Claims {
        sub: "alice".to_string(),
        exp: now_secs() + 3600,
    };
    let token_bad_kid = encode(
        &header,
        &claims,
        &EncodingKey::from_secret(KEY_A.as_bytes()),
    )
    .unwrap();
    let resp = whoami(&client, &server_rotated, &token_bad_kid).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "an unrecognized kid must not block verification via the key-order fallback"
    );
}

// ── test 6: issued tokens carry a kid ──────────────────────────────────────

#[tokio::test]
async fn issued_tokens_carry_a_kid() {
    let cfg = JwtConfig::with_signing_key(KEY_A);
    let token = cfg.issue_token("alice").unwrap();
    let kid = jsonwebtoken::decode_header(&token).unwrap().kid;
    assert!(kid.is_some() && !kid.unwrap().is_empty());
}

// ── test 7: asymmetric JWKS lists every configured key, each with its own kid ──

#[tokio::test]
async fn jwks_lists_every_configured_public_key_with_its_own_kid() {
    let server = TestServer::spawn_with_asymmetric_public_pems(
        RSA_PUB_PEM.as_bytes(),
        Some(EC_PUB_PEM.as_bytes()),
    )
    .await;
    let client = reqwest::Client::new();

    let jwks: Value = client
        .get(server.url("/.well-known/jwks.json"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let keys = jwks["keys"].as_array().expect("keys array");
    assert_eq!(
        keys.len(),
        2,
        "both the current and previous key must be listed"
    );

    let rsa_key = keys
        .iter()
        .find(|k| k["kty"] == "RSA")
        .expect("RSA key present");
    let ec_key = keys
        .iter()
        .find(|k| k["kty"] == "EC")
        .expect("EC key present");
    let rsa_kid = rsa_key["kid"].as_str().expect("RSA entry has a kid");
    let ec_kid = ec_key["kid"].as_str().expect("EC entry has a kid");
    assert_ne!(rsa_kid, ec_kid);

    // Round-trip both: a token signed by either private key must verify
    // against its own configured public key, exactly as item121_a5_a6 checks
    // for the single-key case.
    let rsa_token = sign_asym(RSA_PRIV_PEM, Algorithm::RS256, "rsa-user");
    let resp = whoami(&client, &server, &rsa_token).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let ec_token = sign_asym(EC_PRIV_PEM, Algorithm::ES256, "ec-user");
    let resp = whoami(&client, &server, &ec_token).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

fn sign_asym(pem: &str, alg: Algorithm, sub: &str) -> String {
    let claims = Claims {
        sub: sub.to_string(),
        exp: now_secs() + 3600,
    };
    let key = match alg {
        Algorithm::RS256 => EncodingKey::from_rsa_pem(pem.as_bytes()).unwrap(),
        Algorithm::ES256 => EncodingKey::from_ec_pem(pem.as_bytes()).unwrap(),
        _ => unreachable!("test helper only signs RS256/ES256"),
    };
    encode(&Header::new(alg), &claims, &key).unwrap()
}

// ── test 8: no secret material anywhere ────────────────────────────────────

#[tokio::test]
async fn no_secret_material_anywhere() {
    // HS256: the header of an issued, post-rotation token never contains
    // either raw secret.
    let cfg_rotated = JwtConfig::with_signing_key(KEY_B).with_previous_hs256_key(KEY_A);
    let token = cfg_rotated.issue_token("alice").unwrap();
    let header_b64 = token.split('.').next().unwrap();
    use base64::Engine;
    let header_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(header_b64)
        .unwrap();
    let header_json = String::from_utf8(header_bytes).unwrap();
    assert!(!header_json.contains(KEY_A));
    assert!(!header_json.contains(KEY_B));
    assert!(header_json.contains("\"kid\""));

    // Asymmetric: the JWKS document (extended, item 146) still never leaks
    // private-key material — extends item121_a5_a6's "HS256 secret never
    // leaks into JWKS" spirit to the multi-key case.
    let server = TestServer::spawn_with_asymmetric_public_pems(
        RSA_PUB_PEM.as_bytes(),
        Some(EC_PUB_PEM.as_bytes()),
    )
    .await;
    let client = reqwest::Client::new();
    let jwks_text = client
        .get(server.url("/.well-known/jwks.json"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!jwks_text.contains("BEGIN PRIVATE KEY"));
    assert!(!jwks_text.contains(KEY_A));
    assert!(!jwks_text.contains(KEY_B));
}
