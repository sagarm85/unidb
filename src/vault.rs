//! Encrypt-at-rest secrets vault (item 129, Workstream I3 of the
//! [Supabase parity roadmap](../docs/backlog/120_supabase_parity_roadmap.md)).
//!
//! Config secrets (today: OAuth client secrets — see
//! `src/server/oauth.rs`'s `// I3: route through the vault` seam; later:
//! SMTP credentials) can be stored **encrypted at rest** instead of sitting
//! in plaintext in `roles.json` or an env var. A single master key, read
//! once from `UNIDB_MASTER_KEY` at `Engine::open` time, encrypts/decrypts
//! named secret values with AES-256-GCM. The ciphertext (never the
//! plaintext) is what [`crate::authz::RoleStore`] persists — see its
//! `secrets` field and [`crate::authz::RoleStore::set_secret`]/
//! [`crate::authz::RoleStore::get_secret`].
//!
//! ## Master key format
//!
//! `UNIDB_MASTER_KEY` must decode to exactly 32 bytes (an AES-256 key),
//! encoded as either:
//! - **base64** (standard alphabet, `+`/`/`, padded with `=` or unpadded —
//!   both are accepted): 44 characters with padding, 43 without.
//! - **hex** (`[0-9a-fA-F]`, case-insensitive): exactly 64 characters.
//!
//! Generate one with e.g. `openssl rand -base64 32` or `openssl rand -hex 32`.
//!
//! ## Enabled vs. disabled
//!
//! - **`UNIDB_MASTER_KEY` unset or empty** → the vault is **disabled**: a
//!   startup `tracing::warn!` is logged, [`Vault::is_enabled`] is `false`,
//!   and every secret-backed config value (e.g. an OAuth client secret)
//!   falls back to its plaintext env var exactly as before this item shipped
//!   — never a hard crash, never a behavior change for a deployment that
//!   hasn't opted in. See [`Vault::from_env`].
//! - **`UNIDB_MASTER_KEY` set but malformed** (wrong length, not valid
//!   base64/hex) → [`Vault::from_env`] returns `Err` with a message naming
//!   the exact problem — this is a configuration error the operator must
//!   fix, not something to silently paper over (CLAUDE.md's "prove, don't
//!   assume" / fail-clearly posture). `unidb-server`/`unidb-vault` refuse to
//!   start in this case, the same posture `UNIDB_JWT_SECRET` already has.
//! - **`UNIDB_MASTER_KEY` set and valid** → the vault is enabled. A secret
//!   that was stored while enabled can only ever be read back with the
//!   *same* key: a wrong/rotated key or a tampered ciphertext blob both fail
//!   the AES-GCM authentication tag check and [`Vault::decrypt`] returns
//!   `Err` — **fail closed**, never a silent plaintext fallback for a secret
//!   that was genuinely stored encrypted (see [`Vault::decrypt`]'s doc
//!   comment).
//!
//! ## Crypto
//!
//! AES-256-GCM (RustCrypto `aes-gcm` crate). Each [`Vault::encrypt`] call
//! draws a fresh random 96-bit nonce from the OS CSPRNG (same `OsRng` source
//! `src/authz/mod.rs` already uses for password salts/tokens) — nonces are
//! never reused for a given key, which is the one hard requirement AES-GCM
//! has to stay safe. The stored blob is `base64(nonce(12B) || ciphertext ||
//! tag(16B))`; [`Vault::decrypt`] verifies the GCM authentication tag before
//! returning anything, so any single-bit tamper of the stored blob is
//! detected and rejected rather than silently decrypting to garbage.
//!
//! Base64 is hand-rolled (RFC 4648, standard alphabet, padded) rather than
//! pulling in the `base64` crate as a new unconditional dependency — the
//! same "hand-roll the small thing" call `src/authz/mod.rs` already makes
//! for TOTP's base32.

use crate::error::{DbError, Result};

/// AES-256 key length in bytes.
const KEY_LEN: usize = 32;
/// AES-GCM standard nonce length in bytes (96 bits).
const NONCE_LEN: usize = 12;

const B64_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// RFC 4648 standard base64 encode, with `=` padding.
fn b64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied();
        let b2 = chunk.get(2).copied();
        let n = (b0 as u32) << 16 | (b1.unwrap_or(0) as u32) << 8 | (b2.unwrap_or(0) as u32);
        out.push(B64_ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(B64_ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        out.push(if b1.is_some() {
            B64_ALPHABET[((n >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if b2.is_some() {
            B64_ALPHABET[(n & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn b64_value(c: u8) -> Option<u32> {
    B64_ALPHABET.iter().position(|&b| b == c).map(|p| p as u32)
}

/// Inverse of [`b64_encode`]. Accepts both **padded** (`=`-terminated,
/// length a multiple of 4 — [`b64_encode`]'s own output shape) and
/// **unpadded** input (the trailing group may have 2 or 3 characters, no
/// `=`) — the module doc's "Master key format" section documents both as
/// valid for `UNIDB_MASTER_KEY`. Rejects any character outside the
/// alphabet/padding, a stray non-final short group, or a dangling single
/// leftover character — a corrupt/foreign blob must never panic, only fail
/// closed (mirrors `src/authz/mod.rs`'s `base32_decode` posture).
fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if s.is_empty() {
        return Some(Vec::new());
    }
    // Trailing `=` padding (0, 1, or 2 chars) is stripped up front; the
    // remaining "core" is handled identically whether the input was padded
    // or not — only the *last* group of `core` may be short (2 or 3 chars).
    let core = s.trim_end_matches('=');
    if core.is_empty() || core.len() % 4 == 1 {
        return None; // an all-padding string, or a dangling single char
    }
    let bytes = core.as_bytes();
    let mut out = Vec::with_capacity(core.len() / 4 * 3 + 3);
    let mut chunks = bytes.chunks(4).peekable();
    while let Some(chunk) = chunks.next() {
        let is_last = chunks.peek().is_none();
        if !is_last && chunk.len() != 4 {
            return None; // only the final group may be short
        }
        let mut vals = [0u32; 4];
        for (i, &b) in chunk.iter().enumerate() {
            vals[i] = b64_value(b)?;
        }
        let n = vals[0] << 18 | vals[1] << 12 | vals[2] << 6 | vals[3];
        out.push((n >> 16) as u8);
        if chunk.len() >= 3 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() == 4 {
            out.push(n as u8);
        }
    }
    Some(out)
}

/// Decode a hex string (case-insensitive), `None` on any non-hex char or
/// odd length.
fn hex_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if !s.len().is_multiple_of(2) || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// The 32-byte AES-256 master key. Never `Debug`/`Display`-printed (manual
/// `Debug` below always prints `<redacted>`) and never persisted anywhere —
/// it lives only in this process's memory, read once from `UNIDB_MASTER_KEY`
/// at [`Vault::from_env`] time.
struct MasterKey([u8; KEY_LEN]);

impl std::fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<redacted>")
    }
}

impl MasterKey {
    /// Parse `raw` as base64 or hex (see the module doc's "Master key
    /// format" section), requiring exactly [`KEY_LEN`] decoded bytes. Never
    /// includes `raw` itself in the returned error — only its length and
    /// which encodings were tried — so a malformed-key error can be logged
    /// without leaking whatever the operator actually typed (which, for a
    /// near-miss typo, could be uncomfortably close to the real key).
    fn parse(raw: &str) -> Result<Self> {
        let trimmed = raw.trim();
        // Hex and base64 alphabets overlap (every hex digit is also a valid
        // base64 character), so a 64-hex-digit key is *also* syntactically
        // valid base64 — just the wrong decoded length (48 bytes, not 32).
        // The two valid lengths never collide (64-char hex vs. 43/44-char
        // base64), so pick the decode order by length: try hex first for a
        // 64-char input, base64 first otherwise, falling back to the other
        // if the first attempt fails outright.
        let decoded = if trimmed.len() == 64 {
            hex_decode(trimmed).or_else(|| b64_decode(trimmed))
        } else {
            b64_decode(trimmed).or_else(|| hex_decode(trimmed))
        };
        let Some(bytes) = decoded else {
            return Err(DbError::Authz(format!(
                "UNIDB_MASTER_KEY ({} chars) is not valid base64 or hex — expected a 32-byte \
                 AES-256 key encoded as base64 (44 chars with padding, 43 without) or hex \
                 (64 hex digits); generate one with `openssl rand -base64 32`",
                trimmed.len()
            )));
        };
        if bytes.len() != KEY_LEN {
            return Err(DbError::Authz(format!(
                "UNIDB_MASTER_KEY decodes to {} bytes, expected exactly {KEY_LEN} (an AES-256 \
                 key) — generate one with `openssl rand -base64 32`",
                bytes.len()
            )));
        }
        let mut key = [0u8; KEY_LEN];
        key.copy_from_slice(&bytes);
        Ok(MasterKey(key))
    }
}

/// The encrypt-at-rest secrets vault (item 129 / I3). Construct once via
/// [`Vault::from_env`] (normal startup path — `Engine::open` does this) or
/// [`Vault::disabled`] (explicit no-vault construction, e.g. in tests that
/// don't touch secrets). `Send + Sync` (no interior mutability — the key
/// never changes after construction) so it composes into the shared
/// `Engine`/`EngineHandle` exactly like [`crate::authz::RoleStore`].
pub struct Vault {
    key: Option<MasterKey>,
}

impl std::fmt::Debug for Vault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vault")
            .field("enabled", &self.key.is_some())
            .finish()
    }
}

impl Vault {
    /// Explicit no-vault construction (every secret-backed lookup falls back
    /// to its plaintext source; [`Vault::encrypt`]/[`Vault::decrypt`] both
    /// error). Used by call sites that don't want to touch the environment,
    /// e.g. targeted unit tests.
    pub fn disabled() -> Self {
        Self { key: None }
    }

    /// Load the vault from `UNIDB_MASTER_KEY`. See the module doc's
    /// "Enabled vs. disabled" section for the exact three-way behavior
    /// (unset → disabled+warn; malformed → `Err`; valid → enabled).
    pub fn from_env() -> Result<Self> {
        match std::env::var("UNIDB_MASTER_KEY") {
            Ok(raw) if !raw.is_empty() => {
                let key = MasterKey::parse(&raw)?;
                tracing::info!("UNIDB_MASTER_KEY is set — secrets vault enabled (AES-256-GCM)");
                Ok(Self { key: Some(key) })
            }
            _ => {
                tracing::warn!(
                    "UNIDB_MASTER_KEY is not set — secrets vault disabled; secret-backed \
                     config (e.g. OAuth client secrets) falls back to plaintext env vars. A \
                     secret stored while the vault WAS enabled cannot be read back without the \
                     original key."
                );
                Ok(Self { key: None })
            }
        }
    }

    /// Whether a master key is loaded. `false` means [`Vault::encrypt`]/
    /// [`Vault::decrypt`] both error — callers use this to decide whether to
    /// fall back to a plaintext source instead of even attempting a vault
    /// round-trip.
    pub fn is_enabled(&self) -> bool {
        self.key.is_some()
    }

    fn cipher(&self) -> Result<aes_gcm::Aes256Gcm> {
        use aes_gcm::KeyInit;
        let key = self.key.as_ref().ok_or_else(|| {
            DbError::Authz(
                "secrets vault is disabled (UNIDB_MASTER_KEY is not set) — cannot encrypt/\
                 decrypt a vault secret"
                    .to_string(),
            )
        })?;
        aes_gcm::Aes256Gcm::new_from_slice(&key.0)
            .map_err(|e| DbError::Authz(format!("vault cipher init failed: {e}")))
    }

    /// Encrypt `plaintext` with a fresh random nonce, returning
    /// `base64(nonce || ciphertext || tag)` — the exact form
    /// [`crate::authz::RoleStore::set_secret`] persists. Errors (never
    /// panics) if the vault is disabled — there is deliberately no path that
    /// stores a secret in plaintext when no key is configured.
    pub fn encrypt(&self, plaintext: &str) -> Result<String> {
        use aes_gcm::aead::Aead;
        use argon2::password_hash::rand_core::{OsRng, RngCore};
        let cipher = self.cipher()?;
        let mut nonce_bytes = [0u8; NONCE_LEN];
        OsRng.fill_bytes(&mut nonce_bytes);
        // `Array::from` (not the deprecated `from_slice`) — infallible since
        // `nonce_bytes` is already exactly `NONCE_LEN` bytes.
        let nonce = aes_gcm::Nonce::from(nonce_bytes);
        let ciphertext = cipher
            .encrypt(&nonce, plaintext.as_bytes())
            .map_err(|_| DbError::Authz("vault encryption failed".to_string()))?;
        let mut blob = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        blob.extend_from_slice(&nonce_bytes);
        blob.extend_from_slice(&ciphertext);
        Ok(b64_encode(&blob))
    }

    /// Decrypt a blob produced by [`Vault::encrypt`]. **Fail closed**: an
    /// unparseable blob, a disabled vault, a wrong/rotated master key, or a
    /// tampered ciphertext (GCM authentication tag mismatch) all return
    /// `Err` — there is no code path that returns a "best effort" or partial
    /// plaintext, and never one that falls back to treating the blob itself
    /// as plaintext. Never logs `blob` or the decrypted plaintext.
    pub fn decrypt(&self, blob: &str) -> Result<String> {
        use aes_gcm::aead::Aead;
        let cipher = self.cipher()?;
        let raw = b64_decode(blob)
            .ok_or_else(|| DbError::Authz("stored secret is not valid base64".to_string()))?;
        if raw.len() < NONCE_LEN {
            return Err(DbError::Authz(
                "stored secret ciphertext is too short to contain a nonce".to_string(),
            ));
        }
        let (nonce_slice, ciphertext) = raw.split_at(NONCE_LEN);
        let mut nonce_bytes = [0u8; NONCE_LEN];
        nonce_bytes.copy_from_slice(nonce_slice);
        let nonce = aes_gcm::Nonce::from(nonce_bytes);
        let plaintext = cipher.decrypt(&nonce, ciphertext).map_err(|_| {
            DbError::Authz(
                "vault decrypt failed: wrong master key or tampered/corrupt ciphertext".to_string(),
            )
        })?;
        String::from_utf8(plaintext)
            .map_err(|_| DbError::Authz("decrypted secret is not valid UTF-8".to_string()))
    }
}

/// Test-only constructor that bypasses the environment entirely (item 129).
/// `cfg(test)` is a crate-wide flag under `cargo test`, so this is reachable
/// from `src/authz/mod.rs`'s own `#[cfg(test)] mod tests` — those tests use
/// this instead of `Vault::from_env()` + `std::env::set_var`/`remove_var` on
/// `UNIDB_MASTER_KEY`, which would otherwise race against every other test
/// in the same `cargo test --lib` process (env vars are process-global,
/// tests run in parallel by default).
#[cfg(test)]
impl Vault {
    pub(crate) fn for_test(key_bytes: [u8; KEY_LEN]) -> Self {
        Self {
            key: Some(MasterKey(key_bytes)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key_b64() -> String {
        b64_encode(&[7u8; KEY_LEN])
    }

    fn test_key_hex() -> String {
        (0..KEY_LEN).map(|_| "0a").collect::<String>()
    }

    #[test]
    fn b64_roundtrip() {
        for data in [
            b"".as_slice(),
            b"a",
            b"ab",
            b"abc",
            b"abcd",
            &[0u8, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12],
            &[255u8; 32],
        ] {
            let encoded = b64_encode(data);
            let decoded = b64_decode(&encoded).expect("decode should succeed");
            assert_eq!(decoded, data, "roundtrip mismatch for {data:?}");
        }
    }

    #[test]
    fn b64_decode_rejects_garbage() {
        assert!(b64_decode("not base64!!").is_none());
        assert!(b64_decode("a").is_none()); // single dangling char, never valid
        assert!(b64_decode("abc").is_some()); // unpadded 3-char tail IS valid (2 bytes)
    }

    #[test]
    fn master_key_parses_base64_and_hex() {
        assert!(MasterKey::parse(&test_key_b64()).is_ok());
        assert!(MasterKey::parse(&test_key_hex()).is_ok());
    }

    #[test]
    fn master_key_rejects_wrong_length() {
        let err = MasterKey::parse("dG9vc2hvcnQ=").unwrap_err(); // "tooshort" b64, way < 32 bytes
        assert!(format!("{err}").contains("32"));
    }

    #[test]
    fn master_key_rejects_garbage() {
        assert!(MasterKey::parse("not-a-valid-key-at-all!!!").is_err());
    }

    #[test]
    fn disabled_vault_errors_on_encrypt_and_decrypt() {
        let vault = Vault::disabled();
        assert!(!vault.is_enabled());
        assert!(vault.encrypt("secret").is_err());
        assert!(vault.decrypt("anything").is_err());
    }

    #[test]
    fn encrypt_then_decrypt_roundtrips() {
        let key = test_key_b64();
        let vault = Vault {
            key: Some(MasterKey::parse(&key).unwrap()),
        };
        let blob = vault.encrypt("hunter2-client-secret").unwrap();
        assert_ne!(blob, "hunter2-client-secret");
        assert!(!blob.contains("hunter2"));
        let plain = vault.decrypt(&blob).unwrap();
        assert_eq!(plain, "hunter2-client-secret");
    }

    #[test]
    fn each_encryption_uses_a_fresh_nonce() {
        let vault = Vault {
            key: Some(MasterKey::parse(&test_key_b64()).unwrap()),
        };
        let b1 = vault.encrypt("same-plaintext").unwrap();
        let b2 = vault.encrypt("same-plaintext").unwrap();
        assert_ne!(
            b1, b2,
            "two encryptions of the same plaintext must differ (random nonce)"
        );
    }

    #[test]
    fn wrong_key_fails_closed() {
        let vault_a = Vault {
            key: Some(MasterKey::parse(&test_key_b64()).unwrap()),
        };
        let blob = vault_a.encrypt("top-secret").unwrap();
        let vault_b = Vault {
            key: Some(MasterKey::parse(&test_key_hex()).unwrap()),
        };
        let err = vault_b.decrypt(&blob).unwrap_err();
        assert!(format!("{err}").contains("decrypt failed"));
    }

    #[test]
    fn tampered_ciphertext_fails_closed() {
        let vault = Vault {
            key: Some(MasterKey::parse(&test_key_b64()).unwrap()),
        };
        let blob = vault.encrypt("top-secret").unwrap();
        let mut raw = b64_decode(&blob).unwrap();
        let last = raw.len() - 1;
        raw[last] ^= 0xff; // flip a bit in the GCM tag
        let tampered = b64_encode(&raw);
        assert!(vault.decrypt(&tampered).is_err());
    }

    #[test]
    fn debug_never_leaks_the_key_material() {
        let vault = Vault {
            key: Some(MasterKey::parse(&test_key_b64()).unwrap()),
        };
        let s = format!("{vault:?}");
        assert!(!s.contains(&test_key_b64()));
        assert!(s.contains("enabled"));
    }
}
