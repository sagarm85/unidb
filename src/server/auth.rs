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
//!
//! **Key rotation with a grace window (item 146).** Rotating a signing key
//! used to invalidate every outstanding token at once. `JwtConfig` now holds
//! an ordered list of verification keys — the **current** key first, then an
//! optional **previous** ("grace") key — and tries them in that order:
//! - **HS256:** `UNIDB_JWT_SIGNING_KEY_PREVIOUS` adds a previous HS256 key
//!   (via [`JwtConfig::with_previous_hs256_key`]). Rotation procedure: set
//!   `..._PREVIOUS` to the old secret, set `UNIDB_JWT_SIGNING_KEY` to the new
//!   one, restart. Tokens signed with the old secret keep verifying (the
//!   grace window) until the operator drops `..._PREVIOUS` (after the max
//!   token TTL has elapsed), at which point they stop verifying again — the
//!   same 401 as an unrelated key.
//! - **Asymmetric:** `UNIDB_JWT_PUBLIC_KEY_PREVIOUS` adds a second public key
//!   (via [`JwtConfig::from_asymmetric_public_pems`]); both are listed in
//!   [`JwtConfig::jwks_document`], each under its own `kid`. Issuance stays
//!   HS256-only and out of scope for rotation (see the module doc above).
//! - **`kid`.** Every issued token (and every JWKS entry) carries a `kid`: a
//!   short, stable id derived via a **one-way truncated SHA-256 hash of the
//!   key material** ([`derive_kid`]) — never the key itself. It is purely a
//!   verification *hint*: verification always tries every configured key in
//!   order regardless of whether the token's `kid` is present, matches, or is
//!   unrecognized, so a stale/missing/wrong `kid` can never weaken the check.
//! - The previous key (either kind) is **verify-only** — issuance always uses
//!   the current key. A token matching neither key gets the same `401` as
//!   today.

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
use sha2::{Digest, Sha256};

/// Derive a short, stable, one-way `kid` from key material (item 146): the
/// first 16 hex characters (8 bytes) of the SHA-256 digest. Deliberately
/// irreversible — a `kid` never lets an observer recover the key it was
/// derived from, so it is safe to place in a token header or a public JWKS
/// document even when the underlying key is an HS256 secret. Truncated
/// (rather than the full 64 hex chars) because it only needs to be a stable
/// *hint* for key selection among at most two configured keys, not a
/// collision-proof identifier.
fn derive_kid(key_material: &[u8]) -> String {
    let digest = Sha256::digest(key_material);
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

/// One configured verification key: its `kid` (for JWKS / informational use
/// only — see the module doc), the decoding key, and the `Validation` that
/// matches its algorithm.
#[derive(Clone)]
struct VerifyKey {
    kid: String,
    decoding_key: DecodingKey,
    validation: Validation,
}

impl VerifyKey {
    fn hs256(secret: &str) -> Self {
        Self {
            kid: derive_kid(secret.as_bytes()),
            decoding_key: DecodingKey::from_secret(secret.as_bytes()),
            validation: Validation::new(Algorithm::HS256),
        }
    }
}

/// Loaded once at startup. See the module doc for the full precedence rules
/// governing verification algorithm and issuance availability, and the
/// "Key rotation with a grace window (item 146)" section for the
/// current-then-previous verify order.
#[derive(Clone)]
pub struct JwtConfig {
    /// Verification keys, tried strictly in order: the current key first,
    /// then the previous ("grace") key when one is configured (item 146).
    /// Exactly one entry for every pre-146 constructor; two only when a
    /// `..._PREVIOUS` env var / builder call added a grace key.
    keys: Vec<VerifyKey>,
    /// Non-`None` only when issuance is enabled (`UNIDB_JWT_SIGNING_KEY` or
    /// `UNIDB_DEV_LOGIN=1`) — used by `POST /auth/login` et al. Always an
    /// HS256 HMAC key (see the module doc — asymmetric issuance is deferred).
    pub encoding_key: Option<EncodingKey>,
    /// The `kid` stamped on every token minted via [`JwtConfig::issue_token`]
    /// — the current signing key's `kid` (item 146). `None` exactly when
    /// `encoding_key` is `None`.
    encoding_kid: Option<String>,
    /// Item 121 A6, extended by item 146: the public-key JWK Set entries to
    /// serve at `GET /.well-known/jwks.json`, computed once here at config
    /// time — one entry per configured asymmetric public key (current, and
    /// previous when rotating), each carrying its own `kid`. Empty when
    /// verification is HS256 (shared secret) — [`JwtConfig::jwks_document`]
    /// renders that case as `{"keys":[]}`. There is deliberately no code path
    /// that can put an HS256 secret into this field — it is only ever
    /// populated by the `from_asymmetric_*` constructors, which never see a
    /// secret, only public keys.
    jwks: Vec<serde_json::Value>,
}

impl JwtConfig {
    /// Verify-only config using an HS256 shared secret (production default
    /// when no issuance flag is set).
    pub fn new(secret: &str) -> Self {
        Self {
            keys: vec![VerifyKey::hs256(secret)],
            encoding_key: None,
            encoding_kid: None,
            jwks: Vec::new(),
        }
    }

    /// Verify + issue config, HS256, same secret both ways (HS256 requires
    /// it — the token issued here must verify against the same decoding
    /// key). Used by both the legacy `UNIDB_DEV_LOGIN=1` path and (via
    /// [`JwtConfig::with_signing_key`]) item 121 A5's production issuer path;
    /// kept under its original name for back-compat call sites.
    pub fn with_dev_login(secret: &str) -> Self {
        Self {
            keys: vec![VerifyKey::hs256(secret)],
            encoding_key: Some(EncodingKey::from_secret(secret.as_bytes())),
            encoding_kid: Some(derive_kid(secret.as_bytes())),
            jwks: Vec::new(),
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

    /// Item 146 — add a previous HS256 signing key to an existing HS256
    /// config, **verify-only**, for a rotation grace window: appended after
    /// the current key, so verification tries current→previous in order
    /// (never the reverse, and issuance never touches this key — see the
    /// module doc). Chains onto [`JwtConfig::new`]/[`JwtConfig::with_dev_login`]/
    /// [`JwtConfig::with_signing_key`]; a no-op call site should simply not
    /// call this (there is deliberately no "clear the previous key" — a fresh
    /// config without it already has none).
    pub fn with_previous_hs256_key(mut self, previous_secret: &str) -> Self {
        self.keys.push(VerifyKey::hs256(previous_secret));
        self
    }

    /// Item 121 A6 — verify-only asymmetric config: accepts a PEM-encoded
    /// RSA or EC (P-256) **public** key and auto-detects which one it is
    /// (tries RSA first, falls back to EC) so no separate "which algorithm"
    /// env var is needed. Issuance stays disabled (`encoding_key: None`) —
    /// see the module doc for why. Populates [`JwtConfig::jwks_document`]
    /// with exactly this key's public JWK (its own `kid` included, item 146).
    pub fn from_asymmetric_public_pem(pem: &[u8]) -> Result<Self, String> {
        let (key, jwk) = parse_asymmetric_public_pem(pem)?;
        Ok(Self {
            keys: vec![key],
            encoding_key: None,
            encoding_kid: None,
            jwks: vec![jwk],
        })
    }

    /// Item 146 — asymmetric verify config with an optional previous public
    /// key (`UNIDB_JWT_PUBLIC_KEY_PREVIOUS`): verification accepts either
    /// public key (current tried first), and both appear in
    /// [`JwtConfig::jwks_document`], each under its own `kid`. `previous_pem`
    /// need not be the same key type as `pem` (RSA/EC are each auto-detected
    /// independently), though a real rotation would normally keep the same
    /// scheme. Issuance stays disabled either way, as in
    /// [`JwtConfig::from_asymmetric_public_pem`].
    pub fn from_asymmetric_public_pems(
        pem: &[u8],
        previous_pem: Option<&[u8]>,
    ) -> Result<Self, String> {
        let mut cfg = Self::from_asymmetric_public_pem(pem)?;
        if let Some(prev_pem) = previous_pem {
            let (prev_key, prev_jwk) = parse_asymmetric_public_pem(prev_pem)?;
            cfg.keys.push(prev_key);
            cfg.jwks.push(prev_jwk);
        }
        Ok(cfg)
    }

    /// `GET /.well-known/jwks.json`'s body (item 121 A6, extended by item
    /// 146 to potentially list more than one key): every configured
    /// asymmetric public key as a JWK Set, or `{"keys":[]}` when
    /// verification is HS256 — **never** an HS256 secret, which has no
    /// public representation to publish in the first place.
    pub fn jwks_document(&self) -> serde_json::Value {
        serde_json::json!({ "keys": self.jwks.clone() })
    }

    /// Issue a short-lived HS256 JWT for `username`, carrying a `kid` header
    /// (item 146) derived from the current signing key.
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
    /// Always uses the **current** key — the previous ("grace") key added by
    /// [`JwtConfig::with_previous_hs256_key`] is verify-only and is never
    /// reachable from here.
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
        let mut header = Header::default();
        header.kid.clone_from(&self.encoding_kid);
        encode(&header, &claims, key)
    }
}

/// Shared by [`JwtConfig::from_asymmetric_public_pem`] and
/// [`JwtConfig::from_asymmetric_public_pems`]: parse one PEM-encoded RSA or
/// EC (P-256) public key into its [`VerifyKey`] plus its JWK (item 146: the
/// JWK's `kid` is derived from the same PEM bytes, one-way).
fn parse_asymmetric_public_pem(pem: &[u8]) -> Result<(VerifyKey, serde_json::Value), String> {
    let kid = derive_kid(pem);
    if let Ok(decoding_key) = DecodingKey::from_rsa_pem(pem) {
        let jwk = rsa_jwk(&decoding_key, &kid)?;
        return Ok((
            VerifyKey {
                kid,
                decoding_key,
                validation: Validation::new(Algorithm::RS256),
            },
            jwk,
        ));
    }
    if let Ok(decoding_key) = DecodingKey::from_ec_pem(pem) {
        let jwk = ec_jwk(&decoding_key, &kid)?;
        return Ok((
            VerifyKey {
                kid,
                decoding_key,
                validation: Validation::new(Algorithm::ES256),
            },
            jwk,
        ));
    }
    Err(
        "UNIDB_JWT_PUBLIC_KEY is not a recognized RSA or EC (P-256) public key in PEM format"
            .to_string(),
    )
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
/// `DecodingKey::from_rsa_pem`). `kid` (item 146) identifies this key among
/// others in the same JWKS document — derived one-way from the PEM, never
/// secret material.
fn rsa_jwk(decoding_key: &DecodingKey, kid: &str) -> Result<serde_json::Value, String> {
    let DecodingKeyKind::SecretOrDer(der) = decoding_key.kind() else {
        return Err("unexpected RSA decoding-key representation".to_string());
    };
    let (n, e) = parse_rsa_public_key_der(der)?;
    Ok(serde_json::json!({
        "kty": "RSA",
        "use": "sig",
        "alg": "RS256",
        "kid": kid,
        "n": b64url(&n),
        "e": b64url(&e),
    }))
}

/// Build the public JWK for an EC `DecodingKey` (as produced by
/// `DecodingKey::from_ec_pem`). `jsonwebtoken` (and `require_jwt`'s ES256
/// validation) only supports P-256 for ES256, so this only handles the
/// 65-byte uncompressed-point (`0x04 || X(32) || Y(32)`) shape. `kid` (item
/// 146) identifies this key among others in the same JWKS document.
fn ec_jwk(decoding_key: &DecodingKey, kid: &str) -> Result<serde_json::Value, String> {
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
        "kid": kid,
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
/// the handler. Verifies against `config`'s configured key(s) — HS256 or
/// item 121 A6's RS256/ES256, see the module doc — trying each in order
/// (current key, then the previous grace key when rotating, item 146). The
/// token's own `kid` header (if any) is **not** used to select which key to
/// try: every configured key is always attempted in order regardless, so a
/// missing or unrecognized `kid` can never weaken verification — it is only
/// ever a hint for callers that inspect it out-of-band. Succeeds on the
/// first key that verifies; a token matching none of them gets a single 401,
/// same as ever.
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
    // Item 146: `kid` (if present and recognized) is tried first as a
    // selection hint, purely to avoid a wasted attempt in the common case —
    // it is never a trust boundary. Every configured key still gets tried,
    // in current->previous order, regardless of whether the hint was
    // present, matched, or wrong: a missing/unknown/forged `kid` can only
    // cost an extra decode attempt, never weaken the check.
    let hinted_kid = jsonwebtoken::decode_header(token).ok().and_then(|h| h.kid);
    let mut result = None;
    let mut hinted_idx = None;
    if let Some(ref kid) = hinted_kid {
        if let Some(idx) = config.keys.iter().position(|k| &k.kid == kid) {
            let key = &config.keys[idx];
            result = Some(decode::<Claims>(token, &key.decoding_key, &key.validation));
            hinted_idx = Some(idx);
        }
    }
    if !matches!(result, Some(Ok(_))) {
        for (idx, key) in config.keys.iter().enumerate() {
            if Some(idx) == hinted_idx {
                continue; // already attempted above
            }
            match decode::<Claims>(token, &key.decoding_key, &key.validation) {
                Ok(data) => {
                    result = Some(Ok(data));
                    break;
                }
                Err(e) => result = Some(Err(e)),
            }
        }
    }
    metrics::histogram!("unidb_jwt_verify_seconds").record(start.elapsed().as_secs_f64());

    match result {
        Some(Ok(data)) => {
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
        Some(Err(e)) => unauthorized(format!("invalid token: {e}")),
        // `config.keys` is never empty (every constructor pushes at least
        // one), but degrade safely rather than panic if that invariant were
        // ever violated.
        None => unauthorized("no verification key configured"),
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
        let key = &cfg.keys[0];
        let data = decode::<serde_json::Value>(&token, &key.decoding_key, &key.validation).unwrap();
        assert_eq!(data.claims["sub"], "alice");
    }

    #[test]
    fn garbage_public_pem_is_rejected_not_panicking() {
        assert!(JwtConfig::from_asymmetric_public_pem(b"not a pem at all").is_err());
    }

    // ── item 146: kid + rotation grace window ──────────────────────────────

    #[test]
    fn issued_tokens_carry_a_stable_kid_derived_one_way_from_the_key() {
        let cfg1 = JwtConfig::with_signing_key("rotation-kid-secret");
        let cfg2 = JwtConfig::with_signing_key("rotation-kid-secret");
        let t1 = cfg1.issue_token("alice").unwrap();
        let t2 = cfg2.issue_token("bob").unwrap();
        let kid1 = jsonwebtoken::decode_header(&t1).unwrap().kid.unwrap();
        let kid2 = jsonwebtoken::decode_header(&t2).unwrap().kid.unwrap();
        // Same secret -> same, stable kid across independently built configs.
        assert_eq!(kid1, kid2);
        // One-way: the kid is short and never contains the secret itself.
        assert_ne!(kid1, "rotation-kid-secret");
        assert!(!kid1.contains("rotation-kid-secret"));
        assert_eq!(kid1.len(), 16, "kid is a truncated (16 hex char) digest");
    }

    #[test]
    fn different_keys_yield_different_kids() {
        let cfg_a = JwtConfig::with_signing_key("rotation-key-a");
        let cfg_b = JwtConfig::with_signing_key("rotation-key-b");
        let kid_a = jsonwebtoken::decode_header(cfg_a.issue_token("x").unwrap())
            .unwrap()
            .kid
            .unwrap();
        let kid_b = jsonwebtoken::decode_header(cfg_b.issue_token("x").unwrap())
            .unwrap()
            .kid
            .unwrap();
        assert_ne!(kid_a, kid_b);
    }

    #[test]
    fn previous_key_grace_window_then_drop() {
        let cfg_a = JwtConfig::with_signing_key("rotation-secret-a");
        let token_a = cfg_a.issue_token("alice").unwrap();

        // Rotate: current = B, previous = A. The A-signed token still
        // verifies (grace window); new tokens are B-signed.
        let cfg_rotated = JwtConfig::with_signing_key("rotation-secret-b")
            .with_previous_hs256_key("rotation-secret-a");
        let validation_any = Validation::new(Algorithm::HS256);
        let verifies = cfg_rotated.keys.iter().any(|k| {
            decode::<serde_json::Value>(&token_a, &k.decoding_key, &validation_any).is_ok()
        });
        assert!(
            verifies,
            "A-signed token must still verify during the grace window"
        );

        let token_b = cfg_rotated.issue_token("alice").unwrap();
        let kid_a = jsonwebtoken::decode_header(&token_a).unwrap().kid.unwrap();
        let kid_b = jsonwebtoken::decode_header(&token_b).unwrap().kid.unwrap();
        assert_ne!(kid_a, kid_b, "new tokens are B-signed with a different kid");

        // Drop the previous key -> the A-signed token no longer verifies.
        let cfg_dropped = JwtConfig::with_signing_key("rotation-secret-b");
        let still_verifies = cfg_dropped.keys.iter().any(|k| {
            decode::<serde_json::Value>(&token_a, &k.decoding_key, &validation_any).is_ok()
        });
        assert!(
            !still_verifies,
            "grace window must end once the previous key is dropped"
        );
    }

    #[test]
    fn unrelated_key_never_verifies_even_during_rotation() {
        let cfg_rotated = JwtConfig::with_signing_key("rotation-secret-b")
            .with_previous_hs256_key("rotation-secret-a");
        let unrelated = JwtConfig::with_signing_key("rotation-secret-c");
        let token_c = unrelated.issue_token("mallory").unwrap();
        let validation_any = Validation::new(Algorithm::HS256);
        let verifies = cfg_rotated.keys.iter().any(|k| {
            decode::<serde_json::Value>(&token_c, &k.decoding_key, &validation_any).is_ok()
        });
        assert!(
            !verifies,
            "a token signed by an unrelated key must never verify"
        );
    }

    #[test]
    fn jwks_lists_every_configured_public_key_with_its_own_kid_and_no_secret() {
        let cfg = JwtConfig::from_asymmetric_public_pems(
            RSA_PUB_PEM.as_bytes(),
            Some(EC_PUB_PEM.as_bytes()),
        )
        .expect("both PEMs are valid public keys");
        let doc = cfg.jwks_document();
        let keys = doc["keys"].as_array().unwrap();
        assert_eq!(keys.len(), 2);
        let kid0 = keys[0]["kid"].as_str().expect("current key has a kid");
        let kid1 = keys[1]["kid"].as_str().expect("previous key has a kid");
        assert_ne!(kid0, kid1);
        // No private-key/secret material anywhere in the published document.
        let text = doc.to_string();
        assert!(!text.contains("BEGIN PRIVATE KEY"));
        assert!(!text.contains("rotation-secret"));
    }

    #[test]
    fn no_secret_material_in_token_header_across_rotation() {
        let cfg_rotated = JwtConfig::with_signing_key("super-secret-current-key")
            .with_previous_hs256_key("super-secret-previous-key");
        let token = cfg_rotated.issue_token("alice").unwrap();
        // Decode the raw header segment (base64url, no padding) ourselves to
        // inspect exactly the bytes placed on the wire, independent of how
        // `jsonwebtoken::Header` chooses to expose fields.
        use base64::Engine;
        let header_b64 = token.split('.').next().unwrap();
        let header_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(header_b64)
            .unwrap();
        let header_json = String::from_utf8(header_bytes).unwrap();
        assert!(!header_json.contains("super-secret-current-key"));
        assert!(!header_json.contains("super-secret-previous-key"));
        assert!(header_json.contains("\"kid\""), "header must carry a kid");
    }
}
