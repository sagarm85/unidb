// Item 121 (Workstream A) — A5 production issuer + A6 asymmetric JWT/JWKS.
//
// Test matrix:
//   1. production_signing_key_config_issues_usable_tokens — A5 acceptance
//      (d): `UNIDB_JWT_SIGNING_KEY` semantics (not `UNIDB_DEV_LOGIN`) issue a
//      token via `POST /auth/login` that verifies against the same server.
//   2. rs256_token_signed_by_external_idp_verifies_and_reaches_handler — A6
//      acceptance (a): a token signed with an RSA private key verifies
//      against the matching configured public key and reaches a protected
//      handler.
//   3. es256_token_signed_by_external_idp_verifies_and_reaches_handler — same
//      as 2, for EC/P-256.
//   4. rs256_wrong_key_token_returns_401 — A6 acceptance (b): a token signed
//      with a *different* RSA keypair's private key is rejected.
//   5. es256_wrong_key_token_returns_401 — same as 4, for EC.
//   6. jwks_returns_matching_rsa_public_key — A6 acceptance (c): `GET
//      /.well-known/jwks.json` returns a JWK whose (n, e) actually verify a
//      token signed by the configured private key (round-trip, not just
//      "some JSON came back").
//   7. jwks_returns_matching_ec_public_key — same as 6, for EC.
//   8. jwks_is_empty_when_hs256_only — A6 acceptance (c), the other half:
//      an HS256-only server exposes `{"keys":[]}`, never the shared secret.
//   9. asymmetric_verify_mode_disables_local_issuance — `POST /auth/login`
//      stays disabled even if the server also happens to have a signing key
//      configured, because verification is asymmetric-only.

#![cfg(feature = "server")]

use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use reqwest::StatusCode;
use serde::Serialize;
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

#[path = "server_common/mod.rs"]
mod server_common;
use server_common::TestServer;

// ── test keypairs (openssl-generated once, fixed here for determinism) ────
//
// RSA 2048 (openssl genrsa 2048 / rsa -pubout) and EC P-256 (openssl ecparam
// -name prime256v1 -genkey -noout | pkcs8 -topk8 -nocrypt), plus one
// "wrong" keypair of each kind to exercise the mismatched-key 401 path.

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

// A wholly different RSA keypair — used to prove a mismatched signature is
// rejected, not just "any RSA-shaped token is accepted."
const RSA_WRONG_PRIV_PEM: &str = "-----BEGIN PRIVATE KEY-----
MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQCvB0g9Kt7/D/ec
MHeMDwnEECoV/eCWcvgPY3ydZdXgb0+Q723D497uvvlJXDkJBi1VfstWo2e6HBIw
Wi7X5eAWwy9xhQabooZjsDp3NRDowG9VDtL9zVgxZicuHyy6ysp+BuZhybj5HVTs
x6LEEVfQFUCFxhCPwsVOaVSUAcKI8hVUcl+jObBoSCDeNU1CjvIqal3GNxszlFT9
ThQyy38srmd6rDeWPeSlnZZsqWSA0caSlH56ltE7VaKCB8TczRnhcLZEdF3Sb6Vy
ebt0/SDM19aJxWPCNjs4IPfPv2rRBY7YhnlC9Kb7tXsony9DtCxQHx64vZdnnFbo
6gzBbayZAgMBAAECggEASzWE3AvM8KrqyimlZQCdQKt1eieyVtOmNe6ZAIVexubt
uKi5cOA8zjgvpE9FjtQhrMgkFeF3U+h2BGLsGAeGKCHNBCmWMlA/ER0LsmeSEYGH
FXSeQ0L2b5umoFHzBXzYUBkk68YjfqAU+v25uih9pENNi24VdcDKyavHFSZAcllk
KPL31uzHG3627nscni87rpZ5+iWLm/zWCoQwLeNHi4NZl1UpB+OpfJJxeAODLvst
HKiJIupJq7m4P47JPKHLRnpIhDfv+3qp5x9TjqT/CI+r9zAJIaiPvDABa9g9xsYo
saCn7FH2Ym6xoNM7T9f3oOIiWuFAQ67O+wmWSo4QZQKBgQDyg/osmdRj48zb/L2c
0lFZGNjvv88WeL6v7rnuiY4WZbHgxlelXXPinMHOMC+LT9ksBJkE+6/H3I2lVQXi
8dYg3lV2XbcQ9N/AGCPEejVvkwNLeJLB9SHcC8AyRbXUa8pxtOh2IU1tOuwj9yue
zr/h6Lm5qWwTXI56dnfROTDfMwKBgQC4wq2NzqM7lp5tdZbS1aNsPEiG+RfL2F2m
FLsfCIqaRLEnupZBpsm5BFKtddZ2JRYhDSUHjy4MA/ZzgWUbrPUjIqxH59OBuTP1
lzH4DXTHrLFGq78wElD3Tnz+DDMUkpfxULsK5SEPcR/90/cRRRAA6Gltxw+ZQIrg
v5DKx8S1AwKBgQCGVVCoj/Uz96UsXg1x5pYk8jyIYQkG+480yNm5JfzMrzwes/8s
nF1qs0YvPkW3t10mos1YE0pFPQhBAp2mHitsPXu9ex/ChsHCGB0H4mHjEd4LWhiA
05YT23Z04mRb6/FRltIFTWEkFjVjnrBM4V0sd8sY6p3xA53we9rWzAUkPQKBgEsQ
PPa6FzNkdCVAevBZf7W/oC/GD9bvpsyM66EmFTmr4tWjRtyRaK9UhEqY73K8iosP
DhZOI4UaLwyqa2udD1MhCSGFnDa+CdAjh1eiD+n3zWZK7LgZGPAA4WNNjYs0K6sN
A5DfmljtuvOjJGPNzTyxL/Q7xaibwlChQ7A/DToFAoGBAM78+vMFPd6Gpd3+IeRv
VhH2VEdJh0OdBhq76ZBPdZzYLAJ4bkRIo1GxCjRO1qSLloqJNvVAitfO84k/ZhX2
CbXrwSys61HTLpEMQDLhWyqa+ZntM0dXKBnU+iY+8MH69zMGM1TEVfMx1yjxJg8U
d96rF8VMLlKVO6NWb9Q/hDlg
-----END PRIVATE KEY-----
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

// A different EC keypair — used the same way as `RSA_WRONG_PRIV_PEM`.
const EC_WRONG_PRIV_PEM: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgZ5DgLIb6GMLTZUmy
4Nsfpoc3K8bNKxnHXdjfeFz1InehRANCAASaXoDflezE/ft/uo5Y4em8W1xd6cEp
mB5zuk7B80ye9/1i3oht0MZ5Iz9fwQuqRGagXa4teEK4qt1MHhDgr5Ps
-----END PRIVATE KEY-----
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

fn sign(pem: &str, alg: Algorithm, sub: &str) -> String {
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

async fn whoami(client: &reqwest::Client, server: &TestServer, token: &str) -> reqwest::Response {
    client
        .get(server.url("/auth/whoami"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap()
}

// ── test 1: A5 — production signing-key config issues usable tokens ───────

#[tokio::test]
async fn production_signing_key_config_issues_usable_tokens() {
    const SECRET: &str = "a5-production-signing-key-not-dev-login";
    let server = TestServer::spawn_with_signing_key(SECRET).await;
    let client = reqwest::Client::new();

    // Bootstrap: no users exist yet (open_mode), so any validly-HS256-signed
    // token (for THIS server's signing-key secret, not `TEST_JWT_SECRET`)
    // authenticates as the implicit superuser — enough to `CREATE USER`.
    let admin_claims = Claims {
        sub: "bootstrap-admin".to_string(),
        exp: now_secs() + 3600,
    };
    let admin_tok = encode(
        &Header::default(),
        &admin_claims,
        &EncodingKey::from_secret(SECRET.as_bytes()),
    )
    .unwrap();
    client
        .post(server.url("/sql"))
        .header("Authorization", format!("Bearer {admin_tok}"))
        .json(&serde_json::json!({ "sql": "CREATE USER produser PASSWORD 'prod-pw'" }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    // `UNIDB_JWT_SIGNING_KEY` (via `JwtConfig::with_signing_key`) must be
    // enough on its own — no `UNIDB_DEV_LOGIN` involved — to make `POST
    // /auth/login` mint a token that this same server then verifies.
    let resp = client
        .post(server.url("/auth/login"))
        .json(&serde_json::json!({ "username": "produser", "password": "prod-pw" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    let token = body["access_token"].as_str().unwrap();

    let who: Value = whoami(&client, &server, token)
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(who["user"].as_str(), Some("produser"));
}

// ── test 2/3: an externally RS256/ES256-signed token verifies ─────────────

#[tokio::test]
async fn rs256_token_signed_by_external_idp_verifies_and_reaches_handler() {
    let server = TestServer::spawn_with_asymmetric_public_pem(RSA_PUB_PEM.as_bytes()).await;
    let client = reqwest::Client::new();

    let token = sign(RSA_PRIV_PEM, Algorithm::RS256, "external-rsa-user");
    let resp = whoami(&client, &server, &token).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["user"].as_str(), Some("external-rsa-user"));
}

#[tokio::test]
async fn es256_token_signed_by_external_idp_verifies_and_reaches_handler() {
    let server = TestServer::spawn_with_asymmetric_public_pem(EC_PUB_PEM.as_bytes()).await;
    let client = reqwest::Client::new();

    let token = sign(EC_PRIV_PEM, Algorithm::ES256, "external-ec-user");
    let resp = whoami(&client, &server, &token).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["user"].as_str(), Some("external-ec-user"));
}

// ── test 4/5: a token signed by the WRONG key is rejected ─────────────────

#[tokio::test]
async fn rs256_wrong_key_token_returns_401() {
    let server = TestServer::spawn_with_asymmetric_public_pem(RSA_PUB_PEM.as_bytes()).await;
    let client = reqwest::Client::new();

    let token = sign(RSA_WRONG_PRIV_PEM, Algorithm::RS256, "impostor");
    let resp = whoami(&client, &server, &token).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn es256_wrong_key_token_returns_401() {
    let server = TestServer::spawn_with_asymmetric_public_pem(EC_PUB_PEM.as_bytes()).await;
    let client = reqwest::Client::new();

    let token = sign(EC_WRONG_PRIV_PEM, Algorithm::ES256, "impostor");
    let resp = whoami(&client, &server, &token).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// ── test 6/7: GET /.well-known/jwks.json returns a *correct*, usable key ───

#[tokio::test]
async fn jwks_returns_matching_rsa_public_key() {
    let server = TestServer::spawn_with_asymmetric_public_pem(RSA_PUB_PEM.as_bytes()).await;
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
    assert_eq!(keys.len(), 1);
    let key = &keys[0];
    assert_eq!(key["kty"].as_str(), Some("RSA"));
    assert_eq!(key["alg"].as_str(), Some("RS256"));
    let n = key["n"].as_str().expect("n present");
    let e = key["e"].as_str().expect("e present");

    // Round-trip: build a DecodingKey purely from the JWKS-published (n, e)
    // and confirm it verifies a token signed by the real private key — this
    // proves the hand-rolled DER parser extracted the *correct* modulus and
    // exponent, not just well-formed-looking JSON.
    let decoding_key = jsonwebtoken::DecodingKey::from_rsa_components(n, e).unwrap();
    let token = sign(RSA_PRIV_PEM, Algorithm::RS256, "jwks-roundtrip");
    let validation = jsonwebtoken::Validation::new(Algorithm::RS256);
    let data = jsonwebtoken::decode::<serde_json::Value>(&token, &decoding_key, &validation)
        .expect("token signed by the real private key must verify against the published JWK");
    assert_eq!(data.claims["sub"].as_str(), Some("jwks-roundtrip"));
}

#[tokio::test]
async fn jwks_returns_matching_ec_public_key() {
    let server = TestServer::spawn_with_asymmetric_public_pem(EC_PUB_PEM.as_bytes()).await;
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
    assert_eq!(keys.len(), 1);
    let key = &keys[0];
    assert_eq!(key["kty"].as_str(), Some("EC"));
    assert_eq!(key["alg"].as_str(), Some("ES256"));
    assert_eq!(key["crv"].as_str(), Some("P-256"));
    let x = key["x"].as_str().expect("x present");
    let y = key["y"].as_str().expect("y present");

    let decoding_key = jsonwebtoken::DecodingKey::from_ec_components(x, y).unwrap();
    let token = sign(EC_PRIV_PEM, Algorithm::ES256, "jwks-roundtrip");
    let validation = jsonwebtoken::Validation::new(Algorithm::ES256);
    let data = jsonwebtoken::decode::<serde_json::Value>(&token, &decoding_key, &validation)
        .expect("token signed by the real private key must verify against the published JWK");
    assert_eq!(data.claims["sub"].as_str(), Some("jwks-roundtrip"));
}

// ── test 8: HS256-only server never publishes a key (and never the secret) ─

#[tokio::test]
async fn jwks_is_empty_when_hs256_only() {
    let server = TestServer::spawn().await;
    let client = reqwest::Client::new();

    let resp = client
        .get(server.url("/.well-known/jwks.json"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let text = resp.text().await.unwrap();
    let jwks: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(jwks["keys"].as_array().unwrap().len(), 0);
    assert!(
        !text.contains(server_common::TEST_JWT_SECRET),
        "the HS256 secret must never appear in the JWKS response"
    );
}

// ── test 9: asymmetric verify mode leaves local issuance disabled ─────────

#[tokio::test]
async fn asymmetric_verify_mode_disables_local_issuance() {
    let server = TestServer::spawn_with_asymmetric_public_pem(RSA_PUB_PEM.as_bytes()).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(server.url("/auth/login"))
        .json(&serde_json::json!({ "username": "someone", "password": "whatever" }))
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_client_error(),
        "login must stay disabled in asymmetric verify-only mode, got {}",
        resp.status()
    );
}
