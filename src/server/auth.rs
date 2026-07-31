//! JWT auth (M5.c, extended by item 121 A5/A6): the server validates bearer
//! tokens applied as a `tower::Layer` (`axum::middleware::from_fn_with_state`)
//! wrapping every data-plane route in `router.rs`. **Deliberately excluded
//! from `GET /metrics`**: Prometheus scrapers don't carry app-level bearer
//! tokens, and the operational expectation is that `/metrics` gets
//! firewalled at the network layer in production — the same "no TLS
//! termination, assume a reverse proxy" assumption already stated for the
//! rest of this server, not an oversight.
//!
//! **Verification algorithms.** One [`JwtConfig`] instance verifies with
//! exactly one active algorithm, selected by which key material startup
//! configured it with — never both at once:
//! - **HS256** (shared secret, `DecodingKey::from_secret`) — the original v1
//!   path (`UNIDB_JWT_SECRET`).
//! - **RS256 / ES256** (item 121 A6, asymmetric, verify with a PUBLIC key
//!   only) — `UNIDB_JWT_PUBLIC_KEY` (PEM). Lets an external IdP (or anything
//!   holding the matching private key) issue tokens this server can verify
//!   without ever holding a shared secret. The concrete algorithm (RSA vs
//!   EC/P-256) is auto-detected from the PEM's own key type — no separate
//!   "which algorithm" env var needed. Supporting simultaneous "accept either
//!   HS256 or asymmetric" on one server (peeking `decode_header`'s `alg` and
//!   branching to the matching key) is a natural follow-up, deliberately not
//!   built here — every acceptance case for A6 is single-algorithm-at-a-time,
//!   matching how a real deployment picks one signing scheme.
//!
//! **Issuance (`POST /auth/login` et al.).** Stays **HS256-only** even when
//! verification is asymmetric — see `JwtConfig::issue_token`'s doc comment
//! for why local RS256/ES256 issuance is deliberately deferred (item 121 A6
//! note). Issuance is populated by exactly one of, in this precedence order:
//! 1. `UNIDB_JWT_SIGNING_KEY` (item 121 A5 — the production issuer path,
//!    first-class and independent of the dev flag below).
//! 2. `UNIDB_DEV_LOGIN=1` (pre-A5, kept for back-compat — same construction,
//!    using `UNIDB_JWT_SECRET` as the signing secret).
//! 3. Neither set ⇒ issuance disabled (the safe default) — `POST
//!    /auth/login`/`signup`/`refresh` return the existing "issuance
//!    disabled" error; verification is unaffected either way.
//!
//! Asymmetric verify mode (`UNIDB_JWT_PUBLIC_KEY` set) takes over the
//! decoding key entirely and disables local issuance outright: a locally
//! HS256-signed token would never verify against a configured asymmetric
//! public key, so minting one would be silently useless at best. See
//! `src/bin/unidb-server.rs`'s startup wiring for the exact precedence logic
//! and its warning log lines.

use axum::{
    extract::Request,
    http::{header, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use jsonwebtoken::{
    decode, encode, Algorithm, DecodingKey, DecodingKeyKind, EncodingKey, Header, Validation,
};
use serde::{Deserialize, Serialize};

/// Loaded once at startup. See the module doc for the full precedence rules
/// governing verification algorithm and issuance availability.
#[derive(Clone)]
pub struct JwtConfig {
    decoding_key: DecodingKey,
    validation: Validation,
    /// Non-`None` only when issuance is enabled (`UNIDB_JWT_SIGNING_KEY` or
    /// `UNIDB_DEV_LOGIN=1`) — used by `POST /auth/login` et al. Always an
    /// HS256 HMAC key (see the module doc — asymmetric issuance is deferred).
    pub encoding_key: Option<EncodingKey>,
    /// Item 121 A6: the public-key JWK Set document to serve at `GET
    /// /.well-known/jwks.json`, computed once here at config time. `None`
    /// when verification is HS256 (shared secret) — [`JwtConfig::jwks_document`]
    /// renders that case as `{"keys":[]}`. There is deliberately no code path
    /// that can put an HS256 secret into this field — it is only ever
    /// populated by the `from_*_public_pem` constructors, which never see a
    /// secret, only a public key.
    jwk: Option<serde_json::Value>,
}

impl JwtConfig {
    /// Verify-only config using an HS256 shared secret (production default
    /// when no issuance flag is set).
    pub fn new(secret: &str) -> Self {
        Self {
            decoding_key: DecodingKey::from_secret(secret.as_bytes()),
            validation: Validation::new(Algorithm::HS256),
            encoding_key: None,
            jwk: None,
        }
    }

    /// Verify + issue config, HS256, same secret both ways (HS256 requires
    /// it — the token issued here must verify against the same decoding
    /// key). Used by both the legacy `UNIDB_DEV_LOGIN=1` path and (via
    /// [`JwtConfig::with_signing_key`]) item 121 A5's production issuer path;
    /// kept under its original name for back-compat call sites.
    pub fn with_dev_login(secret: &str) -> Self {
        Self {
            decoding_key: DecodingKey::from_secret(secret.as_bytes()),
            validation: Validation::new(Algorithm::HS256),
            encoding_key: Some(EncodingKey::from_secret(secret.as_bytes())),
            jwk: None,
        }
    }

    /// Item 121 A5 — production issuer: verify + issue using an explicit
    /// signing secret (`UNIDB_JWT_SIGNING_KEY`), independent of
    /// `UNIDB_DEV_LOGIN`. Identical construction to
    /// [`JwtConfig::with_dev_login`] (HS256 has no other option) — a
    /// distinct name so call sites and startup logs read as "production
    /// issuer configured," not "dev flag is on."
    pub fn with_signing_key(secret: &str) -> Self {
        Self::with_dev_login(secret)
    }

    /// Item 121 A6 — verify-only asymmetric config: accepts a PEM-encoded
    /// RSA or EC (P-256) **public** key and auto-detects which one it is
    /// (tries RSA first, falls back to EC) so no separate "which algorithm"
    /// env var is needed. Issuance stays disabled (`encoding_key: None`) —
    /// see the module doc for why. Populates [`JwtConfig::jwks_document`]
    /// with exactly this key's public JWK.
    pub fn from_asymmetric_public_pem(pem: &[u8]) -> Result<Self, String> {
        if let Ok(decoding_key) = DecodingKey::from_rsa_pem(pem) {
            let jwk = rsa_jwk(&decoding_key)?;
            return Ok(Self {
                decoding_key,
                validation: Validation::new(Algorithm::RS256),
                encoding_key: None,
                jwk: Some(jwk),
            });
        }
        if let Ok(decoding_key) = DecodingKey::from_ec_pem(pem) {
            let jwk = ec_jwk(&decoding_key)?;
            return Ok(Self {
                decoding_key,
                validation: Validation::new(Algorithm::ES256),
                encoding_key: None,
                jwk: Some(jwk),
            });
        }
        Err(
            "UNIDB_JWT_PUBLIC_KEY is not a recognized RSA or EC (P-256) public key in PEM format"
                .to_string(),
        )
    }

    /// `GET /.well-known/jwks.json`'s body (item 121 A6): the configured
    /// asymmetric public key as a JWK Set, or `{"keys":[]}` when
    /// verification is HS256 — **never** the HS256 secret, which has no
    /// public representation to publish in the first place.
    pub fn jwks_document(&self) -> serde_json::Value {
        let keys: Vec<serde_json::Value> = self.jwk.clone().into_iter().collect();
        serde_json::json!({ "keys": keys })
    }

    /// Issue a short-lived HS256 JWT for `username`.
    ///
    /// Returns `Err` when `encoding_key` is `None` — issuance disabled
    /// (neither `UNIDB_JWT_SIGNING_KEY` nor `UNIDB_DEV_LOGIN=1` configured,
    /// or `UNIDB_JWT_PUBLIC_KEY` asymmetric-verify mode is active). Always
    /// signs HS256 even when verification is asymmetric — item 121 A6 scoped
    /// asymmetric issuance (`UNIDB_JWT_PRIVATE_KEY` signing RS256/ES256) out:
    /// it needs its own key-management story (private-key handling, rotation)
    /// that is not "straightforward" the way verify-with-a-public-key is, so
    /// it is deferred rather than half-built. Verify-side asymmetric support
    /// (this module) plus HS256 issuance already satisfies the stated use
    /// case: accept externally-issued tokens *and* keep our own login path.
    pub fn issue_token(&self, username: &str) -> Result<String, jsonwebtoken::errors::Error> {
        use std::time::{SystemTime, UNIX_EPOCH};
        // No signing key configured (verify-only HS256, or asymmetric
        // verify-only mode) ⇒ issuance is disabled. `ErrorKind::InvalidToken`
        // is a slight abuse (nothing was actually decoded), but every call
        // site already maps any `Err` here to its own "issuance disabled"
        // API error and never inspects the `jsonwebtoken` error kind, so the
        // exact variant is not load-bearing.
        let key = self
            .encoding_key
            .as_ref()
            .ok_or(jsonwebtoken::errors::ErrorKind::InvalidToken)?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let claims = serde_json::json!({
            "sub": username,
            "iat": now,
            "exp": now + 3600, // 1 hour
        });
        encode(&Header::default(), &claims, key)
    }
}

/// Minimal hand-rolled DER TLV (tag-length-value) reader — just enough to
/// pull the two `INTEGER`s out of an RSA `RSAPublicKey` DER SEQUENCE (RFC
/// 8017) or split an EC uncompressed point. Not a general ASN.1 parser (no
/// support for indefinite lengths, tags beyond one byte, etc.) — the input
/// here is always the already-validated, already-key-type-classified output
/// of `jsonwebtoken`'s own PEM/ASN.1 decoding (`DecodingKey::kind()`), so the
/// shapes we need to handle are fixed and small.
struct DerReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> DerReader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn read_tlv(&mut self) -> Result<(u8, &'a [u8]), String> {
        let tag = *self
            .buf
            .get(self.pos)
            .ok_or_else(|| "truncated DER: missing tag byte".to_string())?;
        self.pos += 1;
        let len_byte = *self
            .buf
            .get(self.pos)
            .ok_or_else(|| "truncated DER: missing length byte".to_string())?;
        self.pos += 1;
        let len = if len_byte & 0x80 == 0 {
            len_byte as usize
        } else {
            let nbytes = (len_byte & 0x7f) as usize;
            if nbytes == 0 || nbytes > std::mem::size_of::<usize>() {
                return Err("unsupported DER length encoding".to_string());
            }
            let mut len = 0usize;
            for _ in 0..nbytes {
                let b = *self
                    .buf
                    .get(self.pos)
                    .ok_or_else(|| "truncated DER: missing length bytes".to_string())?;
                self.pos += 1;
                len = (len << 8) | b as usize;
            }
            len
        };
        let start = self.pos;
        let end = start
            .checked_add(len)
            .ok_or_else(|| "DER length overflow".to_string())?;
        let value = self
            .buf
            .get(start..end)
            .ok_or_else(|| "truncated DER: value shorter than declared length".to_string())?;
        self.pos = end;
        Ok((tag, value))
    }
}

const DER_TAG_SEQUENCE: u8 = 0x30;
const DER_TAG_INTEGER: u8 = 0x02;

/// Parse an RFC 8017 `RSAPublicKey` DER SEQUENCE (`{ INTEGER modulus,
/// INTEGER publicExponent }`) — exactly what `DecodingKey::from_rsa_pem`
/// yields via `kind()` for both PKCS#1 and PKCS#8 ("PUBLIC KEY") PEMs
/// (`jsonwebtoken` already unwrapped the outer `SubjectPublicKeyInfo`/BIT
/// STRING for us). Returns `(modulus, exponent)` with any DER
/// sign-disambiguation leading zero byte stripped, ready for base64url (JWK
/// `n`/`e`).
fn parse_rsa_public_key_der(der: &[u8]) -> Result<(Vec<u8>, Vec<u8>), String> {
    let mut outer = DerReader::new(der);
    let (tag, seq) = outer.read_tlv()?;
    if tag != DER_TAG_SEQUENCE {
        return Err("expected DER SEQUENCE (RSAPublicKey)".to_string());
    }
    let mut fields = DerReader::new(seq);
    let (n_tag, n) = fields.read_tlv()?;
    if n_tag != DER_TAG_INTEGER {
        return Err("expected DER INTEGER (RSA modulus)".to_string());
    }
    let (e_tag, e) = fields.read_tlv()?;
    if e_tag != DER_TAG_INTEGER {
        return Err("expected DER INTEGER (RSA public exponent)".to_string());
    }
    Ok((
        strip_der_integer_sign_byte(n),
        strip_der_integer_sign_byte(e),
    ))
}

/// A DER `INTEGER` is signed, so an otherwise-positive value whose high bit
/// would read as negative gets a leading `0x00` byte. JWK's `n`/`e` are
/// unsigned big-endian — strip that byte when present.
fn strip_der_integer_sign_byte(b: &[u8]) -> Vec<u8> {
    if b.len() > 1 && b[0] == 0 {
        b[1..].to_vec()
    } else {
        b.to_vec()
    }
}

fn b64url(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Build the public JWK for an RSA `DecodingKey` (as produced by
/// `DecodingKey::from_rsa_pem`).
fn rsa_jwk(decoding_key: &DecodingKey) -> Result<serde_json::Value, String> {
    let DecodingKeyKind::SecretOrDer(der) = decoding_key.kind() else {
        return Err("unexpected RSA decoding-key representation".to_string());
    };
    let (n, e) = parse_rsa_public_key_der(der)?;
    Ok(serde_json::json!({
        "kty": "RSA",
        "use": "sig",
        "alg": "RS256",
        "n": b64url(&n),
        "e": b64url(&e),
    }))
}

/// Build the public JWK for an EC `DecodingKey` (as produced by
/// `DecodingKey::from_ec_pem`). `jsonwebtoken` (and `require_jwt`'s ES256
/// validation) only supports P-256 for ES256, so this only handles the
/// 65-byte uncompressed-point (`0x04 || X(32) || Y(32)`) shape.
fn ec_jwk(decoding_key: &DecodingKey) -> Result<serde_json::Value, String> {
    let DecodingKeyKind::SecretOrDer(point) = decoding_key.kind() else {
        return Err("unexpected EC decoding-key representation".to_string());
    };
    if point.len() != 65 || point.first() != Some(&0x04) {
        return Err("only uncompressed P-256 EC public keys are supported".to_string());
    }
    let x = &point[1..33];
    let y = &point[33..65];
    Ok(serde_json::json!({
        "kty": "EC",
        "use": "sig",
        "alg": "ES256",
        "crv": "P-256",
        "x": b64url(x),
        "y": b64url(y),
    }))
}

/// Deliberately permissive: no required custom claims beyond whatever
/// `jsonwebtoken`'s `Validation` already checks by default (`exp`, if
/// present, is validated; `nbf`/`aud`/`iss` are opt-in and not required
/// here). There is no role/scope claim distinction in v1 — any validly
/// signed, unexpired token grants access to every data-plane route alike
/// (see the known-limitations note in `PROGRESS.md`/`MEMORY.md`).
#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    /// The subject = the unidb username (P6.e). Absent ⇒ an anonymous but
    /// authenticated client, treated as the implicit superuser (backward
    /// compatible with pre-P6.e tokens that carry no `sub`).
    sub: Option<String>,
    /// Every other claim the token carries (auth seam). Retained — no longer
    /// discarded — so `AuthPrincipal::claims` can carry the full flattened
    /// claim set down into the engine. Still unconsumed by any policy logic.
    #[serde(flatten)]
    extra: std::collections::HashMap<String, serde_json::Value>,
}

/// The authenticated user carried through request extensions (P6.e). `None`
/// (no `sub` claim) is the implicit superuser.
#[derive(Clone, Debug)]
pub struct CurrentUser(pub Option<String>);

#[derive(Serialize)]
struct AuthErrorBody {
    error: String,
    code: &'static str,
}

fn unauthorized(msg: impl Into<String>) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(AuthErrorBody {
            error: msg.into(),
            code: "UNAUTHORIZED",
        }),
    )
        .into_response()
}

/// `axum::middleware::from_fn_with_state`-compatible middleware: extracts
/// `Authorization: Bearer <token>`, verifies it, and only then forwards
/// the request — any failure (missing header, malformed header, bad
/// signature, expired token) short-circuits with 401 and never reaches
/// the handler. Verifies with whichever single algorithm `config` was built
/// with (HS256, or item 121 A6's RS256/ES256) — see the module doc.
pub async fn require_jwt(
    axum::extract::State(config): axum::extract::State<JwtConfig>,
    request: Request,
    next: Next,
) -> Response {
    let Some(header_value) = request.headers().get(header::AUTHORIZATION) else {
        return unauthorized("missing Authorization header");
    };
    let Ok(header_str) = header_value.to_str() else {
        return unauthorized("Authorization header is not valid UTF-8");
    };
    let Some(token) = header_str.strip_prefix("Bearer ") else {
        return unauthorized("Authorization header must be a Bearer token");
    };

    let start = std::time::Instant::now();
    let result = decode::<Claims>(token, &config.decoding_key, &config.validation);
    metrics::histogram!("unidb_jwt_verify_seconds").record(start.elapsed().as_secs_f64());

    match result {
        Ok(data) => {
            // Carry the authenticated username to handlers for per-user
            // privilege checks (P6.e).
            let mut request = request;
            let principal = crate::AuthPrincipal {
                subject: data.claims.sub.clone(),
                claims: data.claims.extra.into_iter().collect(),
                roles: Vec::new(),
            };
            request
                .extensions_mut()
                .insert(CurrentUser(data.claims.sub));
            request.extensions_mut().insert(principal);
            next.run(request).await
        }
        Err(e) => unauthorized(format!("invalid token: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fixed 2048-bit RSA and P-256 EC test keypairs (openssl-generated,
    // committed here for determinism — no key material is secret, these
    // exist only to exercise the DER parser).
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
    const EC_PUB_PEM: &str = "-----BEGIN PUBLIC KEY-----
MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEDJY7IATtX9cy1/7gQ/3qBngM2ol3
0CVotTMfmFsbIMbRR8E4Rc6whKPiUmLZsL3546eJC9b4ew8BUWpWojnbFw==
-----END PUBLIC KEY-----
";

    #[test]
    fn from_asymmetric_public_pem_detects_rsa_and_builds_matching_jwk() {
        let cfg = JwtConfig::from_asymmetric_public_pem(RSA_PUB_PEM.as_bytes())
            .expect("valid RSA public PEM must parse");
        let doc = cfg.jwks_document();
        let keys = doc["keys"].as_array().unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0]["kty"], "RSA");
        assert_eq!(keys[0]["alg"], "RS256");
        let n = keys[0]["n"].as_str().unwrap();
        let e = keys[0]["e"].as_str().unwrap();

        // Round-trip: a token signed by the real private key must verify
        // against a DecodingKey rebuilt purely from the published (n, e).
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let claims = serde_json::json!({"sub": "unit-test", "exp": now + 3600});
        let token = encode(
            &Header::new(Algorithm::RS256),
            &claims,
            &EncodingKey::from_rsa_pem(RSA_PRIV_PEM.as_bytes()).unwrap(),
        )
        .unwrap();
        let decoding_key = DecodingKey::from_rsa_components(n, e).unwrap();
        let data =
            decode::<serde_json::Value>(&token, &decoding_key, &Validation::new(Algorithm::RS256))
                .expect("must verify against the JWK's own (n, e)");
        assert_eq!(data.claims["sub"], "unit-test");
    }

    #[test]
    fn from_asymmetric_public_pem_detects_ec_and_builds_matching_jwk() {
        let cfg = JwtConfig::from_asymmetric_public_pem(EC_PUB_PEM.as_bytes())
            .expect("valid EC public PEM must parse");
        let doc = cfg.jwks_document();
        let keys = doc["keys"].as_array().unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0]["kty"], "EC");
        assert_eq!(keys[0]["alg"], "ES256");
        assert_eq!(keys[0]["crv"], "P-256");
    }

    #[test]
    fn jwks_document_is_empty_for_hs256_config() {
        let cfg = JwtConfig::new("some-hs256-secret");
        let doc = cfg.jwks_document();
        assert_eq!(doc["keys"].as_array().unwrap().len(), 0);
        // Never leaks the secret in any form.
        assert!(!doc.to_string().contains("some-hs256-secret"));
    }

    #[test]
    fn issue_token_fails_cleanly_without_a_signing_key() {
        let cfg = JwtConfig::new("verify-only-secret");
        assert!(cfg.issue_token("alice").is_err());
    }

    #[test]
    fn with_signing_key_issues_a_token_that_verifies_against_itself() {
        let cfg = JwtConfig::with_signing_key("production-signing-secret");
        let token = cfg.issue_token("alice").unwrap();
        let data = decode::<serde_json::Value>(&token, &cfg.decoding_key, &cfg.validation).unwrap();
        assert_eq!(data.claims["sub"], "alice");
    }

    #[test]
    fn garbage_public_pem_is_rejected_not_panicking() {
        assert!(JwtConfig::from_asymmetric_public_pem(b"not a pem at all").is_err());
    }
}
