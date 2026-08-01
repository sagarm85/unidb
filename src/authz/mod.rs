// Users, roles, and privileges (P6.e / item-24 Z1).
//
// A persisted authorization store: users (optionally superuser), roles, role
// membership (users→roles and roles→roles, resolved transitively), and
// per-table privileges (SELECT/INSERT/UPDATE/DELETE, or ALL). It is the identity
// + access-control layer that turns the single-shared-JWT server into per-user
// auth, and gives the embedded API a `GRANT`/`REVOKE` surface.
//
// **Superuser model:** the embedded API runs as an implicit superuser (identity
// `None`) — every existing embedded call is unchanged and unrestricted. A named
// user created `SUPERUSER` can also administer. Auth DDL (CREATE/DROP USER|ROLE,
// GRANT, REVOKE, CREATE/DROP POLICY) and schema DDL require superuser in v1;
// data statements (SELECT/INSERT/UPDATE/DELETE) require the matching privilege
// on the table.
//
// The auth DDL grammar is small and parsed here (not via `sqlparser`, whose
// GRANT/ROLE AST is awkward) so the surface stays controlled:
//
//   CREATE USER <name> [SUPERUSER]        DROP USER <name>
//   CREATE ROLE <name>                    DROP ROLE <name>
//   GRANT <priv,.. | ALL> ON <table> TO <grantee>
//   GRANT <role> TO <grantee>                       (role membership)
//   REVOKE <priv,.. | ALL> ON <table> FROM <grantee>
//   REVOKE <role> FROM <grantee>
//   CREATE POLICY <name> ON <table> FOR <op> [TO <role,...>] USING (<predicate>) [WITH CHECK (<expr>)]
//   DROP POLICY <name> ON <table>
//
// Roles/users/grants persist to `roles.json` (control-plane metadata, so
// `serde` is fine per CLAUDE.md §4). Named policies persist in the catalog
// blob (alongside `rls_policy`) so there is no FORMAT_VERSION bump.
// `Send + Sync` for the shared `Engine`.
//
// **Built-in roles (item 122, B3):** `anon`, `authenticated`, and
// `service_role` are reserved — `CREATE ROLE`/`DROP ROLE` reject these three
// names (see `is_reserved_role`). They are never persisted as ordinary roles;
// [`RoleStore::effective_roles`] resolves them for a caller on the fly from
// their subject/verified claims (Supabase convention — see that method's doc
// comment for the exact mapping). `service_role` bypasses RLS like a
// superuser, on the audited path (item-103 lesson): see `Engine::
// execute_sql_inner_as_principal` / `ReadHandle::execute_sql_inner`.
//
// **Role-scoped policies (item 122, B4):** the optional `TO <role,...>`
// clause above persists as `PolicyDef::target_roles`. A policy with no `TO`
// clause has an empty `target_roles` and applies to every caller exactly as
// before B4 (back-compat) via the merged `rls_policy`/`update_policy`/
// `delete_policy`/`insert_policy` catalog fields; a `TO`-scoped policy is
// instead re-evaluated, role-gated, at plan/injection time by
// `crate::sql::logical::apply_rls_with_auth` — see that function's doc
// comment for the exact semantics, including the Postgres-style
// deny-by-default when a `TO`-scoped policy exists but none of its target
// roles match the caller.

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

use argon2::{
    password_hash::{
        rand_core::{OsRng, RngCore},
        PasswordHash, PasswordHasher, PasswordVerifier, SaltString,
    },
    Argon2,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{DbError, Result};

/// Hash `password` with argon2id (library defaults: m=19 MiB, t=2, p=1),
/// returning the self-describing PHC string (algorithm + params + salt +
/// hash) that [`verify_password_hash`] can verify against later. Never
/// returns or logs the plaintext.
fn hash_password(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| DbError::Authz(format!("password hash: {e}")))
}

/// Constant-shape verify: parses the PHC `hash` string and checks `password`
/// against it. Returns `false` (never an error) on a malformed stored hash —
/// recovery code must not panic on bad control-plane metadata, and a corrupt
/// hash should simply fail closed.
fn verify_password_hash(hash: &str, password: &str) -> bool {
    match PasswordHash::new(hash) {
        Ok(parsed) => Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

/// A fixed, process-lifetime-stable argon2id hash used to run a same-cost
/// "verify" when the target user doesn't exist or has no stored credential,
/// so `RoleStore::verify_password` does the same amount of work (and takes
/// the same code path) regardless of *why* login should fail — no
/// user-enumeration timing/shape oracle. Computed once per process (not a
/// hash of any real credential) and cached; the exact bytes are irrelevant,
/// only that a real argon2id verify runs against them.
fn dummy_hash() -> &'static str {
    static DUMMY: OnceLock<String> = OnceLock::new();
    DUMMY
        .get_or_init(|| {
            hash_password("unidb-dummy-password-for-timing-parity").unwrap_or_else(|_| {
                // Fallback fixed PHC string in the vanishingly unlikely case
                // hashing itself fails (e.g. RNG unavailable) — still a valid
                // argon2id hash shape, just not freshly salted.
                "$argon2id$v=19$m=19456,t=2,p=1$AAAAAAAAAAAAAAAAAAAAAA$\
                 AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
                    .to_string()
            })
        })
        .as_str()
}

/// Refresh-token session lifetime (item 121, A4): 30 days. Not yet
/// configurable — a fixed, documented default is preferable to silently
/// picking a value; revisit alongside A5 (production issuer config) if a
/// real deployment needs this tunable.
const REFRESH_TOKEN_TTL_SECS: u64 = 30 * 24 * 3600;

/// Current wall-clock time as Unix seconds. Saturates to 0 rather than
/// panicking if the clock is somehow before the epoch (recovery/control-plane
/// code must never panic on an unusual but non-corrupt environment).
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Hex-encoded SHA-256 digest of `input`. Used to turn a raw opaque refresh
/// token into the value actually persisted (item 121, A4) — the raw token
/// itself is *never* written to `roles.json`, mirroring the "only the hash
/// is stored" posture [`hash_password`] already gives credentials. Because
/// verification always compares hashes (never the raw secret), there is no
/// direct-comparison code path over the raw token for a timing side channel
/// to attach to in the first place.
fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Generate a new high-entropy opaque refresh token (item 121, A4): 256 bits
/// from the OS CSPRNG (the same `OsRng` [`hash_password`] uses for salts),
/// hex-encoded. Deliberately **not** a JWT — it carries no claims and is
/// meaningless outside a `sessions` lookup, so it cannot be inspected,
/// forged, or replayed without the server's own persisted state agreeing.
fn generate_refresh_token() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Generate a new opaque session id (item 4 — Studio session listing /
/// revoke-by-id): 128 bits from the OS CSPRNG, hex-encoded. A **separate,
/// independent** random draw from [`generate_refresh_token`] — never derived
/// from the raw token or its SHA-256 hash (not a prefix, not a
/// transformation) — so surfacing it via `unidb_catalog.sessions` can never
/// leak any bit of the token or the hash keyed in [`AuthState::sessions`].
fn generate_session_id() -> String {
    let mut bytes = [0u8; 16];
    OsRng.fill_bytes(&mut bytes);
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Generate a new single-use MFA challenge token (item 127, Workstream D4):
/// 256 bits from the OS CSPRNG, hex-encoded — same shape and construction as
/// [`generate_refresh_token`], but a **separate token space**: an MFA
/// challenge is never accepted where a refresh token is expected or vice
/// versa (they key distinct maps, [`AuthState::sessions`] vs
/// [`AuthState::mfa_challenges`]). Only its SHA-256 hash is ever persisted —
/// see [`RoleStore::create_mfa_challenge`].
fn generate_mfa_challenge_token() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Generate a new single-use OAuth CSRF `state` token (item 128, Workstream
/// D1): 256 bits from the OS CSPRNG, hex-encoded — same shape/construction
/// as [`generate_mfa_challenge_token`], but its own token space (keys
/// [`AuthState::oauth_states`], never accepted where a refresh/MFA-challenge
/// token is expected or vice versa). Only its SHA-256 hash is ever
/// persisted — see [`RoleStore::create_oauth_state`].
fn generate_oauth_state_token() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Generate a new single-use password-recovery token (item 138): 256 bits
/// from the OS CSPRNG, hex-encoded — same shape/construction as
/// [`generate_oauth_state_token`], but its own token space (keys
/// [`AuthState::recovery_tokens`], never accepted where a refresh/MFA/OAuth
/// token is expected or vice versa). Only its SHA-256 hash is ever
/// persisted — see [`RoleStore::create_recovery_token`].
fn generate_recovery_token() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Generate a new single-use magic-link login token (item 138): 256 bits
/// from the OS CSPRNG, hex-encoded — its own token space (keys
/// [`AuthState::magiclink_tokens`]), same construction/posture as
/// [`generate_recovery_token`].
fn generate_magiclink_token() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Generate a new PKCE (RFC 7636) `code_verifier` (item 128): 256 bits from
/// the OS CSPRNG, hex-encoded to a 64-character string. Hex digits are all
/// within RFC 7636's `code_verifier` charset (`ALPHA / DIGIT / "-" / "." /
/// "_" / "~"`) and 64 characters sits comfortably inside the mandated
/// 43-128 length range, so no separate base64url step is needed here —
/// unlike the PKCE `code_challenge` (a SHA-256 digest, which genuinely does
/// need base64url encoding), which `src/server/oauth.rs` computes at
/// authorize time using the `base64`/`sha2` crates already available there.
/// Persisted server-side only (never sent to the client) — see
/// [`OAuthStateRec::code_verifier`].
fn generate_oauth_code_verifier() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

// ── item 127 (Workstream D4): TOTP-based MFA (RFC 6238) ────────────────────
//
// A small, self-contained HOTP/TOTP + base32 implementation on top of the
// `hmac`/`sha1` crates (both already resolve in `Cargo.lock` as transitive
// deps of the existing `jsonwebtoken`/`ring` chain — see `Cargo.toml`'s
// comment). No external TOTP/base32 crate: base32 (RFC 4648) is ~20 lines
// each way and keeps the dependency surface exactly what CLAUDE.md's coding
// conventions ask for (the same "hand-roll the small thing" call the DER/JWK
// reader in `server/auth.rs` already makes).

/// RFC 6238 default time-step size: 30 seconds.
const TOTP_STEP_SECS: u64 = 30;
/// Clock-skew tolerance either side of "now", in whole steps (±1 step =
/// ±30 s, so a code is accepted for a 90 s window centered on issuance —
/// the standard Google-Authenticator-compatible tolerance).
const TOTP_SKEW_STEPS: i64 = 1;
/// MFA challenge token lifetime (item 127): long enough to read a 6-digit
/// code off an authenticator app and type it in, short enough that a leaked
/// challenge (e.g. captured in a proxy access log) is a narrow window, never
/// a standing credential the way a refresh token is. `pub` so the server
/// layer (`handlers.rs`) can echo the real TTL in `POST /auth/login`'s
/// `mfa_required` response instead of duplicating the literal.
pub const MFA_CHALLENGE_TTL_SECS: u64 = 5 * 60;
/// Number of one-time recovery codes issued when MFA enrollment is confirmed
/// (item 127). Matches the common industry default (GitHub/Google issue a
/// similar-sized batch) — enough to cover several device-loss recoveries
/// without becoming an unwieldy list to save.
const RECOVERY_CODE_COUNT: usize = 8;

/// OAuth authorize-flow CSRF-state / PKCE-verifier lifetime (item 128,
/// Workstream D1): long enough for a user to complete the provider's own
/// login+consent UI (which can involve their own MFA, an account picker,
/// etc.), short enough that a captured-but-never-redeemed authorize URL
/// isn't a standing liability. `pub` so `src/server/oauth.rs` can echo the
/// real TTL without duplicating the literal (mirrors `MFA_CHALLENGE_TTL_SECS`).
pub const OAUTH_STATE_TTL_SECS: u64 = 10 * 60;

/// Password-recovery token lifetime (item 138 — `POST /auth/recover`): long
/// enough to receive and act on an email, short enough that a leaked/unused
/// link isn't a standing liability — matches the spec's "~1h" guidance and
/// common industry practice (GitHub/Google use a similar order of
/// magnitude). `pub` so `src/server/email.rs`/`handlers.rs` can echo the
/// real TTL without duplicating the literal.
pub const RECOVERY_TOKEN_TTL_SECS: u64 = 60 * 60;

/// Magic-link login token lifetime (item 138 — `POST /auth/magiclink`):
/// deliberately much shorter than [`RECOVERY_TOKEN_TTL_SECS`] — a magic link
/// mints a full session on redemption (no second factor like a password),
/// so it gets the same narrow window as an MFA login challenge
/// ([`MFA_CHALLENGE_TTL_SECS`]) rather than the recovery flow's longer one.
pub const MAGICLINK_TOKEN_TTL_SECS: u64 = 15 * 60;

const BASE32_ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// RFC 4648 base32 encode, no padding (`=`) — the conventional shape for a
/// TOTP `otpauth://` secret parameter (every mainstream authenticator app
/// accepts unpadded base32 there).
fn base32_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(5) * 8);
    let mut buffer: u32 = 0;
    let mut bits_left: u32 = 0;
    for &byte in data {
        buffer = (buffer << 8) | byte as u32;
        bits_left += 8;
        while bits_left >= 5 {
            bits_left -= 5;
            let idx = ((buffer >> bits_left) & 0x1f) as usize;
            out.push(BASE32_ALPHABET[idx] as char);
        }
    }
    if bits_left > 0 {
        let idx = ((buffer << (5 - bits_left)) & 0x1f) as usize;
        out.push(BASE32_ALPHABET[idx] as char);
    }
    out
}

/// Inverse of [`base32_encode`]. Case-insensitive, tolerates (and strips)
/// trailing `=` padding a client might send back. `None` on any character
/// outside the RFC 4648 alphabet — a corrupt/foreign secret must never panic
/// recovery code, only fail closed (mirrors [`verify_password_hash`]'s "bad
/// stored data ⇒ `false`/`None`, never panic" posture).
fn base32_decode(s: &str) -> Option<Vec<u8>> {
    let mut buffer: u32 = 0;
    let mut bits_left: u32 = 0;
    let mut out = Vec::with_capacity(s.len() * 5 / 8);
    for c in s.trim().trim_end_matches('=').chars() {
        let upper = c.to_ascii_uppercase();
        let val = BASE32_ALPHABET.iter().position(|&b| b as char == upper)? as u32;
        buffer = (buffer << 5) | val;
        bits_left += 5;
        if bits_left >= 8 {
            bits_left -= 8;
            out.push(((buffer >> bits_left) & 0xff) as u8);
        }
    }
    Some(out)
}

/// Generate a fresh 160-bit TOTP secret (RFC 6238's recommended key length
/// for HMAC-SHA1) from the OS CSPRNG.
fn generate_totp_secret() -> Vec<u8> {
    let mut bytes = [0u8; 20];
    OsRng.fill_bytes(&mut bytes);
    bytes.to_vec()
}

/// Generate one one-time recovery code (item 127): 48 bits from the OS
/// CSPRNG, rendered as two 6-hex-digit groups (`xxxxxx-xxxxxx`) for
/// readability. Only its SHA-256 hash is ever persisted — see
/// [`RoleStore::mfa_confirm`] — the plaintext is returned to the caller
/// exactly once.
fn generate_recovery_code() -> String {
    let mut bytes = [0u8; 6];
    OsRng.fill_bytes(&mut bytes);
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}-{}", &hex[0..6], &hex[6..12])
}

/// Constant-time string comparison (equal-length inputs; unequal lengths
/// short-circuit — length is not the secret here, the *content* is). Used
/// for both the 6-digit TOTP code compare and the recovery-code hash
/// compare, so neither leaks timing information about how many leading
/// digits/hex-chars matched.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (ab, bb) = (a.as_bytes(), b.as_bytes());
    if ab.len() != bb.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in ab.iter().zip(bb.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// RFC 4226 HOTP: HMAC-SHA1 over the big-endian 8-byte `counter`, dynamic
/// truncation, mod 10^6 for a 6-digit code. `None` only if `hmac` rejects
/// the key outright (it never does for HMAC — any key length is valid — but
/// the crate's API is fallible, and this module never panics on it per
/// CLAUDE.md's "no `unwrap`/`expect` outside tests" rule).
fn hotp(secret: &[u8], counter: u64) -> Option<u32> {
    use hmac::{Hmac, KeyInit, Mac};
    use sha1::Sha1;
    let mut mac = Hmac::<Sha1>::new_from_slice(secret).ok()?;
    mac.update(&counter.to_be_bytes());
    let result = mac.finalize().into_bytes();
    let offset = (result[result.len() - 1] & 0x0f) as usize;
    let bytes = result.get(offset..offset + 4)?;
    let code = ((bytes[0] as u32 & 0x7f) << 24)
        | ((bytes[1] as u32) << 16)
        | ((bytes[2] as u32) << 8)
        | (bytes[3] as u32);
    Some(code % 1_000_000)
}

/// RFC 6238 TOTP at a given step counter: [`hotp`] with `counter = step`.
fn totp_at_step(secret: &[u8], step: u64) -> Option<String> {
    hotp(secret, step).map(|code| format!("{code:06}"))
}

/// Verify a caller-supplied 6-digit `code` against `secret`'s current TOTP
/// window (±[`TOTP_SKEW_STEPS`] steps of clock skew either side of "now"),
/// with **replay protection**: `last_used_step`, if `Some`, is the most
/// recent step this secret has already been used to authenticate — any
/// step `<= last_used_step` is rejected outright, so the exact code (or any
/// earlier-window code) just consumed can never authenticate a second time,
/// even if an attacker captures and instantly replays it. Constant-time code
/// compare via [`constant_time_eq`]. Returns `Some(step)` (the step that
/// matched — the caller persists it as the new `last_used_step`) on success,
/// `None` uniformly for a malformed code, no match in the window, or a
/// replayed step.
fn verify_totp_code(secret: &[u8], code: &str, last_used_step: Option<i64>) -> Option<i64> {
    let code = code.trim();
    if code.len() != 6 || !code.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let now = now_secs();
    let current_step = (now / TOTP_STEP_SECS) as i64;
    for delta in -TOTP_SKEW_STEPS..=TOTP_SKEW_STEPS {
        let step = current_step + delta;
        if step < 0 {
            continue;
        }
        if let Some(last) = last_used_step {
            if step <= last {
                continue;
            }
        }
        let Some(expected) = totp_at_step(secret, step as u64) else {
            continue;
        };
        if constant_time_eq(&expected, code) {
            return Some(step);
        }
    }
    None
}

/// A user's TOTP enrollment (item 127, Workstream D4). `enabled == false` is
/// the **pending** state after `POST /auth/mfa/enroll` — a secret exists but
/// login does not yet require it, and no recovery codes have been minted.
/// `enabled == true` (set by [`RoleStore::mfa_confirm`]) is the live,
/// login-gating state. Never `Debug`-printed directly by any call site in
/// this crate; [`AuthState`]'s manual `Debug` (below) redacts the whole
/// `mfa` map to a count, the same posture as `credentials`/`sessions`.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct MfaRecord {
    /// Base32-encoded 160-bit TOTP secret. Never logged, never returned by
    /// `whoami`, only ever handed to the client once at enrollment time.
    secret_base32: String,
    enabled: bool,
    /// SHA-256 hashes of the still-unused one-time recovery codes (never the
    /// plaintext — same posture as [`AuthState::credentials`]). A used code
    /// is removed from this list (single-use), never merely flagged.
    #[serde(default)]
    recovery_code_hashes: Vec<String>,
    /// The most recent TOTP step successfully consumed for this secret
    /// (replay protection — see [`verify_totp_code`]). `None` until the
    /// first successful verification.
    #[serde(default)]
    last_used_step: Option<i64>,
}

/// A pending, single-use MFA login challenge (item 127): issued by `POST
/// /auth/login` when the authenticating user has MFA enabled, in place of a
/// full session. Keyed in [`AuthState::mfa_challenges`] by the SHA-256 hash
/// of the raw opaque challenge token — the raw token itself is never
/// persisted, mirroring [`SessionRec`]'s "only the hash is stored" posture.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct MfaChallengeRec {
    username: String,
    issued_at: u64,
    expires_at: u64,
    /// Single-use: set the moment a challenge is successfully redeemed via
    /// [`RoleStore::verify_mfa_challenge`] so the same challenge (even
    /// re-presented with a still-fresh TOTP code before it expires) can
    /// never mint a second session.
    used: bool,
}

/// A pending, single-use OAuth authorize-flow record (item 128, Workstream
/// D1): issued by `GET /auth/oauth/<provider>/authorize`, redeemed by `GET
/// /auth/oauth/<provider>/callback`. Keyed in [`AuthState::oauth_states`] by
/// the SHA-256 hash of the raw opaque CSRF `state` token — the raw token is
/// never persisted, mirroring [`SessionRec`]/[`MfaChallengeRec`]'s
/// "hash-only" posture.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct OAuthStateRec {
    /// Which provider this state was minted for ("google"/"github") — the
    /// callback route re-checks this so a `state` issued for one provider
    /// can never be redeemed against a different provider's callback.
    provider: String,
    /// PKCE (RFC 7636) code verifier generated alongside `state` — never
    /// sent to the client, only ever used server-side at token-exchange
    /// time (`src/server/oauth.rs`). Kept out of `Debug` (see
    /// [`AuthState`]'s manual impl) — the whole point of PKCE is that this
    /// value stays a server-only secret for the life of the flow.
    code_verifier: String,
    issued_at: u64,
    expires_at: u64,
    /// Single-use: set the moment the state is redeemed
    /// ([`RoleStore::verify_oauth_state`]) so a captured/replayed callback
    /// URL (e.g. from a proxy access log or shared browser history) can
    /// never be redeemed a second time.
    used: bool,
}

/// A pending, single-use auth-email token (item 138 — password recovery /
/// magic-link login). Keyed in [`AuthState::recovery_tokens`]/
/// [`AuthState::magiclink_tokens`] (two independent token spaces, never
/// cross-accepted — a recovery token can never redeem a magic-link login and
/// vice versa) by the SHA-256 hash of the raw opaque token — the raw token
/// itself is never persisted, mirroring [`OAuthStateRec`]/[`MfaChallengeRec`]/
/// [`SessionRec`]'s "hash-only" posture. Shared shape for both flows (both
/// are "a username plus an issue/expiry/used-once envelope") — see
/// [`RoleStore::create_recovery_token`]/[`RoleStore::create_magiclink_token`].
#[derive(Clone, Debug, Serialize, Deserialize)]
struct EmailTokenRec {
    username: String,
    issued_at: u64,
    expires_at: u64,
    /// Single-use: set the moment the token is redeemed (`verify_*_token`)
    /// so a captured/replayed link (e.g. from an email-forwarding service's
    /// link-prefetch, or a shared inbox) can never be redeemed a second
    /// time.
    used: bool,
}

/// A persisted refresh-token session (item 121, A4). Keyed in [`AuthState::
/// sessions`] by the SHA-256 hash of the raw opaque refresh token — the raw
/// token is never stored, only ever handed to the client once, at issuance
/// or rotation time.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct SessionRec {
    /// Opaque session id (item 4), independent of the token/hash map key —
    /// see [`generate_session_id`]. `#[serde(default)]` (empty string) so a
    /// pre-item-4 `roles.json` deserializes without a FORMAT_VERSION bump;
    /// [`RoleStore::open`] backfills any empty id with a freshly generated
    /// one immediately after load, so every in-memory `SessionRec` always
    /// has a non-empty id once the store is open.
    #[serde(default)]
    session_id: String,
    username: String,
    issued_at: u64,
    expires_at: u64,
    revoked: bool,
}

/// Non-secret session summary for `unidb_catalog.sessions` (item 4 — Studio
/// session listing). Deliberately excludes the raw refresh token and its
/// SHA-256 hash; carries only what's safe to show a caller viewing their own
/// (or, for a superuser, anyone's) active sessions.
#[derive(Clone, Debug)]
pub struct SessionView {
    pub session_id: String,
    pub username: String,
    pub issued_at: u64,
    pub expires_at: u64,
    pub revoked: bool,
}

/// Per-user admin-surface state (item 142 — `auth.admin` user management):
/// a ban flag plus Supabase-style split metadata (`app_metadata` is
/// admin-only; `user_metadata` is admin-writable in v1, see the item-142
/// backlog doc's non-goals for the deferred self-service case). Every
/// field is `#[serde(default)]` (and so is [`AuthState::user_extra`]
/// itself) so a pre-142 `roles.json` deserializes unchanged — no
/// FORMAT_VERSION bump. A user gets an entry the moment it's created (see
/// [`RoleStore::apply`]'s `CreateUser` branch) whether via `CREATE USER`
/// SQL or `POST /auth/admin/users`; a **missing** entry (only possible for
/// a `roles.json` written before this item shipped) reads as
/// `Default::default()` via [`RoleStore::admin_view_locked`] — unbanned,
/// empty metadata, `created_at: 0` (meaning "unknown," never treated as a
/// real Unix timestamp).
#[derive(Clone, Debug, Serialize, Deserialize)]
struct UserExtra {
    #[serde(default)]
    banned: bool,
    #[serde(default)]
    created_at: u64,
    /// Admin-only — never settable through any non-admin route (item 142's
    /// security contract). Empty object, not null, by default so a caller
    /// can always index into it without a null check.
    #[serde(default = "default_metadata_object")]
    app_metadata: serde_json::Value,
    /// User-facing metadata; admin-writable in v1 (see the item-142 backlog
    /// doc's non-goals for self-service writes).
    #[serde(default = "default_metadata_object")]
    user_metadata: serde_json::Value,
}

impl Default for UserExtra {
    fn default() -> Self {
        Self {
            banned: false,
            created_at: 0,
            app_metadata: default_metadata_object(),
            user_metadata: default_metadata_object(),
        }
    }
}

fn default_metadata_object() -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::new())
}

/// Admin-surface view of one user (item 142): everything `GET
/// /auth/admin/users` / `GET /auth/admin/users/{id}` return — **never** a
/// password hash, refresh token, or session detail (see [`RoleStore::
/// users_admin_page`]/[`RoleStore::user_admin_view`]).
#[derive(Clone, Debug)]
pub struct AdminUserView {
    pub username: String,
    pub is_superuser: bool,
    pub banned: bool,
    /// Direct role memberships (not transitive) — same scope as
    /// [`RoleStore::roles_for`], reused by `GET /auth/whoami`.
    pub roles: Vec<String>,
    /// Unix seconds, or `0` for "unknown" (a user created before item 142
    /// shipped — see [`UserExtra`]'s doc comment).
    pub created_at: u64,
    pub app_metadata: serde_json::Value,
    pub user_metadata: serde_json::Value,
}

/// Reserved built-in role: an unauthenticated / no-subject caller (item 122,
/// B3 — Supabase convention). Never created/dropped by `CREATE`/`DROP ROLE`;
/// assigned automatically by [`RoleStore::effective_roles`].
pub const ANON_ROLE: &str = "anon";
/// Reserved built-in role: any caller with a verified subject (item 122, B3).
/// Assigned automatically alongside the user's own transitively-granted
/// roles; never created/dropped by `CREATE`/`DROP ROLE`.
pub const AUTHENTICATED_ROLE: &str = "authenticated";
/// Reserved built-in role: a token whose verified claims carry
/// `"role": "service_role"` (item 122, B3 — Supabase convention). Bypasses
/// RLS like a superuser, but stays on the audited path (item-103 lesson) —
/// see `Engine::execute_sql_inner_as_principal`'s `is_service_role` gate and
/// `ReadHandle::execute_sql_inner`'s mirror of it. Never created/dropped by
/// `CREATE`/`DROP ROLE`.
pub const SERVICE_ROLE: &str = "service_role";

const RESERVED_ROLES: [&str; 3] = [ANON_ROLE, AUTHENTICATED_ROLE, SERVICE_ROLE];

/// Whether `name` is one of the three built-in roles (item 122, B3) — these
/// are assigned automatically by [`RoleStore::effective_roles`], never via
/// `CREATE ROLE`/`DROP ROLE` (rejected in [`RoleStore::apply`]).
pub fn is_reserved_role(name: &str) -> bool {
    RESERVED_ROLES.contains(&name)
}

/// A table-level privilege.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Privilege {
    Select,
    Insert,
    Update,
    Delete,
}

impl Privilege {
    fn parse(s: &str) -> Option<Privilege> {
        match s.to_ascii_uppercase().as_str() {
            "SELECT" => Some(Privilege::Select),
            "INSERT" => Some(Privilege::Insert),
            "UPDATE" => Some(Privilege::Update),
            "DELETE" => Some(Privilege::Delete),
            _ => None,
        }
    }
    /// The four grantable privileges (what `ALL` expands to).
    fn all() -> [Privilege; 4] {
        [
            Privilege::Select,
            Privilege::Insert,
            Privilege::Update,
            Privilege::Delete,
        ]
    }
    /// String representation for catalog exposure (Z5).
    pub fn as_str(self) -> &'static str {
        match self {
            Privilege::Select => "SELECT",
            Privilege::Insert => "INSERT",
            Privilege::Update => "UPDATE",
            Privilege::Delete => "DELETE",
        }
    }
}

/// Column scope of one granted privilege (item 112). `All` is the pre-112
/// whole-table grant (every column, including columns added later by
/// `ALTER TABLE ADD COLUMN` — matches Postgres and this engine's own
/// pre-112 behavior); `Columns` narrows to exactly the listed set.
///
/// **Semantics (must match Postgres exactly — see the item-112 backlog
/// doc):** holding table-level (`All`) grants every column; a column-scoped
/// grant admits only its listed columns and everything else is a hard
/// `PERMISSION_DENIED`, never a silently NULL-filled/omitted column.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum GrantScope {
    /// Whole-table — every column, present and future.
    All,
    /// Only these columns.
    Columns(BTreeSet<String>),
}

/// The resolved column-level access a caller has for one `(table, privilege)`
/// pair (item 112), after transitively resolving role membership exactly
/// like [`RoleStore::has_privilege`] — returned by [`RoleStore::column_grant`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ColumnGrant {
    /// No grant of this privilege at all (through any role).
    None,
    /// Whole-table — every column. Also returned for a superuser.
    All,
    /// Only these columns (the union across every grantee path that resolved
    /// to a column-scoped — never whole-table — grant).
    Columns(BTreeSet<String>),
}

/// One grantee's per-privilege grants on one table:
/// `Privilege -> GrantScope`. A thin newtype (rather than a bare
/// `BTreeMap<Privilege, GrantScope>`) purely so it can carry a custom
/// [`Deserialize`] impl (below) that also accepts the **pre-112 on-disk
/// shape** — a bare JSON array of privilege names, e.g. `["Select",
/// "Insert"]` — reading every entry as `GrantScope::All`. This is the
/// back-compat contract: an existing `roles.json` written before item 112
/// deserializes unchanged, with every existing grant behaving exactly as
/// before (whole-table). No `FORMAT_VERSION`/data migration needed — see
/// the module doc's "no data migration should be needed" note.
#[derive(Clone, Debug, Default, Serialize)]
struct TablePrivs(BTreeMap<Privilege, GrantScope>);

impl std::ops::Deref for TablePrivs {
    type Target = BTreeMap<Privilege, GrantScope>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl std::ops::DerefMut for TablePrivs {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<'de> Deserialize<'de> for TablePrivs {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // `roles.json` is always JSON (see `RoleStore::open`/`persist`), so
        // shelling out to `serde_json::Value` to distinguish the legacy
        // array shape from the item-112 object shape is exact, not a
        // best-effort sniff — unlike the page/WAL format (D9), this is
        // control-plane metadata where `serde_json` is already the module's
        // documented choice (see the module doc).
        let value = serde_json::Value::deserialize(deserializer)?;
        match &value {
            serde_json::Value::Array(_) => {
                let privs: BTreeSet<Privilege> =
                    serde_json::from_value(value).map_err(serde::de::Error::custom)?;
                Ok(TablePrivs(
                    privs.into_iter().map(|p| (p, GrantScope::All)).collect(),
                ))
            }
            serde_json::Value::Object(_) => {
                let map: BTreeMap<Privilege, GrantScope> =
                    serde_json::from_value(value).map_err(serde::de::Error::custom)?;
                Ok(TablePrivs(map))
            }
            other => Err(serde::de::Error::custom(format!(
                "invalid table-grant shape (expected a privilege array or object): {other}"
            ))),
        }
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct AuthState {
    /// username → superuser?
    users: BTreeMap<String, bool>,
    roles: BTreeSet<String>,
    /// grantee (user or role) → roles it is a member of.
    memberships: BTreeMap<String, BTreeSet<String>>,
    /// grantee → table → privilege → column scope (item 112). Was `grantee →
    /// table → {priv}` pre-112; [`TablePrivs`]'s custom `Deserialize` reads
    /// that legacy shape as every privilege scoped `GrantScope::All` — see
    /// its doc comment.
    table_grants: BTreeMap<String, BTreeMap<String, TablePrivs>>,
    /// username → argon2id PHC hash string (item 121, A1). Never the
    /// plaintext. `#[serde(default)]` so a pre-A1 `roles.json` (no
    /// `credentials` key) deserializes with an empty map — no
    /// FORMAT_VERSION bump. Persisted (it must be, to survive a restart);
    /// deliberately kept out of `Debug` (see the manual impl below) so it
    /// never leaks into `tracing::debug!`/`{:?}` logging.
    #[serde(default)]
    credentials: BTreeMap<String, String>,
    /// token_hash (SHA-256 hex of the raw refresh token) → session record
    /// (item 121, A4). Never the raw token itself — see [`sha256_hex`] /
    /// [`generate_refresh_token`]. `#[serde(default)]` so a pre-A4
    /// `roles.json` (no `sessions` key) deserializes with an empty map — no
    /// FORMAT_VERSION bump. Persisted (sessions must survive a restart);
    /// kept out of `Debug` (see the manual impl below), same posture as
    /// `credentials` — the hash is one-way, but the username/timestamps in
    /// each record are still control-plane detail that shouldn't leak into
    /// `tracing::debug!`/`{:?}` logging.
    #[serde(default)]
    sessions: BTreeMap<String, SessionRec>,
    /// username → TOTP MFA enrollment (item 127, Workstream D4). Never the
    /// secret/recovery-code plaintext. `#[serde(default)]` so a pre-127
    /// `roles.json` (no `mfa` key) deserializes with an empty map — no
    /// FORMAT_VERSION bump. Kept out of `Debug` (see the manual impl below),
    /// same posture as `credentials`/`sessions`.
    #[serde(default)]
    mfa: BTreeMap<String, MfaRecord>,
    /// challenge_hash (SHA-256 hex of the raw MFA challenge token) → pending
    /// login-challenge record (item 127). Never the raw challenge token
    /// itself. `#[serde(default)]` so a pre-127 `roles.json` deserializes
    /// with an empty map — no FORMAT_VERSION bump. Kept out of `Debug` (see
    /// the manual impl below), same posture as `sessions`.
    #[serde(default)]
    mfa_challenges: BTreeMap<String, MfaChallengeRec>,
    /// `(provider, provider_user_id) -> unidb username` (item 128, Workstream
    /// D1: OAuth social login). Outer key is the provider name
    /// ("google"/"github"); inner key is that provider's own opaque user id
    /// (never a unidb row id). `#[serde(default)]` so a pre-128 `roles.json`
    /// (no `oauth_identities` key) deserializes with an empty map — no
    /// FORMAT_VERSION bump. No secret material lives here (just ids), so
    /// unlike `credentials`/`sessions`/`mfa`/`mfa_challenges` this map is
    /// **not** redacted from `Debug` below — see the item-128 backlog doc's
    /// "redact nothing secret here" note.
    #[serde(default)]
    oauth_identities: BTreeMap<String, BTreeMap<String, String>>,
    /// state_hash (SHA-256 hex of the raw OAuth CSRF `state` token) -> pending
    /// authorize-flow record (item 128). Never the raw state token itself —
    /// same posture as `sessions`/`mfa_challenges`. `#[serde(default)]` so a
    /// pre-128 `roles.json` deserializes with an empty map — no
    /// FORMAT_VERSION bump. Kept out of `Debug` (see the manual impl below):
    /// the record also carries the PKCE `code_verifier`, which must stay a
    /// server-only secret for the whole point of PKCE to hold.
    #[serde(default)]
    oauth_states: BTreeMap<String, OAuthStateRec>,
    /// name → encrypted blob (item 129, Workstream I3: secrets vault). The
    /// blob is `base64(nonce || ciphertext || tag)` from
    /// [`crate::vault::Vault::encrypt`] — **never** the plaintext secret.
    /// `#[serde(default)]` so a pre-129 `roles.json` (no `secrets` key)
    /// deserializes with an empty map — no FORMAT_VERSION bump. Kept out of
    /// `Debug` (see the manual impl below), same posture as `credentials`/
    /// `sessions`/`mfa` — even though the value is already ciphertext (not
    /// plaintext), it is still control-plane detail that shouldn't leak via
    /// `{:?}`/`tracing::debug!`, and redacting it uniformly means a future
    /// reader never has to reason about "is this field safe to print" on a
    /// case-by-case basis.
    #[serde(default)]
    secrets: BTreeMap<String, String>,
    /// token_hash (SHA-256 hex of the raw recovery token) → pending
    /// password-recovery record (item 138 — `POST /auth/recover`/`POST
    /// /auth/verify`). Never the raw token itself — same posture as
    /// `sessions`/`oauth_states`. `#[serde(default)]` so a pre-138
    /// `roles.json` deserializes with an empty map — no FORMAT_VERSION bump.
    /// Kept out of `Debug` (see the manual impl below).
    #[serde(default)]
    recovery_tokens: BTreeMap<String, EmailTokenRec>,
    /// token_hash (SHA-256 hex of the raw magic-link token) → pending
    /// magic-link login record (item 138 — `POST /auth/magiclink`/`POST
    /// /auth/magiclink/verify`). Independent token space from
    /// `recovery_tokens` — see [`EmailTokenRec`]'s doc comment.
    /// `#[serde(default)]`, kept out of `Debug`, same posture as
    /// `recovery_tokens`.
    #[serde(default)]
    magiclink_tokens: BTreeMap<String, EmailTokenRec>,
    /// Realtime channel-authorization policies (item 140): one entry per
    /// `(topic_pattern, operation)` pair. No secret material — shown in full
    /// in `Debug` below, same posture as `oauth_identities`/`table_grants`.
    /// `#[serde(default)]` so a pre-140 `roles.json` (no `channel_policies`
    /// key) deserializes with an empty list — no FORMAT_VERSION bump.
    #[serde(default)]
    channel_policies: Vec<ChannelPolicyDef>,
    /// Database webhook registrations (item 141): one entry per `id`. May
    /// carry a plaintext fallback signing secret (see [`WebhookDef::
    /// signing_secret`]) — kept out of the default `Debug` derive path via
    /// [`WebhookSecret`]'s own redacting `Debug` impl, not by hiding this
    /// field entirely (unlike `credentials`/`sessions` below), so `GET
    /// /webhooks`/`list_webhooks` callers still see every other field.
    /// `#[serde(default)]` so a pre-141 `roles.json` (no `webhooks` key)
    /// deserializes with an empty list — no FORMAT_VERSION bump.
    #[serde(default)]
    webhooks: Vec<WebhookDef>,
    /// Per-user ban flag + app/user metadata (item 142). `#[serde(default)]`
    /// so a pre-142 `roles.json` (no `user_extra` key) deserializes with an
    /// empty map — no FORMAT_VERSION bump. Shown in full in `Debug` below —
    /// no secret material (same posture as `oauth_identities`/
    /// `table_grants`/`webhooks`).
    #[serde(default)]
    user_extra: BTreeMap<String, UserExtra>,
    /// Scheduled-job (cron) registrations (item 144): one entry per `name`.
    /// `sql` is admin-authored (superuser-gated registration), the same
    /// "not user input" posture the item-141 backlog doc documents for a
    /// webhook's `target_url` — shown in full below, no secret material.
    /// `#[serde(default)]` so a pre-144 `roles.json` (no `cron_jobs` key)
    /// deserializes with an empty list — no FORMAT_VERSION bump.
    #[serde(default)]
    cron_jobs: Vec<CronJobDef>,
}

/// Manual `Debug`: every field except `credentials`/`sessions`/`mfa`/
/// `mfa_challenges`, which are each redacted to a count. Prevents an
/// argon2id hash (a secret the CLAUDE.md rules for this milestone say must
/// never appear in logs) — and, as of item 121 A4, refresh-token session
/// detail, and as of item 127, TOTP secrets/recovery-code hashes — from
/// leaking via `{:?}` if `AuthState`/`RoleStore` internals are ever
/// debug-printed.
impl std::fmt::Debug for AuthState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthState")
            .field("users", &self.users)
            .field("roles", &self.roles)
            .field("memberships", &self.memberships)
            .field("table_grants", &self.table_grants)
            .field(
                "credentials",
                &format!("<{} redacted>", self.credentials.len()),
            )
            .field("sessions", &format!("<{} redacted>", self.sessions.len()))
            .field("mfa", &format!("<{} redacted>", self.mfa.len()))
            .field(
                "mfa_challenges",
                &format!("<{} redacted>", self.mfa_challenges.len()),
            )
            // item 128: identity links carry no secret (just provider/user
            // ids and the unidb username they resolve to) — shown in full,
            // same posture as `users`/`roles`/`table_grants` above.
            .field("oauth_identities", &self.oauth_identities)
            .field(
                "oauth_states",
                &format!("<{} redacted>", self.oauth_states.len()),
            )
            // item 129: never show even the ciphertext blob — a count only,
            // same posture as `credentials`/`sessions`/`mfa` above.
            .field("secrets", &format!("<{} redacted>", self.secrets.len()))
            // item 138: pending recovery/magic-link tokens — counts only,
            // same posture as `sessions`/`oauth_states` above.
            .field(
                "recovery_tokens",
                &format!("<{} redacted>", self.recovery_tokens.len()),
            )
            .field(
                "magiclink_tokens",
                &format!("<{} redacted>", self.magiclink_tokens.len()),
            )
            // item 140: channel policies carry no secret (topic patterns +
            // role names only) — shown in full, same posture as
            // `oauth_identities`/`table_grants` above.
            .field("channel_policies", &self.channel_policies)
            // item 141: shown in full — each `WebhookDef`'s own `Debug`
            // redacts its `signing_secret` field via `WebhookSecret`'s
            // manual impl, so this can safely reuse the derived struct
            // debug instead of collapsing to a bare count.
            .field("webhooks", &self.webhooks)
            // item 142: ban flag + metadata — no secret material, shown in
            // full, same posture as `webhooks`/`table_grants` above.
            .field("user_extra", &self.user_extra)
            // item 144: scheduled-job registrations — admin-authored SQL,
            // no secret material, shown in full, same posture as
            // `webhooks`/`channel_policies` above.
            .field("cron_jobs", &self.cron_jobs)
            .finish()
    }
}

/// Operation scope for a named RLS policy (item-24 Z1).
/// `All` expands to "applies to every DML operation".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PolicyOp {
    Select,
    Insert,
    Update,
    Delete,
    All,
}

impl PolicyOp {
    fn parse(s: &str) -> Option<PolicyOp> {
        match s.to_ascii_uppercase().as_str() {
            "SELECT" => Some(PolicyOp::Select),
            "INSERT" => Some(PolicyOp::Insert),
            "UPDATE" => Some(PolicyOp::Update),
            "DELETE" => Some(PolicyOp::Delete),
            "ALL" => Some(PolicyOp::All),
            _ => None,
        }
    }

    /// String representation for catalog exposure (Z5).
    pub fn as_str(self) -> &'static str {
        match self {
            PolicyOp::Select => "SELECT",
            PolicyOp::Insert => "INSERT",
            PolicyOp::Update => "UPDATE",
            PolicyOp::Delete => "DELETE",
            PolicyOp::All => "ALL",
        }
    }
}

/// A named RLS policy stored in the catalog `TableDef.policies` (item-24 Z1).
/// Mirrors Postgres `pg_policy`: a name, an operation scope, a USING predicate
/// (row-filter — which rows may be seen/affected), and an optional WITH CHECK
/// predicate (write-side — new row must satisfy this; defaults to `using_expr`
/// when absent, per Postgres semantics).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PolicyDef {
    pub name: String,
    pub table: String,
    pub op: PolicyOp,
    /// Raw SQL predicate (the `USING (…)` clause), stored verbatim for
    /// catalog exposure.  Re-parsed into `Expr` at apply time.
    pub using_expr: String,
    /// Optional `WITH CHECK (…)` predicate (item-24 R-a).  When `None` the
    /// USING expression doubles as the write-side check, per Postgres
    /// semantics.  `#[serde(default)]` ensures pre-R-a catalog blobs
    /// deserialize with `None` — no FORMAT_VERSION bump required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub with_check_sql: Option<String>,
    /// Target-role list for `CREATE POLICY … TO <role,...>` (item 122, B4).
    /// Empty means "no TO clause" — the policy applies to every caller,
    /// exactly like every pre-B4 policy (back-compat contract). Matched
    /// against the caller's *effective* roles (see [`RoleStore::
    /// effective_roles`]) by `apply_rls_with_auth`'s role gate in
    /// `sql/logical.rs`: a policy is only AND-injected for a caller whose
    /// effective roles intersect this set (or when this set is empty).
    /// `#[serde(default)]` so a pre-B4 catalog blob (this field absent)
    /// deserializes as an empty list — identical behavior, no
    /// FORMAT_VERSION bump.
    #[serde(default)]
    pub target_roles: Vec<String>,
}

/// Operation scope for a realtime channel-authorization policy (item 140).
/// `All` matches every one of the other three when checking whether a stored
/// policy governs a caller's requested operation — see [`ChannelOp::governs`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChannelOp {
    Publish,
    Subscribe,
    /// Covers both `GET /realtime/presence/subscribe` and `POST /realtime/
    /// presence/track` — the two presence routes share one operation kind
    /// (item-140 backlog doc's enforcement-points table), distinct from
    /// broadcast's `Publish`/`Subscribe`.
    Presence,
    All,
}

impl ChannelOp {
    pub fn parse(s: &str) -> Option<ChannelOp> {
        match s.to_ascii_lowercase().as_str() {
            "publish" => Some(ChannelOp::Publish),
            "subscribe" => Some(ChannelOp::Subscribe),
            "presence" => Some(ChannelOp::Presence),
            "all" => Some(ChannelOp::All),
            _ => None,
        }
    }

    /// String representation for the admin surface (`GET /realtime/policies`).
    pub fn as_str(self) -> &'static str {
        match self {
            ChannelOp::Publish => "publish",
            ChannelOp::Subscribe => "subscribe",
            ChannelOp::Presence => "presence",
            ChannelOp::All => "all",
        }
    }

    /// Whether a policy scoped to `self` governs a caller's `requested`
    /// operation — exact match, or `self == All` (a catch-all policy governs
    /// every operation on a topic it matches).
    fn governs(self, requested: ChannelOp) -> bool {
        self == ChannelOp::All || self == requested
    }
}

/// A realtime channel-authorization policy (item 140, backlog doc's locked
/// v1 design): `(topic_pattern, operation) -> allowed_roles`. `topic_pattern`
/// is an exact topic or a `*`-suffix glob (`"room:*"`); `roles` reuses the
/// existing role system (including the built-in `anon`/`authenticated`/
/// `service_role` — see [`RoleStore::effective_roles`]). Persisted in
/// `roles.json` alongside every other control-plane object (no separate
/// store, no FORMAT_VERSION bump — see [`AuthState::channel_policies`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChannelPolicyDef {
    pub topic_pattern: String,
    pub operation: ChannelOp,
    pub roles: BTreeSet<String>,
}

/// The three CDC operations a database webhook (item 141) can subscribe to.
/// Deliberately no `All` variant (unlike [`ChannelOp`]) — the backlog spec's
/// locked v1 design has `events` as an explicit list the caller enumerates
/// (`["insert","update","delete"]` for "all"), not a catch-all sentinel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum WebhookEvent {
    Insert,
    Update,
    Delete,
}

impl WebhookEvent {
    pub fn parse(s: &str) -> Option<WebhookEvent> {
        match s.to_ascii_lowercase().as_str() {
            "insert" => Some(WebhookEvent::Insert),
            "update" => Some(WebhookEvent::Update),
            "delete" => Some(WebhookEvent::Delete),
            _ => None,
        }
    }

    /// String representation for the admin surface (`GET /webhooks`) and for
    /// matching against a captured [`crate::queue::Event::op`] string, which
    /// already speaks this exact vocabulary (see `event_format.rs`).
    pub fn as_str(self) -> &'static str {
        match self {
            WebhookEvent::Insert => "insert",
            WebhookEvent::Update => "update",
            WebhookEvent::Delete => "delete",
        }
    }
}

/// A webhook signing secret (item 141). Never `Debug`-printed — mirrors
/// [`crate::server::oauth::ClientSecret`]'s exact posture: a small accessor
/// wrapper around the raw string so a `{:?}` of a [`WebhookDef`] (or a
/// `tracing::*!(?def, ..)` call) can never leak it, while `Serialize`/
/// `Deserialize` still round-trip it through `roles.json` like every other
/// control-plane field (the file itself is operator-trusted local storage,
/// same posture as `credentials`/`sessions` — see `AuthState`'s doc comment).
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct WebhookSecret(String);

impl WebhookSecret {
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    /// The raw secret. Callers use this only to compute an HMAC signature
    /// (the delivery worker) — never to log it or return it in an HTTP
    /// response.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for WebhookSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<redacted>")
    }
}

/// A database webhook registration (item 141, backlog doc's locked v1
/// design): `(id, target_url, table_pattern, events, signing_secret?,
/// enabled, headers?)`. `table_pattern` is an exact table name or `"*"` for
/// every table — deliberately simpler than [`ChannelPolicyDef`]'s
/// `*`-suffix glob (the backlog spec states "exact or `*`", nothing
/// in-between). Persisted in `roles.json` alongside every other
/// control-plane object (no separate store, no FORMAT_VERSION bump — see
/// [`AuthState::webhooks`]). `#[derive(Debug)]` is safe here because
/// [`WebhookSecret`]'s own `Debug` redacts the one sensitive field.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WebhookDef {
    pub id: String,
    pub target_url: String,
    pub table_pattern: String,
    pub events: BTreeSet<WebhookEvent>,
    /// The registration-time fallback secret (item 141's "vault-first, then
    /// the registration body" posture — mirrors [`crate::server::oauth::
    /// resolve_client_secret`]'s vault-then-env order exactly, with this
    /// field standing in for OAuth's env value since a webhook has no static
    /// env var to read). `None` when the webhook was registered with no
    /// `signing_secret` and no `webhook.<id>.secret` vault entry exists
    /// either — such a webhook's deliveries carry no `X-Unidb-Signature`
    /// header. `#[serde(default)]` so a pre-141 `roles.json` (impossible
    /// today, future-proofing) would still deserialize.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signing_secret: Option<WebhookSecret>,
    #[serde(default = "default_webhook_enabled")]
    pub enabled: bool,
    /// Extra static headers sent with every delivery (e.g. a caller-side API
    /// key the receiver expects). Never used for authentication of unidb
    /// itself — only `X-Unidb-Signature` is unidb's own trust signal.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

fn default_webhook_enabled() -> bool {
    true
}

/// Whether `table` matches `pattern` for webhook delivery (item 141): exact
/// equality, or `pattern == "*"` (every table). Free function (not a
/// `WebhookDef` method) so the delivery worker in `src/server/webhooks.rs`
/// can call it without needing a whole `WebhookDef` in scope for a unit test.
pub fn webhook_table_matches(pattern: &str, table: &str) -> bool {
    pattern == "*" || pattern == table
}

/// A scheduled-job (cron) registration (item 144, backlog doc's locked v1
/// design): `(name, schedule, sql, enabled, run_as?)`. Persisted in
/// `roles.json` alongside every other control-plane object (no separate
/// store, no FORMAT_VERSION bump — see [`AuthState::cron_jobs`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CronJobDef {
    /// Unique job name — the upsert key (`POST /cron/jobs` replaces any
    /// existing job of the same name, mirroring [`WebhookDef::id`]).
    pub name: String,
    /// A standard 5-field cron expression (`minute hour day-of-month month
    /// day-of-week`), validated at registration time by
    /// [`crate::cron::CronSchedule::parse`] — see that type's doc comment
    /// for the exact grammar. Evaluated against the server's **local**
    /// time (documented in `docs/REST_API.md`), minute granularity, no
    /// missed-tick backfill.
    pub schedule: String,
    /// The statement(s) run under [`Self::run_as`] on every matching tick,
    /// via the exact same `execute_sql` path any other caller uses — see
    /// `server::cron`'s module doc. Admin-authored (superuser-gated
    /// registration), not end-user input, same posture
    /// [`WebhookDef::target_url`]'s doc comment documents for a webhook's
    /// URL.
    pub sql: String,
    #[serde(default = "default_cron_enabled")]
    pub enabled: bool,
    /// The principal the job's `sql` runs as. `None` (the default) is the
    /// embedded/superuser identity — `execute_sql_as`'s existing "no
    /// subject" meaning — so a job with no `run_as` runs unrestricted,
    /// bypassing RLS like any other superuser call. `Some(name)` runs the
    /// job exactly as [`crate::Engine::execute_sql_as`]`(Some(name), ..)`
    /// would: that user/role's own table grants and RLS policies apply,
    /// letting a job be scoped down rather than always running as an
    /// implicit superuser.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_as: Option<String>,
}

fn default_cron_enabled() -> bool {
    true
}

/// A parsed auth-DDL statement.
#[derive(PartialEq)]
pub enum AuthStmt {
    CreateUser {
        name: String,
        superuser: bool,
        /// Plaintext from the optional `PASSWORD '<pw>'` clause (item 121,
        /// A1) — hashed with argon2id before it ever reaches `AuthState`.
        /// Never stored or logged as-is; kept out of `Debug` (see the manual
        /// impl below) so a `{:?}` of a pending statement can't leak it
        /// either.
        password: Option<String>,
    },
    DropUser(String),
    CreateRole(String),
    DropRole(String),
    GrantPrivs {
        privs: Vec<Privilege>,
        table: String,
        grantee: String,
        /// `GRANT <priv,..> (col, ...) ON t TO r` (item 112): `None` is the
        /// pre-112 whole-table grammar (implies every column, unchanged
        /// behavior); `Some(cols)` narrows every privilege in `privs` to
        /// exactly these columns. A repeated `GRANT` only ever widens (never
        /// narrows) an existing grant — see `RoleStore::apply`.
        columns: Option<Vec<String>>,
    },
    RevokePrivs {
        privs: Vec<Privilege>,
        table: String,
        grantee: String,
        /// `REVOKE <priv,..> (col, ...) ON t FROM r` (item 112): `None` is a
        /// table-level revoke — clears the privilege entirely (pre-112
        /// behavior, unchanged). `Some(cols)` narrows: removes exactly these
        /// columns from the grantee's existing column-scoped grant (or from
        /// an existing whole-table grant, via `Engine::exec_auth_stmt`
        /// materializing it to an explicit column set first — see
        /// `RoleStore::materialize_all_to_columns`).
        columns: Option<Vec<String>>,
    },
    GrantRole {
        role: String,
        grantee: String,
    },
    RevokeRole {
        role: String,
        grantee: String,
    },
    /// `CREATE POLICY <name> ON <table> FOR <op> USING (<predicate>) [WITH CHECK (<expr>)]` (Z1/R-a).
    CreatePolicy(PolicyDef),
    /// `DROP POLICY <name> ON <table>` (Z1).
    DropPolicy {
        name: String,
        table: String,
    },
    /// `ALTER USER <name> PASSWORD '<pw>'` (Studio password reset). Superuser
    /// gated exactly like `CREATE USER ... PASSWORD` (auth DDL already
    /// requires superuser — see `Engine::execute_sql_as_principal`). Errors
    /// if `name` doesn't exist. Sets the same argon2id credential as
    /// `RoleStore::set_password`, via the same `hash_password` helper.
    AlterUserPassword {
        name: String,
        /// Plaintext from the `PASSWORD '<pw>'` clause — hashed with
        /// argon2id before it ever reaches `AuthState`. Never stored or
        /// logged as-is; kept out of `Debug` (see the manual impl below).
        password: String,
    },
}

/// Manual `Debug`: redacts `CreateUser`'s `password` field to `Some("<redacted>")`/
/// `None` instead of the plaintext, so `tracing::info!(?stmt, ..)` in
/// [`RoleStore::apply`] (and any other `{:?}`/audit-log use of a pending
/// statement) can never print a password.
impl std::fmt::Debug for AuthStmt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthStmt::CreateUser {
                name,
                superuser,
                password,
            } => f
                .debug_struct("CreateUser")
                .field("name", name)
                .field("superuser", superuser)
                .field("password", &password.as_ref().map(|_| "<redacted>"))
                .finish(),
            AuthStmt::DropUser(name) => f.debug_tuple("DropUser").field(name).finish(),
            AuthStmt::CreateRole(name) => f.debug_tuple("CreateRole").field(name).finish(),
            AuthStmt::DropRole(name) => f.debug_tuple("DropRole").field(name).finish(),
            AuthStmt::GrantPrivs {
                privs,
                table,
                grantee,
                columns,
            } => f
                .debug_struct("GrantPrivs")
                .field("privs", privs)
                .field("table", table)
                .field("grantee", grantee)
                .field("columns", columns)
                .finish(),
            AuthStmt::RevokePrivs {
                privs,
                table,
                grantee,
                columns,
            } => f
                .debug_struct("RevokePrivs")
                .field("privs", privs)
                .field("table", table)
                .field("grantee", grantee)
                .field("columns", columns)
                .finish(),
            AuthStmt::GrantRole { role, grantee } => f
                .debug_struct("GrantRole")
                .field("role", role)
                .field("grantee", grantee)
                .finish(),
            AuthStmt::RevokeRole { role, grantee } => f
                .debug_struct("RevokeRole")
                .field("role", role)
                .field("grantee", grantee)
                .finish(),
            AuthStmt::CreatePolicy(policy) => f.debug_tuple("CreatePolicy").field(policy).finish(),
            AuthStmt::DropPolicy { name, table } => f
                .debug_struct("DropPolicy")
                .field("name", name)
                .field("table", table)
                .finish(),
            AuthStmt::AlterUserPassword { name, .. } => f
                .debug_struct("AlterUserPassword")
                .field("name", name)
                .field("password", &"<redacted>")
                .finish(),
        }
    }
}

/// Persisted authorization store.
pub struct RoleStore {
    path: PathBuf,
    inner: Mutex<AuthState>,
}

impl RoleStore {
    pub fn open(dir: &Path) -> Result<Self> {
        let path = dir.join("roles.json");
        let mut inner: AuthState = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                tracing::warn!(error = %e, "roles.json unreadable — starting with no roles");
                AuthState::default()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => AuthState::default(),
            Err(e) => return Err(e.into()),
        };
        // item 4: `session_id` is new (`#[serde(default)]` = empty string for
        // a pre-item-4 `roles.json`). Backfill any empty id here so every
        // in-memory session has a stable, independently-random opaque id
        // from the moment the store opens — persisted once below so the
        // backfilled ids survive a restart too, rather than being
        // regenerated (and thus churning) on every open.
        let mut backfilled = false;
        for rec in inner.sessions.values_mut() {
            if rec.session_id.is_empty() {
                rec.session_id = generate_session_id();
                backfilled = true;
            }
        }
        let store = Self {
            path,
            inner: Mutex::new(inner),
        };
        if backfilled {
            let st = store.lock();
            store.persist(&st)?;
        }
        Ok(store)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, AuthState> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn persist(&self, st: &AuthState) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(st)
            .map_err(|e| DbError::Authz(format!("serialize roles: {e}")))?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// Whether `user` is a superuser (a named user created `SUPERUSER`).
    pub fn is_superuser(&self, user: &str) -> bool {
        self.lock().users.get(user).copied().unwrap_or(false)
    }

    /// Whether any user is registered. When empty, the engine runs in **open /
    /// bootstrap mode** (everyone is an effective superuser) — this preserves
    /// the pre-P6.e "any valid token grants full access" behavior until an
    /// operator creates the first user. Create a `SUPERUSER` first to bootstrap.
    pub fn has_users(&self) -> bool {
        !self.lock().users.is_empty()
    }

    /// Set (or replace) `user`'s password credential, hashed with argon2id
    /// (item 121, A1). Errors if `user` doesn't exist. This is the Rust-API
    /// path (`Engine::set_password`); `CREATE USER ... PASSWORD '<pw>'`
    /// hashes inline in [`Self::apply`] instead.
    pub fn set_password(&self, user: &str, password: &str) -> Result<()> {
        let hash = hash_password(password)?;
        let mut st = self.lock();
        if !st.users.contains_key(user) {
            return Err(DbError::Authz(format!("user '{user}' not found")));
        }
        st.credentials.insert(user.to_string(), hash);
        self.persist(&st)?;
        tracing::info!(user = %user, "password credential set");
        Ok(())
    }

    /// Verify `password` against `user`'s stored credential (item 121, A2).
    ///
    /// Returns `false` uniformly for every failure case — unknown user, user
    /// with no stored credential, or a genuine mismatch — and always runs a
    /// real argon2id verify (against [`dummy_hash`] in the first two cases)
    /// so the cost and code path are identical regardless of *why* it fails.
    /// This is the login authentication check: no user-enumeration
    /// timing/shape oracle.
    pub fn verify_password(&self, user: &str, password: &str) -> bool {
        let hash = self.lock().credentials.get(user).cloned();
        match hash {
            Some(h) => verify_password_hash(&h, password),
            None => {
                // Constant-shape dummy verify: burns the same argon2id cost,
                // discards the result, and always reports failure.
                let _ = verify_password_hash(dummy_hash(), password);
                false
            }
        }
    }

    /// Issue a fresh refresh-token session for `user` (item 121, A4): a new
    /// high-entropy opaque token ([`generate_refresh_token`]) is generated,
    /// its SHA-256 hash is persisted as a [`SessionRec`] (never the raw
    /// token), and the raw token + its absolute expiry (Unix seconds) are
    /// returned for the caller to hand to the client exactly once.
    pub fn create_session(&self, user: &str) -> Result<(String, u64)> {
        let raw = generate_refresh_token();
        let hash = sha256_hex(&raw);
        let now = now_secs();
        let expires_at = now + REFRESH_TOKEN_TTL_SECS;
        let mut st = self.lock();
        st.sessions.insert(
            hash,
            SessionRec {
                session_id: generate_session_id(),
                username: user.to_string(),
                issued_at: now,
                expires_at,
                revoked: false,
            },
        );
        self.persist(&st)?;
        Ok((raw, expires_at))
    }

    /// Verify a raw refresh token (item 121, A4): hashes it and looks up the
    /// session record. Returns the session's username only when the record
    /// exists, is not revoked, and has not expired — every other case
    /// (unknown hash, expired, revoked) returns `None` uniformly, so a
    /// caller building a 401 response can't distinguish *why* verification
    /// failed. The raw token is never compared directly (only its hash is
    /// looked up), which is what forecloses a timing oracle on the secret
    /// itself — see [`sha256_hex`]'s doc comment.
    pub fn verify_session(&self, raw_token: &str) -> Option<String> {
        let hash = sha256_hex(raw_token);
        let st = self.lock();
        st.sessions.get(&hash).and_then(|rec| {
            if !rec.revoked && rec.expires_at > now_secs() {
                Some(rec.username.clone())
            } else {
                None
            }
        })
    }

    /// Verify + rotate a refresh token in one call (item 121, A4): if
    /// `raw_token` is a valid (unexpired, unrevoked, known) session, revoke
    /// it and issue a brand-new one for the same user, persisting both
    /// changes together. Returns `Ok(None)` — never an error — for the
    /// unknown/expired/revoked cases, so `POST /auth/refresh` can map all
    /// three to the identical uniform 401 exactly like [`Self::
    /// verify_session`]. `Ok(Some((username, new_raw_token, expires_at)))`
    /// on success.
    pub fn rotate_session(&self, raw_token: &str) -> Result<Option<(String, String, u64)>> {
        let old_hash = sha256_hex(raw_token);
        let mut st = self.lock();
        let username = match st.sessions.get(&old_hash) {
            Some(rec) if !rec.revoked && rec.expires_at > now_secs() => rec.username.clone(),
            _ => return Ok(None),
        };
        if let Some(rec) = st.sessions.get_mut(&old_hash) {
            rec.revoked = true;
        }
        let new_raw = generate_refresh_token();
        let new_hash = sha256_hex(&new_raw);
        let now = now_secs();
        let expires_at = now + REFRESH_TOKEN_TTL_SECS;
        st.sessions.insert(
            new_hash,
            SessionRec {
                session_id: generate_session_id(),
                username: username.clone(),
                issued_at: now,
                expires_at,
                revoked: false,
            },
        );
        self.persist(&st)?;
        Ok(Some((username, new_raw, expires_at)))
    }

    /// Revoke a refresh-token session (item 121, A4: `POST /auth/logout`).
    /// Idempotent by design: an unknown hash (already revoked, already
    /// expired and pruned, or simply never issued) is a silent no-op, not an
    /// error — logging out twice, or logging out with a stale/garbage token,
    /// must never surface a distinguishable error to the caller. Returns
    /// `Err` only for a genuine persistence (disk I/O) failure.
    pub fn revoke_session(&self, raw_token: &str) -> Result<()> {
        let hash = sha256_hex(raw_token);
        let mut st = self.lock();
        if let Some(rec) = st.sessions.get_mut(&hash) {
            if !rec.revoked {
                rec.revoked = true;
                self.persist(&st)?;
            }
        }
        Ok(())
    }

    /// Every session, stripped down to non-secret fields (item 4 — Studio
    /// session listing). Never the raw token or its SHA-256 hash — see
    /// [`SessionView`]. Per-caller filtering (superuser sees all, a named
    /// user sees only their own) is applied by the call site
    /// (`information_schema::sessions_rows`), mirroring item 111 — this
    /// method itself is identity-agnostic, matching `roles`/`grants`/`users`.
    pub fn list_sessions(&self) -> Vec<SessionView> {
        let st = self.lock();
        st.sessions
            .values()
            .map(|rec| SessionView {
                session_id: rec.session_id.clone(),
                username: rec.username.clone(),
                issued_at: rec.issued_at,
                expires_at: rec.expires_at,
                revoked: rec.revoked,
            })
            .collect()
    }

    /// Username owning `session_id`, if any session (revoked or not, expired
    /// or not) currently has that id. Used by the self/superuser gate on the
    /// revoke-by-id route before calling [`Self::revoke_session_by_id`], so
    /// a non-superuser caller can be confirmed to own the session before
    /// their revoke request is allowed to take effect.
    pub fn session_owner(&self, session_id: &str) -> Option<String> {
        let st = self.lock();
        st.sessions
            .values()
            .find(|rec| rec.session_id == session_id)
            .map(|rec| rec.username.clone())
    }

    /// Revoke a session by its opaque id (item 4), rather than by raw token —
    /// backs `DELETE /auth/sessions/{id}`. Idempotent, same posture as
    /// [`Self::revoke_session`]: an unknown id (already revoked, or never
    /// issued) is a silent `Ok(())`, not an error. Callers that need an
    /// ownership check (a non-superuser may only revoke their own sessions)
    /// must call [`Self::session_owner`] first — this method itself performs
    /// no identity check, matching the identity-agnostic posture of
    /// [`Self::list_sessions`].
    pub fn revoke_session_by_id(&self, session_id: &str) -> Result<()> {
        let mut st = self.lock();
        if let Some(rec) = st
            .sessions
            .values_mut()
            .find(|rec| rec.session_id == session_id)
        {
            if !rec.revoked {
                rec.revoked = true;
                self.persist(&st)?;
            }
        }
        Ok(())
    }

    // ── item 127 (Workstream D4): TOTP MFA ──────────────────────────────

    /// `POST /auth/mfa/enroll`'s embedded-API entry point: generate a fresh
    /// per-user TOTP secret, store it as **pending** (MFA is not enabled
    /// yet — see [`Self::mfa_confirm`]), and return `(base32_secret,
    /// otpauth_uri)` for the caller to render as a QR code / hand to an
    /// authenticator app. Errors if `user` doesn't exist, or if MFA is
    /// already enabled (re-enrolling over a live enrollment must go through
    /// `POST /auth/mfa/disable` first — this prevents silently invalidating
    /// a user's already-working authenticator without their confirmation).
    /// Re-calling while a *pending* (unconfirmed) enrollment exists is
    /// allowed and simply replaces it with a fresh secret — a user who lost
    /// the QR code before confirming can just re-enroll.
    pub fn mfa_enroll(&self, user: &str) -> Result<(String, String)> {
        let mut st = self.lock();
        if !st.users.contains_key(user) {
            return Err(DbError::Authz(format!("user '{user}' not found")));
        }
        if st.mfa.get(user).map(|r| r.enabled).unwrap_or(false) {
            return Err(DbError::Authz(
                "MFA is already enabled for this user — disable it before re-enrolling".into(),
            ));
        }
        let secret_base32 = base32_encode(&generate_totp_secret());
        st.mfa.insert(
            user.to_string(),
            MfaRecord {
                secret_base32: secret_base32.clone(),
                enabled: false,
                recovery_code_hashes: Vec::new(),
                last_used_step: None,
            },
        );
        self.persist(&st)?;
        let otpauth_uri =
            format!("otpauth://totp/unidb:{user}?secret={secret_base32}&issuer=unidb");
        tracing::info!(user = %user, "MFA enrollment started (pending confirmation)");
        Ok((secret_base32, otpauth_uri))
    }

    /// `POST /auth/mfa/verify`'s embedded-API entry point: confirm a pending
    /// enrollment ([`Self::mfa_enroll`]) with a live 6-digit TOTP `code`.
    /// On success, MFA flips to **enabled**, a fresh batch of one-time
    /// recovery codes is minted (only their SHA-256 hashes are persisted —
    /// see [`RECOVERY_CODE_COUNT`]), and the plaintext codes are returned
    /// for the caller to show exactly once. `Ok(None)` uniformly covers a
    /// wrong/malformed/replayed code (no distinguishing oracle); `Err` only
    /// for a real precondition failure — no pending enrollment, or MFA
    /// already enabled.
    pub fn mfa_confirm(&self, user: &str, code: &str) -> Result<Option<Vec<String>>> {
        let mut st = self.lock();
        let Some(rec) = st.mfa.get(user).cloned() else {
            return Err(DbError::Authz(
                "no pending MFA enrollment for this user — call POST /auth/mfa/enroll first".into(),
            ));
        };
        if rec.enabled {
            return Err(DbError::Authz(
                "MFA is already enabled for this user".into(),
            ));
        }
        let Some(secret_bytes) = base32_decode(&rec.secret_base32) else {
            return Err(DbError::Authz("corrupt MFA secret".into()));
        };
        let Some(step) = verify_totp_code(&secret_bytes, code, rec.last_used_step) else {
            return Ok(None);
        };
        let mut plaintext_codes = Vec::with_capacity(RECOVERY_CODE_COUNT);
        let mut hashes = Vec::with_capacity(RECOVERY_CODE_COUNT);
        for _ in 0..RECOVERY_CODE_COUNT {
            let plain = generate_recovery_code();
            hashes.push(sha256_hex(&plain));
            plaintext_codes.push(plain);
        }
        st.mfa.insert(
            user.to_string(),
            MfaRecord {
                secret_base32: rec.secret_base32,
                enabled: true,
                recovery_code_hashes: hashes,
                last_used_step: Some(step),
            },
        );
        self.persist(&st)?;
        tracing::info!(user = %user, "MFA enabled");
        Ok(Some(plaintext_codes))
    }

    /// Whether `user` currently has MFA **enabled** (not merely pending) —
    /// used by the login gate (`POST /auth/login`) and `GET /auth/whoami`.
    /// `false` for an unknown user (same fail-closed posture as every other
    /// lookup on a name that isn't in the store).
    pub fn mfa_enabled(&self, user: &str) -> bool {
        self.lock()
            .mfa
            .get(user)
            .map(|r| r.enabled)
            .unwrap_or(false)
    }

    /// `POST /auth/mfa/disable`'s embedded-API entry point. `skip_code_check
    /// == true` is the superuser-bypass path (the caller/handler has already
    /// established the actor is a superuser); otherwise `code` must be
    /// `Some` and must verify (TOTP or a recovery code) or the call fails.
    /// Errors if MFA isn't currently enabled for `user`. Returns `Ok(true)`
    /// on success, `Ok(false)` for a present-but-wrong code (maps to a
    /// uniform 401 at the HTTP layer — no oracle on *why* the code was
    /// rejected).
    pub fn mfa_disable(
        &self,
        user: &str,
        code: Option<&str>,
        skip_code_check: bool,
    ) -> Result<bool> {
        let mut st = self.lock();
        let Some(rec) = st.mfa.get(user).cloned() else {
            return Err(DbError::Authz("MFA is not enabled for this user".into()));
        };
        if !rec.enabled {
            return Err(DbError::Authz("MFA is not enabled for this user".into()));
        }
        if !skip_code_check {
            let Some(code) = code else {
                return Ok(false);
            };
            let verified = match base32_decode(&rec.secret_base32) {
                Some(secret_bytes) => {
                    verify_totp_code(&secret_bytes, code, rec.last_used_step).is_some()
                }
                None => false,
            } || st
                .mfa
                .get(user)
                .map(|r| {
                    r.recovery_code_hashes
                        .iter()
                        .any(|h| constant_time_eq(h, &sha256_hex(code.trim())))
                })
                .unwrap_or(false);
            if !verified {
                return Ok(false);
            }
        }
        st.mfa.remove(user);
        self.persist(&st)?;
        tracing::info!(user = %user, superuser_bypass = skip_code_check, "MFA disabled");
        Ok(true)
    }

    /// `POST /auth/login`'s MFA gate: when [`Self::mfa_enabled`] is true for
    /// the (already password-verified) `user`, this issues a short-lived,
    /// single-use MFA challenge **instead of** a full session — see
    /// [`Self::verify_mfa_challenge`] for the redeem side. Returns
    /// `(raw_challenge_token, expires_at_unix_secs)`; only the token's
    /// SHA-256 hash is persisted, mirroring [`Self::create_session`].
    pub fn create_mfa_challenge(&self, user: &str) -> Result<(String, u64)> {
        let raw = generate_mfa_challenge_token();
        let hash = sha256_hex(&raw);
        let now = now_secs();
        let expires_at = now + MFA_CHALLENGE_TTL_SECS;
        let mut st = self.lock();
        st.mfa_challenges.insert(
            hash,
            MfaChallengeRec {
                username: user.to_string(),
                issued_at: now,
                expires_at,
                used: false,
            },
        );
        self.persist(&st)?;
        Ok((raw, expires_at))
    }

    /// `POST /auth/mfa/challenge`'s embedded-API entry point: redeem a raw
    /// MFA challenge token plus a 6-digit TOTP code (or a one-time recovery
    /// code) for the username that owns it. `Ok(Some(username))` on success
    /// — the caller (the HTTP handler) then mints a real session via the
    /// normal [`Self::create_session`] path, exactly like a non-MFA login.
    /// `Ok(None)` uniformly covers every failure case: unknown/garbage
    /// challenge, expired challenge, already-used (single-use) challenge,
    /// MFA no longer enabled for the owning user (fail-closed if it was
    /// disabled between issuing the challenge and redeeming it), or a
    /// wrong/replayed code — no oracle distinguishing *why* it failed. A
    /// successfully-verified TOTP step or consumed recovery code is
    /// persisted atomically with marking the challenge used, so a retried
    /// request after a successful-but-uncommitted attempt can never replay.
    pub fn verify_mfa_challenge(&self, raw_challenge: &str, code: &str) -> Result<Option<String>> {
        let hash = sha256_hex(raw_challenge);
        let mut st = self.lock();
        let Some(chal) = st.mfa_challenges.get(&hash).cloned() else {
            return Ok(None);
        };
        if chal.used || chal.expires_at <= now_secs() {
            return Ok(None);
        }
        let Some(rec) = st.mfa.get(&chal.username).cloned() else {
            return Ok(None);
        };
        if !rec.enabled {
            return Ok(None);
        }
        let totp_step = base32_decode(&rec.secret_base32)
            .and_then(|secret_bytes| verify_totp_code(&secret_bytes, code, rec.last_used_step));
        let verified = if let Some(step) = totp_step {
            if let Some(r) = st.mfa.get_mut(&chal.username) {
                r.last_used_step = Some(step);
            }
            true
        } else {
            let candidate_hash = sha256_hex(code.trim());
            match st.mfa.get_mut(&chal.username) {
                Some(r) => {
                    match r
                        .recovery_code_hashes
                        .iter()
                        .position(|h| constant_time_eq(h, &candidate_hash))
                    {
                        Some(pos) => {
                            r.recovery_code_hashes.remove(pos);
                            true
                        }
                        None => false,
                    }
                }
                None => false,
            }
        };
        if !verified {
            return Ok(None);
        }
        if let Some(c) = st.mfa_challenges.get_mut(&hash) {
            c.used = true;
        }
        self.persist(&st)?;
        Ok(Some(chal.username))
    }

    // ── item 128 (Workstream D1): OAuth 2.0 social login ────────────────

    /// `GET /auth/oauth/<provider>/authorize`'s embedded-API entry point
    /// (item 128): mint a fresh CSRF `state` token + PKCE `code_verifier`,
    /// persist them (hash-only for `state` — same posture as every other
    /// single-use token in this module) as a pending, single-use
    /// [`OAuthStateRec`], and return `(raw_state, raw_code_verifier,
    /// expires_at_unix_secs)`. The caller (`src/server/oauth.rs`) derives
    /// the PKCE `code_challenge` from `raw_code_verifier` (a pure hash, no
    /// stored state needed for that half) and builds the provider's
    /// authorize URL; `raw_code_verifier` itself is never sent to the
    /// client — only `code_challenge` is, embedded in the redirect to the
    /// provider.
    pub fn create_oauth_state(&self, provider: &str) -> Result<(String, String, u64)> {
        let raw_state = generate_oauth_state_token();
        let raw_verifier = generate_oauth_code_verifier();
        let hash = sha256_hex(&raw_state);
        let now = now_secs();
        let expires_at = now + OAUTH_STATE_TTL_SECS;
        let mut st = self.lock();
        st.oauth_states.insert(
            hash,
            OAuthStateRec {
                provider: provider.to_string(),
                code_verifier: raw_verifier.clone(),
                issued_at: now,
                expires_at,
                used: false,
            },
        );
        self.persist(&st)?;
        Ok((raw_state, raw_verifier, expires_at))
    }

    /// `GET /auth/oauth/<provider>/callback`'s state-validation step (item
    /// 128): hashes `raw_state`, looks up the pending record, and — only if
    /// it exists, is unexpired, is not already used, and was issued for
    /// exactly `provider` — marks it used and returns the PKCE
    /// `code_verifier` needed to complete the token exchange.
    /// `Ok(None)` uniformly covers every failure case (unknown/garbage
    /// state, expired, already-used/replayed, or a provider mismatch — e.g.
    /// a `google` state replayed against the `github` callback route) — no
    /// oracle on *why* validation failed, same posture as [`Self::
    /// verify_mfa_challenge`]. `Err` only for a genuine persistence failure.
    pub fn verify_oauth_state(&self, raw_state: &str, provider: &str) -> Result<Option<String>> {
        let hash = sha256_hex(raw_state);
        let mut st = self.lock();
        let Some(rec) = st.oauth_states.get(&hash).cloned() else {
            return Ok(None);
        };
        if rec.used || rec.expires_at <= now_secs() || rec.provider != provider {
            return Ok(None);
        }
        if let Some(r) = st.oauth_states.get_mut(&hash) {
            r.used = true;
        }
        self.persist(&st)?;
        Ok(Some(rec.code_verifier))
    }

    /// Username already linked to `(provider, provider_user_id)`, if any
    /// (item 128). `None` for a never-seen identity.
    pub fn oauth_identity_username(
        &self,
        provider: &str,
        provider_user_id: &str,
    ) -> Option<String> {
        self.lock()
            .oauth_identities
            .get(provider)
            .and_then(|m| m.get(provider_user_id))
            .cloned()
    }

    /// Resolve `(provider, provider_user_id)` to a unidb username in one
    /// atomic step (item 128, the callback handler's "link or create"):
    /// if the identity is already linked, returns the existing username
    /// unchanged (the repeat-login path — requirement (b), "a second login
    /// with the same provider identity reuses the same unidb user, no
    /// duplicate"). Otherwise creates a fresh **non-superuser, no-password**
    /// account (`username_hint`, disambiguated with a short random suffix on
    /// the vanishingly unlikely case that exact name is already taken by an
    /// unrelated user) and links it. A single lock acquisition covers the
    /// whole check-then-create sequence, closing the check-then-act race a
    /// two-call version would have between two concurrent first-time logins
    /// for the same brand-new identity (the item-122 TOCTOU lesson).
    ///
    /// **Design choice — create, never auto-link by email** (documented in
    /// the item-128 backlog doc): a returning identity always resolves via
    /// the `(provider, provider_user_id)` map, never by matching a claimed
    /// `email` against an existing local account. Auto-linking by email
    /// would let anyone who controls *any* OAuth identity sharing an email
    /// string (a provider that allows a self-set/unverified email, or a
    /// simply spoofed claim) silently take over an existing
    /// password-protected unidb account — a real account-takeover surface.
    /// A future D5 (email flows) can offer an explicit, user-initiated "link
    /// this OAuth identity to my existing account" action once verified
    /// email exists; auto-linking on the claim alone never will.
    pub fn oauth_link_or_create(
        &self,
        provider: &str,
        provider_user_id: &str,
        username_hint: &str,
    ) -> Result<String> {
        let mut st = self.lock();
        if let Some(existing) = st
            .oauth_identities
            .get(provider)
            .and_then(|m| m.get(provider_user_id))
        {
            return Ok(existing.clone());
        }
        let mut username = username_hint.to_string();
        if st.users.contains_key(&username) {
            // Extremely unlikely: the deterministic hint collides with an
            // unrelated existing user. Disambiguate with a short random
            // suffix rather than failing the whole login.
            let suffix = &generate_oauth_state_token()[..8];
            username = format!("{username_hint}_{suffix}");
        }
        st.users.insert(username.clone(), false);
        st.oauth_identities
            .entry(provider.to_string())
            .or_default()
            .insert(provider_user_id.to_string(), username.clone());
        self.persist(&st)?;
        tracing::info!(provider = %provider, user = %username, "OAuth identity linked (new account created)");
        Ok(username)
    }

    // ── item 138: email transport + password-reset / magic-link flows ───

    /// Whether `user` is a registered account (item 138 — the existence
    /// check `POST /auth/recover`/`POST /auth/magiclink` make *before*
    /// deciding whether to mint a token/send an email). The caller must
    /// still return the identical `200` regardless of this result — see
    /// `handlers.rs::post_auth_recover`/`post_auth_magiclink`'s doc comments
    /// for the no-account-enumeration contract this check feeds into.
    pub fn user_exists(&self, user: &str) -> bool {
        self.lock().users.contains_key(user)
    }

    /// Mint a fresh, single-use password-recovery token for `user` (item 138
    /// — `POST /auth/recover`'s embedded-API entry point), valid for
    /// [`RECOVERY_TOKEN_TTL_SECS`]. Returns `(raw_token,
    /// expires_at_unix_secs)`. Callers must already have confirmed `user`
    /// exists (see [`Self::user_exists`]) — this method itself performs no
    /// existence check and always succeeds, mirroring [`Self::
    /// create_oauth_state`]'s unconditional-mint posture (the no-enumeration
    /// gate lives in the caller, not here).
    pub fn create_recovery_token(&self, user: &str) -> Result<(String, u64)> {
        let raw = generate_recovery_token();
        let hash = sha256_hex(&raw);
        let now = now_secs();
        let expires_at = now + RECOVERY_TOKEN_TTL_SECS;
        let mut st = self.lock();
        st.recovery_tokens.insert(
            hash,
            EmailTokenRec {
                username: user.to_string(),
                issued_at: now,
                expires_at,
                used: false,
            },
        );
        self.persist(&st)?;
        Ok((raw, expires_at))
    }

    /// Validate + single-use-consume a password-recovery token (item 138 —
    /// `POST /auth/verify`'s embedded-API entry point). `Ok(None)`
    /// uniformly covers every failure case (unknown/garbage token, expired,
    /// already-used/replayed) — no oracle on *why* validation failed, same
    /// posture as [`Self::verify_oauth_state`]/[`Self::
    /// verify_mfa_challenge`]. `Ok(Some(username))` on success; `Err` only
    /// for a genuine persistence failure.
    pub fn verify_recovery_token(&self, raw_token: &str) -> Result<Option<String>> {
        let hash = sha256_hex(raw_token);
        let mut st = self.lock();
        let Some(rec) = st.recovery_tokens.get(&hash).cloned() else {
            return Ok(None);
        };
        if rec.used || rec.expires_at <= now_secs() {
            return Ok(None);
        }
        if let Some(r) = st.recovery_tokens.get_mut(&hash) {
            r.used = true;
        }
        self.persist(&st)?;
        Ok(Some(rec.username))
    }

    /// Mint a fresh, single-use magic-link login token for `user` (item 138
    /// — `POST /auth/magiclink`'s embedded-API entry point), valid for
    /// [`MAGICLINK_TOKEN_TTL_SECS`]. Same unconditional-mint posture as
    /// [`Self::create_recovery_token`] — the caller has already confirmed
    /// `user` exists.
    pub fn create_magiclink_token(&self, user: &str) -> Result<(String, u64)> {
        let raw = generate_magiclink_token();
        let hash = sha256_hex(&raw);
        let now = now_secs();
        let expires_at = now + MAGICLINK_TOKEN_TTL_SECS;
        let mut st = self.lock();
        st.magiclink_tokens.insert(
            hash,
            EmailTokenRec {
                username: user.to_string(),
                issued_at: now,
                expires_at,
                used: false,
            },
        );
        self.persist(&st)?;
        Ok((raw, expires_at))
    }

    /// Validate + single-use-consume a magic-link login token (item 138 —
    /// `POST /auth/magiclink/verify`'s embedded-API entry point). Same
    /// uniform-`Ok(None)`-on-any-failure contract as [`Self::
    /// verify_recovery_token`], over the independent `magiclink_tokens`
    /// token space.
    pub fn verify_magiclink_token(&self, raw_token: &str) -> Result<Option<String>> {
        let hash = sha256_hex(raw_token);
        let mut st = self.lock();
        let Some(rec) = st.magiclink_tokens.get(&hash).cloned() else {
            return Ok(None);
        };
        if rec.used || rec.expires_at <= now_secs() {
            return Ok(None);
        }
        if let Some(r) = st.magiclink_tokens.get_mut(&hash) {
            r.used = true;
        }
        self.persist(&st)?;
        Ok(Some(rec.username))
    }

    /// Revoke every currently-active refresh-token session belonging to
    /// `user` (item 138 — `POST /auth/verify`'s "revoke existing sessions"
    /// step, so a password reset immediately invalidates any session an
    /// attacker who had the *old* password may already hold). Idempotent —
    /// a user with no sessions (or all already revoked) is a silent no-op,
    /// same posture as [`Self::revoke_session`]. Persists once regardless of
    /// how many sessions were revoked.
    pub fn revoke_all_sessions_for_user(&self, user: &str) -> Result<()> {
        let mut st = self.lock();
        let mut changed = false;
        for rec in st.sessions.values_mut() {
            if rec.username == user && !rec.revoked {
                rec.revoked = true;
                changed = true;
            }
        }
        if changed {
            self.persist(&st)?;
        }
        Ok(())
    }

    // ── item 129 (Workstream I3): encrypt-at-rest secrets vault ─────────

    /// Encrypt `plaintext` with `vault` and persist it under `name`,
    /// replacing any existing secret of the same name. Errors (never
    /// stores plaintext) if `vault` is disabled ([`crate::vault::Vault::
    /// is_enabled`] `== false` — no `UNIDB_MASTER_KEY`) — there is no code
    /// path in this store that can persist a secret unencrypted. Never
    /// logs `plaintext`; only the name is logged.
    pub fn set_secret(
        &self,
        name: &str,
        plaintext: &str,
        vault: &crate::vault::Vault,
    ) -> Result<()> {
        let blob = vault.encrypt(plaintext)?;
        let mut st = self.lock();
        st.secrets.insert(name.to_string(), blob);
        self.persist(&st)?;
        tracing::info!(secret = %name, "secret stored in vault");
        Ok(())
    }

    /// Decrypt and return the secret stored under `name`, or `Ok(None)` if
    /// no secret with that name has ever been stored — callers (e.g. the
    /// OAuth client-secret resolver) treat `None` as "fall back to another
    /// source", never as an error. **Fails closed** (`Err`, never a
    /// plaintext-fallback `Ok`) when a secret WAS stored under `name` but
    /// cannot be decrypted — vault disabled/wrong master key, or a
    /// tampered/corrupt ciphertext blob (GCM tag mismatch) — see
    /// [`crate::vault::Vault::decrypt`]. Never logs the plaintext.
    pub fn get_secret(&self, name: &str, vault: &crate::vault::Vault) -> Result<Option<String>> {
        let blob = {
            let st = self.lock();
            st.secrets.get(name).cloned()
        };
        match blob {
            Some(blob) => vault.decrypt(&blob).map(Some),
            None => Ok(None),
        }
    }

    /// Whether a secret is currently stored under `name` (item 129) —
    /// never decrypts, never touches the vault; used by `unidb-vault has`
    /// and the OAuth resolver's "is anything stored at all" check.
    pub fn has_secret(&self, name: &str) -> bool {
        self.lock().secrets.contains_key(name)
    }

    /// Every currently-stored secret name (item 129) — names only, never
    /// ciphertext or plaintext; backs `unidb-vault list`.
    pub fn secret_names(&self) -> Vec<String> {
        self.lock().secrets.keys().cloned().collect()
    }

    // ── item 140: realtime channel-authorization policies ──────────────

    /// Whether `topic` matches `pattern`: exact equality, or `pattern` is a
    /// `*`-suffix glob (the only glob shape the locked v1 design supports)
    /// and `topic` starts with the prefix before the `*`.
    fn channel_topic_matches(pattern: &str, topic: &str) -> bool {
        match pattern.strip_suffix('*') {
            Some(prefix) => topic.starts_with(prefix),
            None => pattern == topic,
        }
    }

    /// Specificity score backing "most-specific match wins (longest
    /// non-wildcard prefix)" (item-140 backlog doc): the length of the
    /// pattern's fixed, non-wildcard prefix. An exact pattern counts its
    /// whole length (nothing to strip), so an exact match always outranks
    /// any glob that is merely a prefix of it.
    fn channel_pattern_specificity(pattern: &str) -> usize {
        pattern.strip_suffix('*').unwrap_or(pattern).len()
    }

    /// Find the single most-specific policy governing `operation` on `topic`.
    /// Ties in specificity (two policies sharing the identical
    /// `topic_pattern` string, one scoped to the exact `operation` and one
    /// scoped `All`) favor the exact-operation policy — a deliberate,
    /// documented tie-break the backlog spec leaves unstated, chosen so an
    /// explicit per-operation rule always overrides a same-pattern catch-all.
    /// Never ambiguous: at most one policy is returned.
    fn match_channel_policy<'a>(
        policies: &'a [ChannelPolicyDef],
        topic: &str,
        operation: ChannelOp,
    ) -> Option<&'a ChannelPolicyDef> {
        policies
            .iter()
            .filter(|p| {
                p.operation.governs(operation)
                    && Self::channel_topic_matches(&p.topic_pattern, topic)
            })
            .max_by_key(|p| {
                (
                    Self::channel_pattern_specificity(&p.topic_pattern),
                    p.operation != ChannelOp::All,
                )
            })
    }

    /// The channel-authorization decision for `(topic, operation)`, given the
    /// caller's already-resolved effective roles. **Deliberately has no
    /// superuser/`service_role` bypass and no audit side effect** — that
    /// decision (and its audited trail, item-103 lesson) is the caller's job;
    /// see `Engine::check_channel_authz`. Fail-closed: a topic matching no
    /// policy is allowed only when `require_authz` is false (item-132
    /// back-compat default); a topic matching a policy is *always* enforced
    /// regardless of `require_authz` — allowed only if `roles` intersects the
    /// policy's `roles`.
    pub fn check_channel_policy(
        &self,
        topic: &str,
        operation: ChannelOp,
        roles: &[String],
        require_authz: bool,
    ) -> bool {
        let st = self.lock();
        match Self::match_channel_policy(&st.channel_policies, topic, operation) {
            Some(policy) => roles.iter().any(|r| policy.roles.contains(r)),
            None => !require_authz,
        }
    }

    /// Upsert a channel-authorization policy (item 140 admin surface):
    /// replaces the role set of an existing `(topic_pattern, operation)`
    /// policy, or inserts a new one. Superuser gating is the HTTP handler's
    /// job (mirrors `set_rls_policy_sql`/`Engine::set_rls_policy`) — this
    /// method itself is unrestricted, matching every other direct
    /// `RoleStore` mutator (the embedded API is the trusted caller).
    pub fn set_channel_policy(
        &self,
        topic_pattern: &str,
        operation: ChannelOp,
        roles: BTreeSet<String>,
    ) -> Result<()> {
        let mut st = self.lock();
        match st
            .channel_policies
            .iter_mut()
            .find(|p| p.topic_pattern == topic_pattern && p.operation == operation)
        {
            Some(existing) => existing.roles = roles,
            None => st.channel_policies.push(ChannelPolicyDef {
                topic_pattern: topic_pattern.to_string(),
                operation,
                roles,
            }),
        }
        self.persist(&st)?;
        // Never log the role set (or anything policy-shaped) at info — only
        // the pattern/operation "coordinates", no roles/tokens, at debug.
        tracing::debug!(
            topic_pattern = %topic_pattern,
            operation = %operation.as_str(),
            "realtime channel policy upserted"
        );
        Ok(())
    }

    /// Remove a channel-authorization policy (item 140 admin surface).
    /// Idempotent: removing a `(topic_pattern, operation)` pair that doesn't
    /// exist is a silent no-op, not an error (mirrors `revoke_session`'s
    /// idempotency posture).
    pub fn remove_channel_policy(&self, topic_pattern: &str, operation: ChannelOp) -> Result<()> {
        let mut st = self.lock();
        let before = st.channel_policies.len();
        st.channel_policies
            .retain(|p| !(p.topic_pattern == topic_pattern && p.operation == operation));
        if st.channel_policies.len() != before {
            self.persist(&st)?;
            tracing::debug!(
                topic_pattern = %topic_pattern,
                operation = %operation.as_str(),
                "realtime channel policy removed"
            );
        }
        Ok(())
    }

    /// Every stored channel-authorization policy (item 140, `GET /realtime/
    /// policies`).
    pub fn list_channel_policies(&self) -> Vec<ChannelPolicyDef> {
        self.lock().channel_policies.clone()
    }

    // ── item 141: database webhooks (outbound HTTP on row change) ──────

    /// Upsert a webhook registration (item 141 admin surface): replaces an
    /// existing entry sharing the same `id`, or inserts a new one. Superuser
    /// gating is the HTTP handler's job (mirrors [`Self::set_channel_policy`])
    /// — this method itself is unrestricted, matching every other direct
    /// `RoleStore` mutator (the embedded API is the trusted caller). Never
    /// logs the secret — only the `id`.
    pub fn upsert_webhook(&self, def: WebhookDef) -> Result<()> {
        let mut st = self.lock();
        let id = def.id.clone();
        match st.webhooks.iter_mut().find(|w| w.id == def.id) {
            Some(existing) => *existing = def,
            None => st.webhooks.push(def),
        }
        self.persist(&st)?;
        tracing::debug!(webhook = %id, "webhook registration upserted");
        Ok(())
    }

    /// Remove a webhook registration by `id` (item 141 admin surface).
    /// Idempotent — removing an `id` that doesn't exist is a silent no-op,
    /// mirroring [`Self::remove_channel_policy`]'s posture.
    pub fn remove_webhook(&self, id: &str) -> Result<()> {
        let mut st = self.lock();
        let before = st.webhooks.len();
        st.webhooks.retain(|w| w.id != id);
        if st.webhooks.len() != before {
            self.persist(&st)?;
            tracing::debug!(webhook = %id, "webhook registration removed");
        }
        Ok(())
    }

    /// Every registered webhook (item 141), secret included — `GET
    /// /webhooks` (`server::handlers::get_webhooks`) redacts before
    /// serializing; the delivery worker (`server::webhooks`) uses the full
    /// record, including the fallback plaintext secret, to sign outbound
    /// payloads.
    pub fn list_webhooks(&self) -> Vec<WebhookDef> {
        self.lock().webhooks.clone()
    }

    // ── item 144: scheduled jobs (cron) ─────────────────────────────────

    /// Upsert a scheduled-job registration (item 144 admin surface):
    /// replaces an existing entry sharing the same `name`, or inserts a new
    /// one. Superuser gating and cron-syntax validation are the caller's
    /// job (mirrors [`Self::upsert_webhook`] — `Engine::upsert_cron_job`
    /// validates the schedule before calling this; this method itself is
    /// unrestricted, matching every other direct `RoleStore` mutator).
    pub fn upsert_cron_job(&self, def: CronJobDef) -> Result<()> {
        let mut st = self.lock();
        let name = def.name.clone();
        match st.cron_jobs.iter_mut().find(|j| j.name == def.name) {
            Some(existing) => *existing = def,
            None => st.cron_jobs.push(def),
        }
        self.persist(&st)?;
        tracing::debug!(job = %name, "cron job registration upserted");
        Ok(())
    }

    /// Remove a scheduled-job registration by `name` (item 144 admin
    /// surface). Idempotent — removing a `name` that doesn't exist is a
    /// silent no-op, mirroring [`Self::remove_webhook`]'s posture.
    pub fn remove_cron_job(&self, name: &str) -> Result<()> {
        let mut st = self.lock();
        let before = st.cron_jobs.len();
        st.cron_jobs.retain(|j| j.name != name);
        if st.cron_jobs.len() != before {
            self.persist(&st)?;
            tracing::debug!(job = %name, "cron job registration removed");
        }
        Ok(())
    }

    /// Every registered scheduled job (item 144), `sql` included — `GET
    /// /cron/jobs` (`server::handlers::get_cron_jobs`) merges this with the
    /// scheduler worker's in-memory last-run status
    /// ([`crate::server::cron::CronState`]); the scheduler worker itself
    /// (`server::cron::run_due`) uses this list every tick to find due jobs.
    pub fn list_cron_jobs(&self) -> Vec<CronJobDef> {
        self.lock().cron_jobs.clone()
    }

    /// Whether `user` holds `priv` on `table` **at all** — `true` for both a
    /// whole-table grant and a column-scoped grant with at least one column
    /// (item 112: this existence check is deliberately coarser than column
    /// enforcement, matching Postgres's own `has_table_privilege` and this
    /// engine's pre-112 callers — `information_schema`'s item-111 ANY-privilege
    /// table visibility, `check_table_grant`'s REST bulk-route gate, and
    /// `check_plan_privileges`'s existing table-level pass all keep their
    /// exact pre-112 meaning: "does this caller have some access", not "does
    /// this caller have unrestricted access"). Column-level narrowing is a
    /// separate, additive check — see [`Self::column_grant`]. Resolves role
    /// membership transitively. Superusers hold every privilege.
    pub fn has_privilege(&self, user: &str, table: &str, priv_: Privilege) -> bool {
        let st = self.lock();
        if st.users.get(user).copied().unwrap_or(false) {
            return true;
        }
        !matches!(
            Self::resolve_column_grant(&st, user, table, priv_),
            ColumnGrant::None
        )
    }

    /// The resolved column-level grant `user` holds for `priv_` on `table`
    /// (item 112), transitively through role membership exactly like
    /// [`Self::has_privilege`]. A superuser resolves to `ColumnGrant::All`
    /// unconditionally (mirrors `has_privilege`'s superuser short-circuit).
    ///
    /// Multiple grantee paths (the user's own direct grant, plus every role
    /// reachable transitively) are **unioned**, matching Postgres: if any
    /// path grants the whole table, the result is `All` (a table-level grant
    /// through one role is never narrowed by a column-scoped grant through
    /// another); otherwise every column-scoped path's columns are unioned.
    pub fn column_grant(&self, user: &str, table: &str, priv_: Privilege) -> ColumnGrant {
        let st = self.lock();
        if st.users.get(user).copied().unwrap_or(false) {
            return ColumnGrant::All;
        }
        Self::resolve_column_grant(&st, user, table, priv_)
    }

    /// Shared resolution behind [`Self::has_privilege`]/[`Self::column_grant`]
    /// (not superuser-aware — callers check that first).
    fn resolve_column_grant(
        st: &AuthState,
        user: &str,
        table: &str,
        priv_: Privilege,
    ) -> ColumnGrant {
        // Collect the user + every role reachable through membership.
        let mut grantees: HashSet<String> = HashSet::new();
        let mut stack = vec![user.to_string()];
        while let Some(g) = stack.pop() {
            if !grantees.insert(g.clone()) {
                continue;
            }
            if let Some(roles) = st.memberships.get(&g) {
                for r in roles {
                    stack.push(r.clone());
                }
            }
        }
        let mut saw_any = false;
        let mut cols: BTreeSet<String> = BTreeSet::new();
        for g in &grantees {
            if let Some(scope) = st
                .table_grants
                .get(g)
                .and_then(|t| t.get(table))
                .and_then(|p| p.get(&priv_))
            {
                saw_any = true;
                match scope {
                    GrantScope::All => return ColumnGrant::All,
                    GrantScope::Columns(set) => cols.extend(set.iter().cloned()),
                }
            }
        }
        if saw_any {
            ColumnGrant::Columns(cols)
        } else {
            ColumnGrant::None
        }
    }

    /// Resolve the caller's **effective** roles (item 122, B3) — the
    /// Supabase-parity mapping consumed by [`crate::sql::logical::
    /// apply_rls_with_auth`]'s role gate (B4). Engine-side only (needs
    /// `self`/authz access, unlike `require_jwt` which only verifies the
    /// token) — called from `Engine::execute_sql_inner_as_principal` and
    /// `ReadHandle::execute_sql_inner`, never from the HTTP auth layer:
    ///
    /// - a verified token carrying `claims["role"] == "service_role"` →
    ///   `["service_role"]` only (Supabase convention) — this bypasses RLS
    ///   like a superuser at the call site, but that bypass decision is made
    ///   by the caller (audited distinctly, item-103 lesson), not here.
    /// - `user` is `Some` (a verified subject) → `"authenticated"` plus every
    ///   role transitively granted to that user (`GRANT <role> TO <user>`,
    ///   resolved through role-of-role membership, mirroring
    ///   [`Self::has_privilege`]'s transitive closure).
    /// - `user` is `None` (no verified subject) → `["anon"]` only.
    ///
    /// Fails closed by construction: an unknown/ungranted role never
    /// appears, and a caller who resolves to no roles at all cannot happen
    /// (every branch returns at least one role) — a role-scoped policy with
    /// a `TO` clause a caller doesn't match on simply never widens access.
    pub fn effective_roles(
        &self,
        user: Option<&str>,
        claims: &BTreeMap<String, serde_json::Value>,
    ) -> Vec<String> {
        // A `role` claim explicitly naming `service_role` or `anon` wins
        // outright, regardless of `user` — this is the real Supabase JWT
        // convention (both the anon key and the service-role key carry a
        // `role` claim; it is not inferred from `sub`'s presence). The
        // literal B3 spec ties `anon`/`authenticated` to subject presence as
        // the *default* mapping (handled below), which this claim-based
        // check does not change for a token with no `role` claim at all —
        // it only adds an explicit, disclosed override for a token that
        // *does* assert one, and is the only way an `anon`-labeled caller
        // can reach the RLS role gate without also tripping the pre-existing,
        // protected "`user == None` is the implicit/embedded superuser"
        // bypass (item 103) that a bare no-subject token always hits first.
        if let Some(role_claim) = claims.get("role").and_then(|v| v.as_str()) {
            if role_claim == SERVICE_ROLE {
                return vec![SERVICE_ROLE.to_string()];
            }
            if role_claim == ANON_ROLE {
                return vec![ANON_ROLE.to_string()];
            }
        }
        match user {
            Some(u) => {
                let st = self.lock();
                let mut roles = vec![AUTHENTICATED_ROLE.to_string()];
                roles.extend(Self::transitive_roles_for(&st, u));
                roles
            }
            None => vec![ANON_ROLE.to_string()],
        }
    }

    /// Every role reachable from `grantee` by transitive membership
    /// (`GRANT <role> TO <grantee>`, then role-of-role), NOT including
    /// `grantee` itself. Factored out of [`Self::has_privilege`]'s inline
    /// BFS (which additionally needs the grantee itself in its closure, for
    /// the direct-grant check) so [`Self::effective_roles`] (item 122, B3)
    /// can reuse the identical traversal without duplicating it.
    fn transitive_roles_for(st: &AuthState, grantee: &str) -> Vec<String> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut stack = vec![grantee.to_string()];
        let mut roles: HashSet<String> = HashSet::new();
        while let Some(g) = stack.pop() {
            if !seen.insert(g.clone()) {
                continue;
            }
            if let Some(rs) = st.memberships.get(&g) {
                for r in rs {
                    roles.insert(r.clone());
                    stack.push(r.clone());
                }
            }
        }
        roles.into_iter().collect()
    }

    /// Snapshot of the users (name, superuser).
    pub fn users(&self) -> Vec<(String, bool)> {
        self.lock()
            .users
            .iter()
            .map(|(n, s)| (n.clone(), *s))
            .collect()
    }

    // ── item 142: Auth admin API (user management) ──────────────────────

    /// Build one [`AdminUserView`] from an already-locked `st` — shared by
    /// [`Self::user_admin_view`] and [`Self::users_admin_page`] so both read
    /// the exact same fields the exact same way.
    fn admin_view_locked(st: &AuthState, name: &str, is_superuser: bool) -> AdminUserView {
        let extra = st.user_extra.get(name).cloned().unwrap_or_default();
        let roles = st
            .memberships
            .get(name)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect();
        AdminUserView {
            username: name.to_string(),
            is_superuser,
            banned: extra.banned,
            roles,
            created_at: extra.created_at,
            app_metadata: extra.app_metadata,
            user_metadata: extra.user_metadata,
        }
    }

    /// `GET /auth/admin/users/{id}` (item 142): `None` if `user` doesn't
    /// exist — the handler maps that to a `404`.
    pub fn user_admin_view(&self, user: &str) -> Option<AdminUserView> {
        let st = self.lock();
        let is_superuser = *st.users.get(user)?;
        Some(Self::admin_view_locked(&st, user, is_superuser))
    }

    /// `GET /auth/admin/users?limit=&offset=` (item 142): a page of users in
    /// stable (username-sorted, since `AuthState::users` is a `BTreeMap`)
    /// order, plus the total user count (item 139's `Content-Range`-style
    /// posture: the total is always the *unpaginated* count, not the page
    /// length).
    pub fn users_admin_page(&self, limit: usize, offset: usize) -> (Vec<AdminUserView>, usize) {
        let st = self.lock();
        let total = st.users.len();
        let rows = st
            .users
            .iter()
            .skip(offset)
            .take(limit)
            .map(|(name, is_superuser)| Self::admin_view_locked(&st, name, *is_superuser))
            .collect();
        (rows, total)
    }

    /// Whether `user` is currently banned (item 142). `false` for an
    /// unknown user or a user with no `user_extra` entry (pre-142
    /// `roles.json`) — see [`UserExtra`]'s doc comment. Checked at every
    /// auth-decision point: `POST /auth/login`, `POST /auth/refresh`, and
    /// the email-flow redemption routes (`POST /auth/verify`, `POST
    /// /auth/magiclink/verify`) — see `server::handlers`.
    pub fn is_banned(&self, user: &str) -> bool {
        self.lock()
            .user_extra
            .get(user)
            .map(|e| e.banned)
            .unwrap_or(false)
    }

    /// Set (or clear) `user`'s ban flag (item 142 — `PATCH
    /// /auth/admin/users/{id}`'s `banned` field, and `POST
    /// /auth/admin/users`'s `banned: true` at creation time). Errors if
    /// `user` doesn't exist. Does **not** revoke sessions itself — the
    /// caller (`Engine::admin_update_user`/`admin_create_user`) does that
    /// as a separate, explicitly-audited step, mirroring how `POST
    /// /auth/verify` composes `set_password` + `revoke_all_sessions_for_user`
    /// today rather than folding session revocation into the credential
    /// setter.
    pub fn set_banned(&self, user: &str, banned: bool) -> Result<()> {
        let mut st = self.lock();
        if !st.users.contains_key(user) {
            return Err(DbError::Authz(format!("user '{user}' not found")));
        }
        st.user_extra.entry(user.to_string()).or_default().banned = banned;
        self.persist(&st)?;
        tracing::info!(user = %user, banned, "user ban state changed");
        Ok(())
    }

    /// Set `user`'s admin-only `app_metadata` (item 142 — `PATCH
    /// /auth/admin/users/{id}`'s `app_metadata` field). Replaces the whole
    /// value (not a merge — same "last write wins on the whole field"
    /// posture as every other admin-surface setter in this module). Errors
    /// if `user` doesn't exist.
    pub fn set_app_metadata(&self, user: &str, value: serde_json::Value) -> Result<()> {
        let mut st = self.lock();
        if !st.users.contains_key(user) {
            return Err(DbError::Authz(format!("user '{user}' not found")));
        }
        st.user_extra
            .entry(user.to_string())
            .or_default()
            .app_metadata = value;
        self.persist(&st)
    }

    /// Set `user`'s `user_metadata` (item 142). Same replace-whole-value and
    /// existence-check posture as [`Self::set_app_metadata`].
    pub fn set_user_metadata(&self, user: &str, value: serde_json::Value) -> Result<()> {
        let mut st = self.lock();
        if !st.users.contains_key(user) {
            return Err(DbError::Authz(format!("user '{user}' not found")));
        }
        st.user_extra
            .entry(user.to_string())
            .or_default()
            .user_metadata = value;
        self.persist(&st)
    }

    /// Set `user`'s superuser flag (item 142 — `PATCH
    /// /auth/admin/users/{id}`'s `superuser` field). Demoting (`superuser:
    /// false`) a currently-superuser account shares the exact same
    /// last-superuser guard as `DROP USER`/`DELETE /auth/admin/users/{id}`
    /// (see [`Self::apply`]'s `DropUser` branch) — removing the last
    /// remaining superuser's status is the identical self-lockout risk as
    /// deleting them outright. A no-op call (already at the requested
    /// value) skips the persist entirely.
    pub fn set_superuser(&self, user: &str, superuser: bool) -> Result<()> {
        let mut st = self.lock();
        let Some(&current) = st.users.get(user) else {
            return Err(DbError::Authz(format!("user '{user}' not found")));
        };
        if current == superuser {
            return Ok(());
        }
        if current && !superuser && st.users.values().filter(|&&s| s).count() <= 1 {
            return Err(DbError::PermissionDenied(format!(
                "cannot remove superuser status from '{user}': it is the last remaining superuser"
            )));
        }
        st.users.insert(user.to_string(), superuser);
        self.persist(&st)
    }

    /// Snapshot of all role names (item-24 Z5: `unidb_catalog.roles`).
    pub fn roles(&self) -> Vec<String> {
        self.lock().roles.iter().cloned().collect()
    }

    /// Snapshot of all role memberships as `(role, member)` pairs.
    /// `member` is a user or role that has been granted membership in `role`.
    /// (item-24 Z4: `unidb_catalog.role_members`)
    pub fn memberships(&self) -> Vec<(String, String)> {
        let st = self.lock();
        let mut out = Vec::new();
        for (member, roles) in &st.memberships {
            for role in roles {
                out.push((role.clone(), member.clone()));
            }
        }
        out
    }

    /// Snapshot of all grants as `(grantee, table, privilege, columns)`
    /// tuples (item-24 Z5 + item 112: `unidb_catalog.grants`). `columns` is
    /// `None` for a whole-table grant, `Some(sorted columns)` for a
    /// column-scoped one.
    pub fn grants(&self) -> Vec<(String, String, Privilege, Option<Vec<String>>)> {
        let st = self.lock();
        let mut out = Vec::new();
        for (grantee, tables) in &st.table_grants {
            for (table, privs) in tables.iter() {
                for (p, scope) in privs.iter() {
                    let cols = match scope {
                        GrantScope::All => None,
                        GrantScope::Columns(set) => Some(set.iter().cloned().collect()),
                    };
                    out.push((grantee.clone(), table.clone(), *p, cols));
                }
            }
        }
        out
    }

    /// Table-level grants for a user, collected as `(table, [privilege_names])`.
    /// Used by `GET /auth/whoami` (item 100). Lists a privilege the user
    /// holds on the table regardless of column scope (item 112) — same
    /// "holds it at all" semantics as [`Self::has_privilege`]; whoami is not
    /// the place callers learn *which* columns (see `unidb_catalog.grants`
    /// or `information_schema.columns` for that).
    pub fn table_grants_for(&self, user: &str) -> Vec<(String, Vec<String>)> {
        let st = self.lock();
        match st.table_grants.get(user) {
            Some(tables) => tables
                .iter()
                .map(|(tbl, privs)| {
                    (
                        tbl.clone(),
                        privs.keys().map(|p| p.as_str().to_string()).collect(),
                    )
                })
                .collect(),
            None => Vec::new(),
        }
    }

    /// Item 112: if `grantee`'s grant of `priv_` on `table` is currently
    /// whole-table (`GrantScope::All`), replace it with an explicit
    /// `Columns(all_columns)` — a no-op if the grant is already column-scoped
    /// or absent. Used by `Engine::exec_auth_stmt` (which has catalog access,
    /// unlike this control-plane-only store) immediately before applying a
    /// column-scoped `REVOKE` against a grantee who currently holds the
    /// whole table, so the revoke has an explicit column set to narrow —
    /// `RoleStore::apply`'s `RevokePrivs` handling only ever narrows an
    /// existing `Columns` scope, it never has catalog access to compute
    /// "every column except these" on its own.
    pub fn materialize_all_to_columns(
        &self,
        grantee: &str,
        table: &str,
        priv_: Privilege,
        all_columns: &[String],
    ) -> Result<()> {
        let mut st = self.lock();
        if let Some(scope) = st
            .table_grants
            .get_mut(grantee)
            .and_then(|t| t.get_mut(table))
            .and_then(|p| p.get_mut(&priv_))
        {
            if matches!(scope, GrantScope::All) {
                *scope = GrantScope::Columns(all_columns.iter().cloned().collect());
                return self.persist(&st);
            }
        }
        Ok(())
    }

    /// Roles a user belongs to (direct memberships only; not transitive).
    /// Used by `GET /auth/whoami` (item 100).
    pub fn roles_for(&self, user: &str) -> Vec<String> {
        let st = self.lock();
        st.memberships
            .get(user)
            .map(|roles| roles.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Apply a parsed auth-DDL statement. The caller must have already checked
    /// the actor is a superuser.
    pub fn apply(&self, stmt: &AuthStmt) -> Result<()> {
        let mut st = self.lock();
        match stmt {
            AuthStmt::CreateUser {
                name,
                superuser,
                password,
            } => {
                if st.users.contains_key(name) {
                    return Err(DbError::Authz(format!("user '{name}' already exists")));
                }
                // Hash before inserting the user, so a hashing failure (e.g.
                // the password is somehow rejected) leaves no partial state —
                // either both the user and its credential land together, or
                // neither does.
                let hash = match password {
                    Some(pw) => Some(hash_password(pw)?),
                    None => None,
                };
                st.users.insert(name.clone(), *superuser);
                if let Some(hash) = hash {
                    st.credentials.insert(name.clone(), hash);
                }
                // item 142: seed a fresh admin-surface record (unbanned,
                // empty metadata) stamped with the creation time. Every user
                // gets one from the moment it exists, whether created via
                // this SQL DDL path or `Engine::admin_create_user`'s `POST
                // /auth/admin/users` — a plain `insert` (not `entry(..)
                // .or_default()`) is deliberate: a name reused after a prior
                // `DropUser` (which already clears `user_extra` below) must
                // never inherit a stale ban/metadata record.
                st.user_extra.insert(
                    name.clone(),
                    UserExtra {
                        created_at: now_secs(),
                        ..Default::default()
                    },
                );
            }
            AuthStmt::DropUser(name) => {
                let Some(&is_super) = st.users.get(name) else {
                    return Err(DbError::Authz(format!("user '{name}' not found")));
                };
                // item 142: last-superuser self-lockout guard. Applies
                // uniformly to `DROP USER` (SQL) and `DELETE
                // /auth/admin/users/{id}` (both route through this one
                // branch) — dropping the only remaining superuser would
                // leave no account able to administer the server at all
                // (short of restarting into open/bootstrap mode by
                // deleting every user, a much bigger footgun).
                if is_super && st.users.values().filter(|&&s| s).count() <= 1 {
                    return Err(DbError::PermissionDenied(format!(
                        "cannot drop user '{name}': it is the last remaining superuser"
                    )));
                }
                st.users.remove(name);
                st.memberships.remove(name);
                st.table_grants.remove(name);
                st.credentials.remove(name);
                // item 127: a dropped user's MFA enrollment (secret +
                // recovery-code hashes) must not linger — mirrors dropping
                // `credentials` above.
                st.mfa.remove(name);
                // item 142: ban flag + metadata must not linger either —
                // same "no orphaned per-user state" posture as `mfa` above.
                st.user_extra.remove(name);
                // item 128: drop any OAuth identity link(s) pointing at this
                // user too, so a later `CREATE USER` reusing the same name
                // never silently inherits a stranger's OAuth login.
                for m in st.oauth_identities.values_mut() {
                    m.retain(|_, u| u != name);
                }
            }
            AuthStmt::AlterUserPassword { name, password } => {
                if !st.users.contains_key(name) {
                    return Err(DbError::Authz(format!("user '{name}' not found")));
                }
                // Same argon2id hashing helper as `CreateUser`'s inline path
                // and the Rust-API `RoleStore::set_password` — reused here
                // rather than calling `set_password` directly because it
                // would re-lock `self.inner` (already held as `st` above)
                // and deadlock.
                let hash = hash_password(password)?;
                st.credentials.insert(name.clone(), hash);
            }
            AuthStmt::CreateRole(name) => {
                if is_reserved_role(name) {
                    return Err(DbError::Authz(format!(
                        "role '{name}' is reserved (built-in) and cannot be created"
                    )));
                }
                if !st.roles.insert(name.clone()) {
                    return Err(DbError::Authz(format!("role '{name}' already exists")));
                }
            }
            AuthStmt::DropRole(name) => {
                if is_reserved_role(name) {
                    return Err(DbError::Authz(format!(
                        "role '{name}' is reserved (built-in) and cannot be dropped"
                    )));
                }
                if !st.roles.remove(name) {
                    return Err(DbError::Authz(format!("role '{name}' not found")));
                }
                st.memberships.remove(name);
                st.table_grants.remove(name);
                for roles in st.memberships.values_mut() {
                    roles.remove(name);
                }
            }
            AuthStmt::GrantPrivs {
                privs,
                table,
                grantee,
                columns,
            } => {
                Self::require_grantee(&st, grantee)?;
                let entry = st
                    .table_grants
                    .entry(grantee.clone())
                    .or_default()
                    .entry(table.clone())
                    .or_default();
                for p in privs {
                    // item 112: GRANT only ever widens, never narrows, an
                    // existing grant (Postgres semantics — REVOKE is the
                    // only narrowing operation). A bare (whole-table)
                    // `GRANT` always sets/keeps `All`; a column-scoped
                    // `GRANT` unions its columns into an existing
                    // column-scoped grant, or is a no-op widen-preserving
                    // step against an existing `All` grant.
                    match columns {
                        None => {
                            entry.insert(*p, GrantScope::All);
                        }
                        Some(cols) => match entry.get_mut(p) {
                            Some(GrantScope::All) => {} // already every column — leave it
                            Some(GrantScope::Columns(set)) => {
                                set.extend(cols.iter().cloned());
                            }
                            None => {
                                entry.insert(
                                    *p,
                                    GrantScope::Columns(cols.iter().cloned().collect()),
                                );
                            }
                        },
                    }
                }
            }
            AuthStmt::RevokePrivs {
                privs,
                table,
                grantee,
                columns,
            } => {
                if let Some(t) = st
                    .table_grants
                    .get_mut(grantee)
                    .and_then(|g| g.get_mut(table))
                {
                    for p in privs {
                        match columns {
                            // Table-level revoke clears the privilege
                            // entirely, regardless of column scope (item 112
                            // spec: "table-level revoke clears") — pre-112
                            // behavior, unchanged.
                            None => {
                                t.remove(p);
                            }
                            Some(cols) => match t.get_mut(p) {
                                // A whole-table grant cannot be narrowed here
                                // (this store has no catalog access to
                                // compute "every column except these") — the
                                // caller (`Engine::exec_auth_stmt`) must call
                                // `materialize_all_to_columns` first so this
                                // arm never actually observes `All` in
                                // practice; left as a safe (access-preserving,
                                // not access-widening) no-op if it ever does.
                                Some(GrantScope::All) => {}
                                Some(GrantScope::Columns(set)) => {
                                    for c in cols {
                                        set.remove(c);
                                    }
                                    if set.is_empty() {
                                        t.remove(p);
                                    }
                                }
                                None => {} // nothing granted — silent no-op, matches table-level revoke-of-nothing
                            },
                        }
                    }
                }
            }
            AuthStmt::GrantRole { role, grantee } => {
                if !st.roles.contains(role) {
                    return Err(DbError::Authz(format!("role '{role}' does not exist")));
                }
                Self::require_grantee(&st, grantee)?;
                st.memberships
                    .entry(grantee.clone())
                    .or_default()
                    .insert(role.clone());
            }
            AuthStmt::RevokeRole { role, grantee } => {
                if let Some(roles) = st.memberships.get_mut(grantee) {
                    roles.remove(role);
                }
            }
            // Policy DDL is routed to the catalog by `Engine::exec_auth_stmt`
            // before it ever reaches here. If somehow called directly, return
            // an internal error rather than panicking.
            AuthStmt::CreatePolicy(_) | AuthStmt::DropPolicy { .. } => {
                return Err(DbError::Authz(
                    "CREATE/DROP POLICY must be applied via Engine::exec_auth_stmt, not RoleStore::apply".into(),
                ));
            }
        }
        self.persist(&st)?;
        tracing::info!(?stmt, "auth DDL applied");
        Ok(())
    }

    fn require_grantee(st: &AuthState, grantee: &str) -> Result<()> {
        if st.users.contains_key(grantee) || st.roles.contains(grantee) {
            Ok(())
        } else {
            Err(DbError::Authz(format!(
                "grantee '{grantee}' is not a known user or role"
            )))
        }
    }
}

/// Compile-time proof the store is shareable on the `Engine`.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<RoleStore>();
};

/// Detect + parse an auth-DDL statement. Returns `None` for non-auth SQL (which
/// flows to the normal parser). Errors only on a malformed auth statement.
pub fn parse_auth_stmt(sql: &str) -> Result<Option<AuthStmt>> {
    let trimmed = sql.trim().trim_end_matches(';');
    let toks: Vec<&str> = trimmed.split_whitespace().collect();
    if toks.len() < 2 {
        return Ok(None);
    }
    let kw = toks[0].to_ascii_uppercase();
    let kw2 = toks[1].to_ascii_uppercase();
    match (kw.as_str(), kw2.as_str()) {
        ("CREATE", "USER") => parse_create_user(trimmed, &toks).map(Some),
        ("ALTER", "USER") => parse_alter_user_password(trimmed, &toks).map(Some),
        ("DROP", "USER") => Ok(Some(AuthStmt::DropUser(ident(toks.get(2))?))),
        ("CREATE", "ROLE") => Ok(Some(AuthStmt::CreateRole(ident(toks.get(2))?))),
        ("DROP", "ROLE") => Ok(Some(AuthStmt::DropRole(ident(toks.get(2))?))),
        ("GRANT", _) => parse_grant_revoke(&toks, true).map(Some),
        ("REVOKE", _) => parse_grant_revoke(&toks, false).map(Some),
        ("CREATE", "POLICY") => parse_create_policy(trimmed).map(Some),
        ("DROP", "POLICY") => parse_drop_policy(&toks).map(Some),
        _ => Ok(None),
    }
}

/// `CREATE USER <name> [SUPERUSER] [PASSWORD '<pw>']` (item 121, A1).
///
/// `name`/`SUPERUSER` come from the whitespace-tokenised `toks` (as before);
/// the optional `PASSWORD` clause is scanned out of the raw `sql` string
/// instead, so a password containing spaces (or an escaped `''` literal
/// quote) parses correctly — unlike the rest of this token-based grammar, a
/// password is free-form text, not an identifier.
fn parse_create_user(sql: &str, toks: &[&str]) -> Result<AuthStmt> {
    let name = ident(toks.get(2))?;
    let superuser = toks
        .get(3)
        .map(|s| s.eq_ignore_ascii_case("SUPERUSER"))
        .unwrap_or(false);

    let upper = sql.to_ascii_uppercase();
    let password = match upper.find("PASSWORD") {
        Some(pw_pos) => Some(parse_quoted_password(&sql[pw_pos + "PASSWORD".len()..])?),
        None => None,
    };

    Ok(AuthStmt::CreateUser {
        name,
        superuser,
        password,
    })
}

/// `ALTER USER <name> PASSWORD '<pw>'` (Studio password reset). Mirrors
/// `parse_create_user`'s `PASSWORD` handling — the clause is scanned out of
/// the raw `sql` string (not the whitespace-tokenised `toks`) so a password
/// containing spaces or an escaped `''` literal quote parses correctly.
fn parse_alter_user_password(sql: &str, toks: &[&str]) -> Result<AuthStmt> {
    let name = ident(toks.get(2))?;
    let upper = sql.to_ascii_uppercase();
    let pw_pos = upper
        .find("PASSWORD")
        .ok_or_else(|| DbError::SqlParse("ALTER USER: expected a PASSWORD '<pw>' clause".into()))?;
    let password = parse_quoted_password(&sql[pw_pos + "PASSWORD".len()..])?;
    Ok(AuthStmt::AlterUserPassword { name, password })
}

/// Parse a single-quoted string literal starting somewhere in `s` (the first
/// `'` found), supporting a doubled `''` as an escaped literal quote
/// (SQL-standard string-literal escaping). Returns the unescaped contents.
fn parse_quoted_password(s: &str) -> Result<String> {
    let quote_start = s.find('\'').ok_or_else(|| {
        DbError::SqlParse("CREATE USER: PASSWORD clause must be a quoted string".into())
    })?;
    let rest = &s[quote_start + 1..];
    let mut out = String::new();
    let mut chars = rest.char_indices();
    let mut closed = false;
    while let Some((i, c)) = chars.next() {
        if c == '\'' {
            if rest[i + 1..].starts_with('\'') {
                out.push('\'');
                chars.next(); // consume the second quote of the doubled escape
            } else {
                closed = true;
                break;
            }
        } else {
            out.push(c);
        }
    }
    if !closed {
        return Err(DbError::SqlParse(
            "CREATE USER: unterminated PASSWORD string".into(),
        ));
    }
    if out.is_empty() {
        return Err(DbError::SqlParse(
            "CREATE USER: PASSWORD cannot be empty".into(),
        ));
    }
    Ok(out)
}

/// `CREATE POLICY <name> ON <table> FOR <op> [TO <role,...>] USING (<predicate>) [WITH CHECK (<expr>)]`
///
/// The USING clause may span multiple tokens (it is a SQL expression); we
/// find `USING` case-insensitively and take everything after `(` through the
/// final `)` as the raw predicate string. The optional `TO <role,...>`
/// clause (item 122, B4) is detected as an exact `TO` token strictly between
/// the operation (or the table name, if `FOR` was omitted) and `USING` — so
/// it can never be confused with a `TO` that might appear inside the
/// predicate text itself, which lives after `USING` and is never tokenised
/// this way.
fn parse_create_policy(sql: &str) -> Result<AuthStmt> {
    let upper = sql.to_ascii_uppercase();
    // Tokenised version for the keyword positions.
    let toks: Vec<&str> = sql.split_whitespace().collect();
    let upper_toks: Vec<String> = toks.iter().map(|t| t.to_ascii_uppercase()).collect();

    // Name is toks[2].
    let name = ident(toks.get(2))?;

    // ON position.
    let on_pos = upper_toks
        .iter()
        .position(|t| t == "ON")
        .ok_or_else(|| DbError::SqlParse("CREATE POLICY: missing ON clause".into()))?;
    let table = ident(toks.get(on_pos + 1))?;

    // FOR position (optional — defaults to ALL).
    let for_pos = upper_toks.iter().position(|t| t == "FOR");
    let op = if let Some(for_pos) = for_pos {
        let op_str = toks.get(for_pos + 1).ok_or_else(|| {
            DbError::SqlParse("CREATE POLICY: missing operation after FOR".into())
        })?;
        PolicyOp::parse(op_str).ok_or_else(|| {
            DbError::SqlParse(format!("CREATE POLICY: unknown operation '{op_str}'"))
        })?
    } else {
        PolicyOp::All
    };

    // Optional `TO <role,...>` clause (item 122, B4): an exact `TO` token
    // somewhere between the operation (or table, if `FOR` absent) and the
    // token that opens the `USING` clause. `toks[i]`/`upper_toks[i]` share
    // indices and every `toks[i]` borrows from `sql`, so pointer arithmetic
    // recovers the exact (non-uppercased) byte range of the role list
    // without re-scanning — no `unsafe` needed, this is plain integer math
    // over already-safe `as_ptr()` values.
    let using_tok_idx = upper_toks
        .iter()
        .position(|t| t.starts_with("USING"))
        .ok_or_else(|| DbError::SqlParse("CREATE POLICY: missing USING clause".into()))?;
    let to_search_start = match for_pos {
        Some(for_pos) => for_pos + 2, // token right after the operation
        None => on_pos + 2,           // token right after the table name
    };
    let target_roles = match (to_search_start..using_tok_idx)
        .find(|&i| upper_toks.get(i).map(String::as_str) == Some("TO"))
    {
        Some(to_idx) => {
            let role_start = tok_offset(sql, toks[to_idx]) + toks[to_idx].len();
            let role_end = tok_offset(sql, toks[using_tok_idx]);
            let roles: Vec<String> = sql[role_start..role_end]
                .split(',')
                .map(|r| r.trim().trim_matches('"').to_string())
                .filter(|r| !r.is_empty())
                .collect();
            if roles.is_empty() {
                return Err(DbError::SqlParse(
                    "CREATE POLICY: TO clause requires at least one role".into(),
                ));
            }
            roles
        }
        None => Vec::new(),
    };

    // USING clause: everything between the outermost `(` and `)` after the
    // USING keyword.
    let using_kw_pos = upper
        .find("USING")
        .ok_or_else(|| DbError::SqlParse("CREATE POLICY: missing USING clause".into()))?;
    let after_using = &sql[using_kw_pos + 5..]; // skip "USING"
    let open = after_using.find('(').ok_or_else(|| {
        DbError::SqlParse("CREATE POLICY: USING clause must be parenthesised".into())
    })?;
    let inner = &after_using[open + 1..];
    // Walk to find the matching close paren (depth-aware).
    let mut depth = 1usize;
    let mut close = None;
    for (i, ch) in inner.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }
    let close = close
        .ok_or_else(|| DbError::SqlParse("CREATE POLICY: unclosed USING parenthesis".into()))?;
    let using_expr = inner[..close].trim().to_string();
    if using_expr.is_empty() {
        return Err(DbError::SqlParse(
            "CREATE POLICY: USING predicate cannot be empty".into(),
        ));
    }

    // Optional `WITH CHECK (expr)` after the USING clause (item-24 R-a).
    // We look in the portion of the original SQL that comes *after* the
    // closing `)` of the USING expression.
    let after_using_close_offset = (using_kw_pos + 5) + (open + 1) + close + 1;
    let remainder = if after_using_close_offset < sql.len() {
        &sql[after_using_close_offset..]
    } else {
        ""
    };
    let upper_rem = remainder.to_ascii_uppercase();
    let with_check_sql = if let Some(wc_pos) = upper_rem.find("WITH CHECK") {
        let after_wc = &remainder[wc_pos + 10..]; // skip "WITH CHECK"
        let wc_open = after_wc.find('(').ok_or_else(|| {
            DbError::SqlParse("CREATE POLICY: WITH CHECK clause must be parenthesised".into())
        })?;
        let wc_inner = &after_wc[wc_open + 1..];
        let mut depth = 1usize;
        let mut wc_close = None;
        for (i, ch) in wc_inner.char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        wc_close = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let wc_close = wc_close.ok_or_else(|| {
            DbError::SqlParse("CREATE POLICY: unclosed WITH CHECK parenthesis".into())
        })?;
        let expr = wc_inner[..wc_close].trim().to_string();
        if expr.is_empty() {
            return Err(DbError::SqlParse(
                "CREATE POLICY: WITH CHECK predicate cannot be empty".into(),
            ));
        }
        Some(expr)
    } else {
        None
    };

    Ok(AuthStmt::CreatePolicy(PolicyDef {
        name,
        table,
        op,
        using_expr,
        with_check_sql,
        target_roles,
    }))
}

/// Byte offset of `tok` within `sql`, given `tok` is itself a substring of
/// `sql` (true for every element of `sql.split_whitespace().collect()`).
/// Plain pointer-to-integer arithmetic — safe, no dereference — used to
/// recover exact (non-uppercased) source ranges from token positions found
/// via an uppercased token list.
fn tok_offset(sql: &str, tok: &str) -> usize {
    tok.as_ptr() as usize - sql.as_ptr() as usize
}

/// `DROP POLICY <name> ON <table>`
fn parse_drop_policy(toks: &[&str]) -> Result<AuthStmt> {
    let name = ident(toks.get(2))?;
    let upper_toks: Vec<String> = toks.iter().map(|t| t.to_ascii_uppercase()).collect();
    let on_pos = upper_toks
        .iter()
        .position(|t| t == "ON")
        .ok_or_else(|| DbError::SqlParse("DROP POLICY: missing ON clause".into()))?;
    let table = ident(toks.get(on_pos + 1))?;
    Ok(AuthStmt::DropPolicy { name, table })
}

fn ident(tok: Option<&&str>) -> Result<String> {
    tok.map(|s| s.trim_matches('"').to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| DbError::SqlParse("auth statement: expected an identifier".into()))
}

/// `GRANT <priv,..|ALL> ON <table> TO <grantee>` /
/// `GRANT <role> TO <grantee>` (and the REVOKE ... FROM forms).
fn parse_grant_revoke(toks: &[&str], grant: bool) -> Result<AuthStmt> {
    // Find the ON / TO|FROM anchors.
    let upper: Vec<String> = toks.iter().map(|t| t.to_ascii_uppercase()).collect();
    let connector = if grant { "TO" } else { "FROM" };
    let conn_pos = upper
        .iter()
        .position(|t| t == connector)
        .ok_or_else(|| DbError::SqlParse(format!("GRANT/REVOKE: missing '{connector}' clause")))?;
    let grantee = ident(toks.get(conn_pos + 1))?;

    if let Some(on_pos) = upper.iter().position(|t| t == "ON") {
        // Table privileges: tokens[1..on_pos] are the privilege list, plus
        // (item 112) an optional parenthesised column list, e.g.
        // `SELECT (email, name)` or `SELECT, UPDATE (a, b)`.
        let table = ident(toks.get(on_pos + 1))?;
        let priv_str: String = toks[1..on_pos].join(" ");
        let (privs, columns) = parse_priv_section(&priv_str)?;
        Ok(if grant {
            AuthStmt::GrantPrivs {
                privs,
                table,
                grantee,
                columns,
            }
        } else {
            AuthStmt::RevokePrivs {
                privs,
                table,
                grantee,
                columns,
            }
        })
    } else {
        // Role membership: `GRANT <role> TO <grantee>`.
        let role = ident(toks.get(1))?;
        Ok(if grant {
            AuthStmt::GrantRole { role, grantee }
        } else {
            AuthStmt::RevokeRole { role, grantee }
        })
    }
}

/// Parse the privilege-list segment between the leading `GRANT`/`REVOKE`
/// keyword and the `ON` keyword: `<priv,..|ALL> [(<col>, ...)]` (item 112).
/// The optional parenthesised column list narrows (or, for a GRANT, is the
/// scope of) every privilege in the list identically — Postgres allows a
/// distinct column list per privilege via repeated grant clauses (`GRANT
/// SELECT (a), UPDATE (b) ON ...`); unidb's hand-rolled grammar instead
/// covers the single-privilege-group case the backlog spec and every
/// acceptance scenario actually exercise (`GRANT SELECT (a, b) ON ...`,
/// `GRANT UPDATE (c) ON ...`). Returns `columns: None` when no parenthesised
/// list is present (the pre-112 whole-table grammar, unchanged).
fn parse_priv_section(s: &str) -> Result<(Vec<Privilege>, Option<Vec<String>>)> {
    let s = s.trim();
    match s.find('(') {
        None => Ok((parse_priv_list(s)?, None)),
        Some(open) => {
            let priv_part = s[..open].trim();
            let rest = &s[open + 1..];
            let close = rest
                .find(')')
                .ok_or_else(|| DbError::SqlParse("GRANT/REVOKE: unclosed column list".into()))?;
            let trailing = rest[close + 1..].trim();
            if !trailing.is_empty() {
                return Err(DbError::SqlParse(
                    "GRANT/REVOKE: unexpected tokens after column list".into(),
                ));
            }
            let cols: Vec<String> = rest[..close]
                .split(',')
                .map(|c| c.trim().trim_matches('"').to_string())
                .filter(|c| !c.is_empty())
                .collect();
            if cols.is_empty() {
                return Err(DbError::SqlParse(
                    "GRANT/REVOKE: column list cannot be empty".into(),
                ));
            }
            Ok((parse_priv_list(priv_part)?, Some(cols)))
        }
    }
}

fn parse_priv_list(s: &str) -> Result<Vec<Privilege>> {
    if s.trim().eq_ignore_ascii_case("ALL") {
        return Ok(Privilege::all().to_vec());
    }
    let mut out = Vec::new();
    for part in s.split(',') {
        let p = part.trim();
        out.push(
            Privilege::parse(p)
                .ok_or_else(|| DbError::SqlParse(format!("unknown privilege '{p}'")))?,
        );
    }
    if out.is_empty() {
        return Err(DbError::SqlParse("GRANT: empty privilege list".into()));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn parse_auth_ddl() {
        assert_eq!(
            parse_auth_stmt("CREATE USER alice SUPERUSER").unwrap(),
            Some(AuthStmt::CreateUser {
                name: "alice".into(),
                superuser: true,
                password: None,
            })
        );
        assert_eq!(
            parse_auth_stmt("GRANT SELECT, INSERT ON accounts TO bob").unwrap(),
            Some(AuthStmt::GrantPrivs {
                privs: vec![Privilege::Select, Privilege::Insert],
                table: "accounts".into(),
                grantee: "bob".into(),
                columns: None,
            })
        );
        assert_eq!(
            parse_auth_stmt("GRANT analyst TO bob").unwrap(),
            Some(AuthStmt::GrantRole {
                role: "analyst".into(),
                grantee: "bob".into()
            })
        );
        assert_eq!(
            parse_auth_stmt("REVOKE ALL ON accounts FROM bob").unwrap(),
            Some(AuthStmt::RevokePrivs {
                privs: Privilege::all().to_vec(),
                table: "accounts".into(),
                grantee: "bob".into(),
                columns: None,
            })
        );
        assert!(parse_auth_stmt("SELECT * FROM t").unwrap().is_none());
    }

    #[test]
    fn privilege_resolution_through_roles() {
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        store
            .apply(&AuthStmt::CreateUser {
                name: "bob".into(),
                superuser: false,
                password: None,
            })
            .unwrap();
        store
            .apply(&AuthStmt::CreateRole("analyst".into()))
            .unwrap();
        store
            .apply(&AuthStmt::GrantPrivs {
                privs: vec![Privilege::Select],
                table: "accounts".into(),
                grantee: "analyst".into(),
                columns: None,
            })
            .unwrap();
        // Bob has nothing yet.
        assert!(!store.has_privilege("bob", "accounts", Privilege::Select));
        // Grant the role → bob inherits SELECT.
        store
            .apply(&AuthStmt::GrantRole {
                role: "analyst".into(),
                grantee: "bob".into(),
            })
            .unwrap();
        assert!(store.has_privilege("bob", "accounts", Privilege::Select));
        assert!(!store.has_privilege("bob", "accounts", Privilege::Insert));
    }

    #[test]
    fn superuser_has_everything_and_persists() {
        let dir = tempdir().unwrap();
        {
            let store = RoleStore::open(dir.path()).unwrap();
            store
                .apply(&AuthStmt::CreateUser {
                    name: "root".into(),
                    superuser: true,
                    password: None,
                })
                .unwrap();
            assert!(store.has_privilege("root", "anything", Privilege::Delete));
        }
        // Reopen: the user persists.
        let store = RoleStore::open(dir.path()).unwrap();
        assert!(store.is_superuser("root"));
    }

    #[test]
    fn parse_create_user_with_password() {
        assert_eq!(
            parse_auth_stmt("CREATE USER alice PASSWORD 'hunter2'").unwrap(),
            Some(AuthStmt::CreateUser {
                name: "alice".into(),
                superuser: false,
                password: Some("hunter2".into()),
            })
        );
        // SUPERUSER + PASSWORD together, in that order.
        assert_eq!(
            parse_auth_stmt("CREATE USER root SUPERUSER PASSWORD 's3cret'").unwrap(),
            Some(AuthStmt::CreateUser {
                name: "root".into(),
                superuser: true,
                password: Some("s3cret".into()),
            })
        );
        // A password containing spaces and an escaped quote.
        assert_eq!(
            parse_auth_stmt("CREATE USER bob PASSWORD 'a b ''c'' d'").unwrap(),
            Some(AuthStmt::CreateUser {
                name: "bob".into(),
                superuser: false,
                password: Some("a b 'c' d".into()),
            })
        );
        // Malformed clauses are rejected rather than silently ignored.
        assert!(parse_auth_stmt("CREATE USER alice PASSWORD").is_err());
        assert!(parse_auth_stmt("CREATE USER alice PASSWORD ''").is_err());
        assert!(parse_auth_stmt("CREATE USER alice PASSWORD 'unterminated").is_err());
    }

    #[test]
    fn credential_store_hash_verify_and_persist() {
        let dir = tempdir().unwrap();
        {
            let store = RoleStore::open(dir.path()).unwrap();
            store
                .apply(&AuthStmt::CreateUser {
                    name: "carol".into(),
                    superuser: false,
                    password: Some("correct horse".into()),
                })
                .unwrap();
            // Correct password verifies.
            assert!(store.verify_password("carol", "correct horse"));
            // Wrong password fails.
            assert!(!store.verify_password("carol", "wrong password"));
            // Unknown user fails the same way (no panic, no special-case).
            assert!(!store.verify_password("nobody", "correct horse"));
            // A user with no stored credential (created without PASSWORD)
            // cannot log in via password at all.
            store
                .apply(&AuthStmt::CreateUser {
                    name: "dave".into(),
                    superuser: false,
                    password: None,
                })
                .unwrap();
            assert!(!store.verify_password("dave", ""));
            assert!(!store.verify_password("dave", "anything"));
        }
        // The credential survives a reopen (it's persisted control-plane
        // metadata, same file as users/roles/grants).
        let store = RoleStore::open(dir.path()).unwrap();
        assert!(store.verify_password("carol", "correct horse"));

        // The persisted roles.json is never plaintext: the raw file bytes
        // must not contain the password, only a PHC-formatted argon2id hash.
        let raw = std::fs::read_to_string(dir.path().join("roles.json")).unwrap();
        assert!(!raw.contains("correct horse"));
        assert!(raw.contains("$argon2id$"));
    }

    #[test]
    fn set_password_rust_api() {
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        store
            .apply(&AuthStmt::CreateUser {
                name: "erin".into(),
                superuser: false,
                password: None,
            })
            .unwrap();
        assert!(!store.verify_password("erin", "new-pw"));
        store.set_password("erin", "new-pw").unwrap();
        assert!(store.verify_password("erin", "new-pw"));
        assert!(!store.verify_password("erin", "wrong"));

        // Setting a password for a nonexistent user is an error.
        assert!(store.set_password("ghost", "x").is_err());
    }

    #[test]
    fn auth_state_debug_never_prints_password_hash() {
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        store
            .apply(&AuthStmt::CreateUser {
                name: "frank".into(),
                superuser: false,
                password: Some("super-secret-value".into()),
            })
            .unwrap();
        let debug_str = format!("{:?}", store.lock());
        assert!(!debug_str.contains("super-secret-value"));
        assert!(!debug_str.contains("$argon2id$"));
        assert!(debug_str.contains("redacted"));

        // The AuthStmt itself must also never Debug-print the plaintext
        // (e.g. via `tracing::info!(?stmt, ..)` in `apply`).
        let stmt = AuthStmt::CreateUser {
            name: "gina".into(),
            superuser: false,
            password: Some("another-secret".into()),
        };
        let stmt_debug = format!("{stmt:?}");
        assert!(!stmt_debug.contains("another-secret"));
        assert!(stmt_debug.contains("redacted"));
    }

    // ── item 121, A4: refresh-token sessions ───────────────────────────────

    #[test]
    fn create_session_returns_high_entropy_token_and_verifies() {
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        store
            .apply(&AuthStmt::CreateUser {
                name: "hank".into(),
                superuser: false,
                password: None,
            })
            .unwrap();

        let (raw, expires_at) = store.create_session("hank").unwrap();
        // 32 bytes hex-encoded = 64 hex chars.
        assert_eq!(raw.len(), 64);
        assert!(expires_at > now_secs());
        assert_eq!(store.verify_session(&raw), Some("hank".to_string()));

        // Two sessions for the same user never collide.
        let (raw2, _) = store.create_session("hank").unwrap();
        assert_ne!(raw, raw2);
        assert_eq!(store.verify_session(&raw2), Some("hank".to_string()));

        // The raw token never appears in the persisted file — only its hash.
        let on_disk = std::fs::read_to_string(dir.path().join("roles.json")).unwrap();
        assert!(!on_disk.contains(&raw));
        assert!(on_disk.contains(&sha256_hex(&raw)));
    }

    #[test]
    fn verify_session_rejects_unknown_and_garbage_tokens() {
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        assert_eq!(store.verify_session("not-a-real-token"), None);
        assert_eq!(store.verify_session(""), None);
    }

    #[test]
    fn rotate_session_revokes_old_and_issues_new_for_same_user() {
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        store
            .apply(&AuthStmt::CreateUser {
                name: "iris".into(),
                superuser: false,
                password: None,
            })
            .unwrap();
        let (raw, _) = store.create_session("iris").unwrap();

        let (username, new_raw, _expires_at) = store
            .rotate_session(&raw)
            .unwrap()
            .expect("valid session must rotate");
        assert_eq!(username, "iris");
        assert_ne!(new_raw, raw);

        // The old token no longer verifies (revoked); the new one does.
        assert_eq!(store.verify_session(&raw), None);
        assert_eq!(store.verify_session(&new_raw), Some("iris".to_string()));

        // Rotating the now-revoked old token again fails uniformly (None,
        // not an error) — same shape as an unknown token.
        assert!(store.rotate_session(&raw).unwrap().is_none());
    }

    #[test]
    fn rotate_session_rejects_unknown_token() {
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        assert!(store.rotate_session("garbage").unwrap().is_none());
    }

    #[test]
    fn revoke_session_is_idempotent_and_blocks_further_use() {
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        store
            .apply(&AuthStmt::CreateUser {
                name: "jack".into(),
                superuser: false,
                password: None,
            })
            .unwrap();
        let (raw, _) = store.create_session("jack").unwrap();
        assert_eq!(store.verify_session(&raw), Some("jack".to_string()));

        store.revoke_session(&raw).unwrap();
        assert_eq!(store.verify_session(&raw), None);

        // Revoking again (already revoked) and revoking an unknown token are
        // both silent no-ops, never an error.
        assert!(store.revoke_session(&raw).is_ok());
        assert!(store.revoke_session("never-issued").is_ok());
    }

    #[test]
    fn expired_session_fails_verification() {
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        store
            .apply(&AuthStmt::CreateUser {
                name: "karen".into(),
                superuser: false,
                password: None,
            })
            .unwrap();
        let (raw, _) = store.create_session("karen").unwrap();
        // Reach into the persisted state and force the expiry into the past
        // (avoids a real-time sleep in a unit test).
        {
            let mut st = store.lock();
            for rec in st.sessions.values_mut() {
                rec.expires_at = now_secs().saturating_sub(1);
            }
            store.persist(&st).unwrap();
        }
        assert_eq!(store.verify_session(&raw), None);
        assert!(store.rotate_session(&raw).unwrap().is_none());
    }

    #[test]
    fn sessions_survive_reopen() {
        let dir = tempdir().unwrap();
        let raw = {
            let store = RoleStore::open(dir.path()).unwrap();
            store
                .apply(&AuthStmt::CreateUser {
                    name: "laura".into(),
                    superuser: false,
                    password: None,
                })
                .unwrap();
            store.create_session("laura").unwrap().0
        };
        let store = RoleStore::open(dir.path()).unwrap();
        assert_eq!(store.verify_session(&raw), Some("laura".to_string()));
    }

    #[test]
    fn auth_state_debug_never_prints_session_detail() {
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        store
            .apply(&AuthStmt::CreateUser {
                name: "mallory".into(),
                superuser: false,
                password: None,
            })
            .unwrap();
        let (raw, _) = store.create_session("mallory").unwrap();
        let debug_str = format!("{:?}", store.lock());
        // The raw refresh token (the actual secret) must never appear.
        assert!(!debug_str.contains(&raw));
        // The session's hash-keyed detail is redacted to a count — note
        // "mallory" itself legitimately still appears via the unrelated
        // `users` field (usernames aren't secret), so this only checks the
        // `sessions` field specifically prints a redacted count.
        assert!(debug_str.contains("sessions"));
        assert!(debug_str.contains("redacted"));
    }

    // ── item 122, B3/B4: built-in roles + role-scoped policies ────────────

    #[test]
    fn reserved_roles_cannot_be_created_or_dropped() {
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        for name in ["anon", "authenticated", "service_role"] {
            assert!(
                store.apply(&AuthStmt::CreateRole(name.into())).is_err(),
                "CREATE ROLE {name} must be rejected (reserved)"
            );
            assert!(
                store.apply(&AuthStmt::DropRole(name.into())).is_err(),
                "DROP ROLE {name} must be rejected (reserved)"
            );
        }
        // A non-reserved role still works normally.
        assert!(store.apply(&AuthStmt::CreateRole("analyst".into())).is_ok());
    }

    #[test]
    fn parse_create_policy_with_to_clause() {
        let stmt = parse_auth_stmt(
            "CREATE POLICY p1 ON accounts FOR SELECT TO authenticated USING (owner = current_user())",
        )
        .unwrap()
        .unwrap();
        match stmt {
            AuthStmt::CreatePolicy(p) => {
                assert_eq!(p.target_roles, vec!["authenticated".to_string()]);
                assert_eq!(p.using_expr, "owner = current_user()");
                assert_eq!(p.op, PolicyOp::Select);
            }
            _ => panic!("expected CreatePolicy"),
        }
    }

    #[test]
    fn parse_create_policy_with_multi_role_to_clause() {
        let stmt =
            parse_auth_stmt("CREATE POLICY p2 ON accounts FOR ALL TO admin, analyst USING (true)")
                .unwrap()
                .unwrap();
        match stmt {
            AuthStmt::CreatePolicy(p) => {
                assert_eq!(
                    p.target_roles,
                    vec!["admin".to_string(), "analyst".to_string()]
                );
            }
            _ => panic!("expected CreatePolicy"),
        }
    }

    #[test]
    fn parse_create_policy_to_clause_with_with_check() {
        // TO clause + WITH CHECK together (both new/optional clauses at once).
        let stmt = parse_auth_stmt(
            "CREATE POLICY p4 ON accounts FOR UPDATE TO authenticated USING (owner = current_user()) WITH CHECK (owner = current_user())",
        )
        .unwrap()
        .unwrap();
        match stmt {
            AuthStmt::CreatePolicy(p) => {
                assert_eq!(p.target_roles, vec!["authenticated".to_string()]);
                assert_eq!(p.with_check_sql.as_deref(), Some("owner = current_user()"));
            }
            _ => panic!("expected CreatePolicy"),
        }
    }

    #[test]
    fn parse_create_policy_without_to_clause_is_unscoped() {
        let stmt = parse_auth_stmt("CREATE POLICY p3 ON accounts FOR SELECT USING (true)")
            .unwrap()
            .unwrap();
        match stmt {
            AuthStmt::CreatePolicy(p) => assert!(p.target_roles.is_empty()),
            _ => panic!("expected CreatePolicy"),
        }
    }

    #[test]
    fn parse_create_policy_to_clause_requires_a_role() {
        // "TO USING" with nothing in between is a malformed TO clause — must
        // error, not silently produce an empty (i.e. unscoped/all-callers)
        // target-role list, which would be a fail-OPEN widening.
        assert!(
            parse_auth_stmt("CREATE POLICY p5 ON accounts FOR SELECT TO USING (true)").is_err()
        );
    }

    #[test]
    fn effective_roles_no_subject_is_anon() {
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        assert_eq!(
            store.effective_roles(None, &BTreeMap::new()),
            vec![ANON_ROLE.to_string()]
        );
    }

    #[test]
    fn effective_roles_subject_is_authenticated_plus_transitive_grants() {
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        store
            .apply(&AuthStmt::CreateUser {
                name: "bob".into(),
                superuser: false,
                password: None,
            })
            .unwrap();
        store
            .apply(&AuthStmt::CreateRole("analyst".into()))
            .unwrap();
        store
            .apply(&AuthStmt::CreateRole("senior_analyst".into()))
            .unwrap();
        store
            .apply(&AuthStmt::GrantRole {
                role: "analyst".into(),
                grantee: "bob".into(),
            })
            .unwrap();
        // Role-of-role: bob -> analyst -> senior_analyst, resolved transitively.
        store
            .apply(&AuthStmt::GrantRole {
                role: "senior_analyst".into(),
                grantee: "analyst".into(),
            })
            .unwrap();
        let mut roles = store.effective_roles(Some("bob"), &BTreeMap::new());
        roles.sort();
        assert_eq!(
            roles,
            vec![
                "analyst".to_string(),
                AUTHENTICATED_ROLE.to_string(),
                "senior_analyst".to_string(),
            ]
        );

        // A subject with no granted roles is still "authenticated", nothing more.
        store
            .apply(&AuthStmt::CreateUser {
                name: "carol".into(),
                superuser: false,
                password: None,
            })
            .unwrap();
        assert_eq!(
            store.effective_roles(Some("carol"), &BTreeMap::new()),
            vec![AUTHENTICATED_ROLE.to_string()]
        );
    }

    #[test]
    fn effective_roles_service_role_claim_wins_regardless_of_subject() {
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        let mut claims = BTreeMap::new();
        claims.insert(
            "role".to_string(),
            serde_json::Value::String(SERVICE_ROLE.to_string()),
        );
        assert_eq!(
            store.effective_roles(Some("bob"), &claims),
            vec![SERVICE_ROLE.to_string()]
        );
        assert_eq!(
            store.effective_roles(None, &claims),
            vec![SERVICE_ROLE.to_string()]
        );
        // A non-matching "role" claim doesn't trigger the bypass.
        let mut other_claims = BTreeMap::new();
        other_claims.insert(
            "role".to_string(),
            serde_json::Value::String("something_else".to_string()),
        );
        assert_eq!(
            store.effective_roles(Some("bob"), &other_claims),
            vec![AUTHENTICATED_ROLE.to_string()]
        );
    }

    #[test]
    fn effective_roles_explicit_anon_role_claim_overrides_subject() {
        // A `role: anon` claim (the real Supabase anon-key convention) is a
        // genuinely reachable way to get an `anon`-labeled caller that does
        // NOT trip the `user == None` superuser-bypass invariant (item 103) —
        // unlike a bare no-subject token, this carries a real `Some(subject)`
        // so it reaches the ordinary (non-bypass) RLS path in
        // `Engine::execute_sql_inner_as_principal`.
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        let mut claims = BTreeMap::new();
        claims.insert(
            "role".to_string(),
            serde_json::Value::String(ANON_ROLE.to_string()),
        );
        assert_eq!(
            store.effective_roles(Some("some_subject"), &claims),
            vec![ANON_ROLE.to_string()]
        );
        // The default (no `role` claim at all) mapping is unaffected: a bare
        // no-subject caller still resolves to anon too.
        assert_eq!(
            store.effective_roles(None, &BTreeMap::new()),
            vec![ANON_ROLE.to_string()]
        );
    }

    // ── item 112: column-level grants ──────────────────────────────────────

    #[test]
    fn parse_grant_with_column_list() {
        let stmt = parse_auth_stmt("GRANT SELECT (email, name) ON users TO support")
            .unwrap()
            .unwrap();
        assert_eq!(
            stmt,
            AuthStmt::GrantPrivs {
                privs: vec![Privilege::Select],
                table: "users".into(),
                grantee: "support".into(),
                columns: Some(vec!["email".into(), "name".into()]),
            }
        );
    }

    #[test]
    fn parse_revoke_with_column_list() {
        let stmt = parse_auth_stmt("REVOKE SELECT (email) ON users FROM support")
            .unwrap()
            .unwrap();
        assert_eq!(
            stmt,
            AuthStmt::RevokePrivs {
                privs: vec![Privilege::Select],
                table: "users".into(),
                grantee: "support".into(),
                columns: Some(vec!["email".into()]),
            }
        );
    }

    #[test]
    fn parse_grant_column_list_multi_privilege() {
        // A single column list applies uniformly to every privilege in the
        // list (documented grammar simplification — see `parse_priv_section`).
        let stmt = parse_auth_stmt("GRANT SELECT, UPDATE (a, b) ON t TO r")
            .unwrap()
            .unwrap();
        match stmt {
            AuthStmt::GrantPrivs { privs, columns, .. } => {
                assert_eq!(privs, vec![Privilege::Select, Privilege::Update]);
                assert_eq!(columns, Some(vec!["a".into(), "b".into()]));
            }
            _ => panic!("expected GrantPrivs"),
        }
    }

    #[test]
    fn parse_grant_column_list_rejects_empty_and_unclosed() {
        assert!(parse_auth_stmt("GRANT SELECT () ON t TO r").is_err());
        assert!(parse_auth_stmt("GRANT SELECT (a, b ON t TO r").is_err());
    }

    #[test]
    fn column_grant_narrows_and_widens_correctly() {
        use ColumnGrant as CG;
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        store
            .apply(&AuthStmt::CreateUser {
                name: "u".into(),
                superuser: false,
                password: None,
            })
            .unwrap();

        // No grant at all yet.
        assert_eq!(store.column_grant("u", "t", Privilege::Select), CG::None);
        assert!(!store.has_privilege("u", "t", Privilege::Select));

        // Column-scoped GRANT.
        store
            .apply(&AuthStmt::GrantPrivs {
                privs: vec![Privilege::Select],
                table: "t".into(),
                grantee: "u".into(),
                columns: Some(vec!["a".into(), "b".into()]),
            })
            .unwrap();
        assert_eq!(
            store.column_grant("u", "t", Privilege::Select),
            CG::Columns(["a".to_string(), "b".to_string()].into_iter().collect())
        );
        // Existence check still true (item 112: coarser than column check).
        assert!(store.has_privilege("u", "t", Privilege::Select));

        // A second column-scoped GRANT unions (never narrows).
        store
            .apply(&AuthStmt::GrantPrivs {
                privs: vec![Privilege::Select],
                table: "t".into(),
                grantee: "u".into(),
                columns: Some(vec!["c".into()]),
            })
            .unwrap();
        assert_eq!(
            store.column_grant("u", "t", Privilege::Select),
            CG::Columns(
                ["a".to_string(), "b".to_string(), "c".to_string()]
                    .into_iter()
                    .collect()
            )
        );

        // A whole-table GRANT explicitly widens to All.
        store
            .apply(&AuthStmt::GrantPrivs {
                privs: vec![Privilege::Select],
                table: "t".into(),
                grantee: "u".into(),
                columns: None,
            })
            .unwrap();
        assert_eq!(store.column_grant("u", "t", Privilege::Select), CG::All);

        // A column-scoped GRANT on top of an existing All grant is a no-op
        // (GRANT never narrows).
        store
            .apply(&AuthStmt::GrantPrivs {
                privs: vec![Privilege::Select],
                table: "t".into(),
                grantee: "u".into(),
                columns: Some(vec!["a".into()]),
            })
            .unwrap();
        assert_eq!(store.column_grant("u", "t", Privilege::Select), CG::All);

        // Column-scoped REVOKE narrows a column-scoped grant.
        store
            .apply(&AuthStmt::RevokePrivs {
                privs: vec![Privilege::Select],
                table: "t".into(),
                grantee: "u".into(),
                columns: None,
            })
            .unwrap();
        store
            .apply(&AuthStmt::GrantPrivs {
                privs: vec![Privilege::Select],
                table: "t".into(),
                grantee: "u".into(),
                columns: Some(vec!["a".into(), "b".into()]),
            })
            .unwrap();
        store
            .apply(&AuthStmt::RevokePrivs {
                privs: vec![Privilege::Select],
                table: "t".into(),
                grantee: "u".into(),
                columns: Some(vec!["a".into()]),
            })
            .unwrap();
        assert_eq!(
            store.column_grant("u", "t", Privilege::Select),
            CG::Columns(["b".to_string()].into_iter().collect())
        );

        // Revoking the last remaining column clears the privilege entirely
        // (no dangling "granted but zero columns" state).
        store
            .apply(&AuthStmt::RevokePrivs {
                privs: vec![Privilege::Select],
                table: "t".into(),
                grantee: "u".into(),
                columns: Some(vec!["b".into()]),
            })
            .unwrap();
        assert_eq!(store.column_grant("u", "t", Privilege::Select), CG::None);
        assert!(!store.has_privilege("u", "t", Privilege::Select));

        // Table-level revoke clears regardless of scope.
        store
            .apply(&AuthStmt::GrantPrivs {
                privs: vec![Privilege::Select],
                table: "t".into(),
                grantee: "u".into(),
                columns: Some(vec!["a".into()]),
            })
            .unwrap();
        store
            .apply(&AuthStmt::RevokePrivs {
                privs: vec![Privilege::Select],
                table: "t".into(),
                grantee: "u".into(),
                columns: None,
            })
            .unwrap();
        assert_eq!(store.column_grant("u", "t", Privilege::Select), CG::None);
    }

    #[test]
    fn column_grant_resolves_transitively_through_roles() {
        use ColumnGrant as CG;
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        store
            .apply(&AuthStmt::CreateUser {
                name: "bob".into(),
                superuser: false,
                password: None,
            })
            .unwrap();
        store
            .apply(&AuthStmt::CreateRole("support".into()))
            .unwrap();
        store
            .apply(&AuthStmt::GrantPrivs {
                privs: vec![Privilege::Select],
                table: "users".into(),
                grantee: "support".into(),
                columns: Some(vec!["email".into(), "name".into()]),
            })
            .unwrap();
        store
            .apply(&AuthStmt::GrantRole {
                role: "support".into(),
                grantee: "bob".into(),
            })
            .unwrap();
        assert_eq!(
            store.column_grant("bob", "users", Privilege::Select),
            CG::Columns(
                ["email".to_string(), "name".to_string()]
                    .into_iter()
                    .collect()
            )
        );
    }

    #[test]
    fn superuser_column_grant_is_all() {
        use ColumnGrant as CG;
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        store
            .apply(&AuthStmt::CreateUser {
                name: "root".into(),
                superuser: true,
                password: None,
            })
            .unwrap();
        assert_eq!(
            store.column_grant("root", "anything", Privilege::Select),
            CG::All
        );
    }

    #[test]
    fn legacy_whole_table_grant_json_deserializes_as_all() {
        // Simulates a pre-112 `roles.json`: `table_grants` values are a bare
        // JSON array of privilege names, e.g. `["Select","Insert"]`, not the
        // item-112 `{priv: scope}` object. Back-compat contract: this must
        // deserialize as if every listed privilege were `GrantScope::All`,
        // with no data migration.
        let dir = tempdir().unwrap();
        let legacy = serde_json::json!({
            "users": {"bob": false},
            "roles": [],
            "memberships": {},
            "table_grants": {
                "bob": {
                    "accounts": ["Select", "Insert"]
                }
            }
        });
        std::fs::write(
            dir.path().join("roles.json"),
            serde_json::to_vec_pretty(&legacy).unwrap(),
        )
        .unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        assert!(store.has_privilege("bob", "accounts", Privilege::Select));
        assert!(store.has_privilege("bob", "accounts", Privilege::Insert));
        assert!(!store.has_privilege("bob", "accounts", Privilege::Update));
        assert_eq!(
            store.column_grant("bob", "accounts", Privilege::Select),
            ColumnGrant::All
        );

        // A round-trip (re-persist under item-112 logic, then reopen) upgrades
        // the on-disk shape but preserves behavior — no explicit migration
        // step required by an operator.
        store
            .apply(&AuthStmt::GrantPrivs {
                privs: vec![Privilege::Delete],
                table: "accounts".into(),
                grantee: "bob".into(),
                columns: None,
            })
            .unwrap();
        let store2 = RoleStore::open(dir.path()).unwrap();
        assert!(store2.has_privilege("bob", "accounts", Privilege::Select));
        assert!(store2.has_privilege("bob", "accounts", Privilege::Delete));
    }

    // ── item 127 (Workstream D4): TOTP MFA ─────────────────────────────────

    /// RFC 4226 Appendix D's official HOTP test vectors (secret =
    /// `"12345678901234567890"`, ASCII, 20 bytes) — proves [`hotp`] against a
    /// reference implementation, not just self-consistency.
    #[test]
    fn hotp_matches_rfc4226_test_vectors() {
        let secret = b"12345678901234567890";
        let expected: [u32; 10] = [
            755224, 287082, 359152, 969429, 338314, 254676, 287922, 162583, 399871, 520489,
        ];
        for (counter, &want) in expected.iter().enumerate() {
            assert_eq!(
                hotp(secret, counter as u64),
                Some(want),
                "counter={counter}"
            );
        }
    }

    #[test]
    fn base32_round_trips_arbitrary_bytes() {
        for len in [0usize, 1, 5, 10, 20, 33] {
            let mut bytes = vec![0u8; len];
            OsRng.fill_bytes(&mut bytes);
            let encoded = base32_encode(&bytes);
            // No padding characters, uppercase RFC 4648 alphabet only.
            assert!(!encoded.contains('='));
            let decoded = base32_decode(&encoded).expect("round-trip decode must succeed");
            assert_eq!(decoded, bytes, "len={len}");
        }
    }

    #[test]
    fn base32_decode_is_case_insensitive_and_rejects_garbage() {
        let bytes = b"hello unidb!";
        let encoded = base32_encode(bytes);
        assert_eq!(base32_decode(&encoded.to_ascii_lowercase()).unwrap(), bytes);
        assert!(base32_decode("not valid base32!!!").is_none());
    }

    /// Compute the *current* valid TOTP code straight from a base32 secret —
    /// the deterministic "derive the expected code" approach the task spec
    /// asks for, rather than sleeping across a 30 s step boundary.
    fn current_totp_code(secret_base32: &str) -> String {
        let secret = base32_decode(secret_base32).unwrap();
        let step = now_secs() / TOTP_STEP_SECS;
        totp_at_step(&secret, step).unwrap()
    }

    fn setup_user_for_mfa(store: &RoleStore, name: &str) {
        store
            .apply(&AuthStmt::CreateUser {
                name: name.into(),
                superuser: false,
                password: Some(format!("{name}-pw")),
            })
            .unwrap();
    }

    #[test]
    fn mfa_enroll_then_confirm_enables_and_returns_recovery_codes() {
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        setup_user_for_mfa(&store, "alice");

        assert!(!store.mfa_enabled("alice"));
        let (secret_b32, uri) = store.mfa_enroll("alice").unwrap();
        assert!(uri.starts_with("otpauth://totp/unidb:alice?secret="));
        assert!(uri.contains(&secret_b32));
        assert!(uri.contains("issuer=unidb"));
        // Still not enabled — enrollment is pending until confirmed.
        assert!(!store.mfa_enabled("alice"));

        let code = current_totp_code(&secret_b32);
        let recovery_codes = store
            .mfa_confirm("alice", &code)
            .unwrap()
            .expect("a valid current code must confirm enrollment");
        assert_eq!(recovery_codes.len(), RECOVERY_CODE_COUNT);
        // Every recovery code is unique.
        let unique: BTreeSet<_> = recovery_codes.iter().collect();
        assert_eq!(unique.len(), RECOVERY_CODE_COUNT);
        assert!(store.mfa_enabled("alice"));

        // Persists across reopen.
        let store2 = RoleStore::open(dir.path()).unwrap();
        assert!(store2.mfa_enabled("alice"));

        // The plaintext secret and recovery codes never land in roles.json.
        let raw = std::fs::read_to_string(dir.path().join("roles.json")).unwrap();
        for rc in &recovery_codes {
            assert!(!raw.contains(rc));
        }
    }

    #[test]
    fn mfa_confirm_wrong_code_returns_none_and_stays_pending() {
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        setup_user_for_mfa(&store, "bob");
        store.mfa_enroll("bob").unwrap();

        assert_eq!(store.mfa_confirm("bob", "000000").unwrap(), None);
        assert!(!store.mfa_enabled("bob"));
    }

    #[test]
    fn mfa_confirm_without_enrollment_is_an_error() {
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        setup_user_for_mfa(&store, "carol");
        assert!(store.mfa_confirm("carol", "123456").is_err());
    }

    #[test]
    fn mfa_replay_of_same_code_is_rejected() {
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        setup_user_for_mfa(&store, "dave");
        let (secret_b32, _) = store.mfa_enroll("dave").unwrap();
        let code = current_totp_code(&secret_b32);
        store.mfa_confirm("dave", &code).unwrap().unwrap();

        // The exact same code/step, presented again via a fresh challenge,
        // must be rejected — replay protection via `last_used_step`.
        let (challenge, _) = store.create_mfa_challenge("dave").unwrap();
        assert_eq!(store.verify_mfa_challenge(&challenge, &code).unwrap(), None);
    }

    #[test]
    fn mfa_login_challenge_succeeds_with_valid_code_and_is_single_use() {
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        setup_user_for_mfa(&store, "erin");
        let (secret_b32, _) = store.mfa_enroll("erin").unwrap();
        let enroll_code = current_totp_code(&secret_b32);
        store.mfa_confirm("erin", &enroll_code).unwrap().unwrap();

        let (challenge, expires_at) = store.create_mfa_challenge("erin").unwrap();
        assert!(expires_at > now_secs());

        // Wrong code fails, challenge remains usable.
        assert_eq!(
            store.verify_mfa_challenge(&challenge, "000000").unwrap(),
            None
        );

        // A fresh valid code (next un-replayed step) succeeds.
        let secret_bytes = base32_decode(&secret_b32).unwrap();
        let next_step = (now_secs() / TOTP_STEP_SECS) as i64 + 1;
        let fresh_code = totp_at_step(&secret_bytes, next_step as u64).unwrap();
        let owner = store.verify_mfa_challenge(&challenge, &fresh_code).unwrap();
        assert_eq!(owner, Some("erin".to_string()));

        // The same challenge cannot be redeemed a second time even with a
        // fresh, still-valid code (single-use).
        let next_step2 = next_step + 1;
        let fresh_code2 = totp_at_step(&secret_bytes, next_step2 as u64).unwrap();
        assert_eq!(
            store
                .verify_mfa_challenge(&challenge, &fresh_code2)
                .unwrap(),
            None
        );
    }

    #[test]
    fn mfa_challenge_unknown_or_garbage_token_returns_none() {
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        assert_eq!(
            store
                .verify_mfa_challenge("not-a-real-challenge", "123456")
                .unwrap(),
            None
        );
    }

    #[test]
    fn mfa_recovery_code_works_once_then_is_consumed() {
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        setup_user_for_mfa(&store, "frank");
        let (secret_b32, _) = store.mfa_enroll("frank").unwrap();
        let enroll_code = current_totp_code(&secret_b32);
        let recovery_codes = store.mfa_confirm("frank", &enroll_code).unwrap().unwrap();
        let rc = recovery_codes[0].clone();

        let (challenge1, _) = store.create_mfa_challenge("frank").unwrap();
        let owner = store.verify_mfa_challenge(&challenge1, &rc).unwrap();
        assert_eq!(owner, Some("frank".to_string()));

        // The same recovery code cannot be used again, even against a brand
        // new challenge.
        let (challenge2, _) = store.create_mfa_challenge("frank").unwrap();
        assert_eq!(store.verify_mfa_challenge(&challenge2, &rc).unwrap(), None);

        // The remaining, unused recovery codes still work.
        let (challenge3, _) = store.create_mfa_challenge("frank").unwrap();
        let owner2 = store
            .verify_mfa_challenge(&challenge3, &recovery_codes[1])
            .unwrap();
        assert_eq!(owner2, Some("frank".to_string()));
    }

    #[test]
    fn mfa_disable_requires_valid_code_unless_superuser_bypass() {
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        setup_user_for_mfa(&store, "gina");
        let (secret_b32, _) = store.mfa_enroll("gina").unwrap();
        let enroll_code = current_totp_code(&secret_b32);
        store.mfa_confirm("gina", &enroll_code).unwrap().unwrap();
        assert!(store.mfa_enabled("gina"));

        // Wrong code, no bypass: rejected, MFA stays enabled.
        assert!(!store.mfa_disable("gina", Some("000000"), false).unwrap());
        assert!(store.mfa_enabled("gina"));

        // No code at all, no bypass: rejected.
        assert!(!store.mfa_disable("gina", None, false).unwrap());
        assert!(store.mfa_enabled("gina"));

        // Superuser bypass disables unconditionally, no code needed.
        assert!(store.mfa_disable("gina", None, true).unwrap());
        assert!(!store.mfa_enabled("gina"));
    }

    #[test]
    fn mfa_disable_with_valid_totp_code_succeeds() {
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        setup_user_for_mfa(&store, "henry");
        let (secret_b32, _) = store.mfa_enroll("henry").unwrap();
        let enroll_code = current_totp_code(&secret_b32);
        store.mfa_confirm("henry", &enroll_code).unwrap().unwrap();

        let secret_bytes = base32_decode(&secret_b32).unwrap();
        let next_step = (now_secs() / TOTP_STEP_SECS) as i64 + 1;
        let fresh_code = totp_at_step(&secret_bytes, next_step as u64).unwrap();
        assert!(store
            .mfa_disable("henry", Some(&fresh_code), false)
            .unwrap());
        assert!(!store.mfa_enabled("henry"));
    }

    #[test]
    fn mfa_disable_without_enrollment_is_an_error() {
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        setup_user_for_mfa(&store, "iris");
        assert!(store.mfa_disable("iris", None, true).is_err());
    }

    #[test]
    fn drop_user_clears_mfa_enrollment() {
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        setup_user_for_mfa(&store, "jack");
        let (secret_b32, _) = store.mfa_enroll("jack").unwrap();
        let enroll_code = current_totp_code(&secret_b32);
        store.mfa_confirm("jack", &enroll_code).unwrap().unwrap();
        assert!(store.mfa_enabled("jack"));

        store.apply(&AuthStmt::DropUser("jack".into())).unwrap();
        assert!(!store.mfa_enabled("jack"));
    }

    #[test]
    fn auth_state_debug_never_leaks_mfa_secret_or_recovery_codes() {
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        setup_user_for_mfa(&store, "kate");
        let (secret_b32, _) = store.mfa_enroll("kate").unwrap();
        let enroll_code = current_totp_code(&secret_b32);
        let recovery_codes = store.mfa_confirm("kate", &enroll_code).unwrap().unwrap();

        let debug_str = format!("{:?}", store.lock());
        assert!(!debug_str.contains(&secret_b32));
        for rc in &recovery_codes {
            assert!(!debug_str.contains(rc));
        }
        assert!(debug_str.contains("redacted"));
    }

    // ── item 128 (Workstream D1): OAuth 2.0 social login ────────────────

    #[test]
    fn oauth_state_round_trips_and_returns_the_verifier() {
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        let (state, verifier, _expires_at) = store.create_oauth_state("google").unwrap();
        assert_eq!(verifier.len(), 64); // 32 random bytes, hex-encoded
        let got = store.verify_oauth_state(&state, "google").unwrap();
        assert_eq!(got, Some(verifier));
    }

    #[test]
    fn oauth_state_is_single_use() {
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        let (state, _verifier, _expires_at) = store.create_oauth_state("google").unwrap();
        assert!(store
            .verify_oauth_state(&state, "google")
            .unwrap()
            .is_some());
        // Replay of the exact same state must fail — single-use.
        assert!(store
            .verify_oauth_state(&state, "google")
            .unwrap()
            .is_none());
    }

    #[test]
    fn oauth_state_rejects_wrong_provider_and_garbage() {
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        let (state, _verifier, _expires_at) = store.create_oauth_state("google").unwrap();
        // Right state, wrong provider — must not verify.
        assert!(store
            .verify_oauth_state(&state, "github")
            .unwrap()
            .is_none());
        // Unknown/garbage state.
        assert!(store
            .verify_oauth_state("not-a-real-state-token", "google")
            .unwrap()
            .is_none());
        // The genuine state for the correct provider still works (proves
        // the wrong-provider attempt above didn't consume it).
        assert!(store
            .verify_oauth_state(&state, "google")
            .unwrap()
            .is_some());
    }

    #[test]
    fn oauth_link_or_create_first_login_creates_a_new_user() {
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        let username = store
            .oauth_link_or_create("google", "provider-uid-1", "oauth_google_provider-uid-1")
            .unwrap();
        assert_eq!(username, "oauth_google_provider-uid-1");
        assert!(store.users().iter().any(|(n, su)| n == &username && !su));
        assert_eq!(
            store.oauth_identity_username("google", "provider-uid-1"),
            Some(username)
        );
    }

    #[test]
    fn oauth_link_or_create_second_login_reuses_the_same_user() {
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        let first = store
            .oauth_link_or_create("google", "provider-uid-2", "oauth_google_provider-uid-2")
            .unwrap();
        let second = store
            .oauth_link_or_create("google", "provider-uid-2", "oauth_google_provider-uid-2")
            .unwrap();
        assert_eq!(first, second);
        // Only one user was created, not two.
        assert_eq!(store.users().iter().filter(|(n, _)| n == &first).count(), 1);
    }

    #[test]
    fn oauth_link_or_create_distinct_providers_are_independent_identities() {
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        let google_user = store
            .oauth_link_or_create("google", "same-id", "oauth_google_same-id")
            .unwrap();
        let github_user = store
            .oauth_link_or_create("github", "same-id", "oauth_github_same-id")
            .unwrap();
        // Same provider_user_id string under two different providers must
        // resolve to two distinct unidb accounts — the identity key is
        // (provider, provider_user_id), not provider_user_id alone.
        assert_ne!(google_user, github_user);
    }

    #[test]
    fn drop_user_clears_oauth_identity_link() {
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        let username = store
            .oauth_link_or_create("google", "provider-uid-3", "oauth_google_provider-uid-3")
            .unwrap();
        assert!(store
            .oauth_identity_username("google", "provider-uid-3")
            .is_some());
        store.apply(&AuthStmt::DropUser(username)).unwrap();
        assert!(store
            .oauth_identity_username("google", "provider-uid-3")
            .is_none());
    }

    #[test]
    fn auth_state_debug_redacts_oauth_state_but_not_identities() {
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        let (state, verifier, _expires_at) = store.create_oauth_state("google").unwrap();
        let username = store
            .oauth_link_or_create("google", "provider-uid-4", "oauth_google_provider-uid-4")
            .unwrap();

        let debug_str = format!("{:?}", store.lock());
        // The PKCE verifier is a secret — must never appear in Debug output.
        assert!(!debug_str.contains(&verifier));
        assert!(!debug_str.contains(&state));
        // Identity links carry no secret — the username IS expected to
        // appear (it's not redacted, per the item-128 design note).
        assert!(debug_str.contains(&username));
    }

    // ── item 138: email transport + password-reset / magic-link flows ───

    #[test]
    fn recovery_token_round_trips_and_is_single_use() {
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        store
            .apply(&AuthStmt::CreateUser {
                name: "liam".to_string(),
                superuser: false,
                password: Some("old-password".to_string()),
            })
            .unwrap();
        let (token, _expires_at) = store.create_recovery_token("liam").unwrap();
        assert_eq!(
            store.verify_recovery_token(&token).unwrap(),
            Some("liam".to_string())
        );
        // Replay of the exact same token must fail — single-use.
        assert_eq!(store.verify_recovery_token(&token).unwrap(), None);
    }

    #[test]
    fn recovery_token_rejects_garbage_and_expired() {
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        assert_eq!(
            store.verify_recovery_token("not-a-real-token").unwrap(),
            None
        );
        // A genuine token, but already past its expiry — simulate by
        // constructing the record directly with an expiry in the past
        // (round-tripping through `create_recovery_token`'s real TTL would
        // require sleeping past an hour in a unit test).
        let (token, _expires_at) = store.create_recovery_token("nobody-yet").unwrap();
        {
            let mut st = store.lock();
            for rec in st.recovery_tokens.values_mut() {
                rec.expires_at = 0;
            }
        }
        assert_eq!(store.verify_recovery_token(&token).unwrap(), None);
    }

    #[test]
    fn magiclink_token_round_trips_and_is_single_use() {
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        store
            .apply(&AuthStmt::CreateUser {
                name: "noor".to_string(),
                superuser: false,
                password: None,
            })
            .unwrap();
        let (token, _expires_at) = store.create_magiclink_token("noor").unwrap();
        assert_eq!(
            store.verify_magiclink_token(&token).unwrap(),
            Some("noor".to_string())
        );
        assert_eq!(store.verify_magiclink_token(&token).unwrap(), None);
    }

    #[test]
    fn recovery_and_magiclink_token_spaces_are_independent() {
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        let (recovery_token, _) = store.create_recovery_token("iris").unwrap();
        // A recovery token must never redeem as a magic-link token, even
        // though both are 256-bit hex strings from the same construction.
        assert_eq!(store.verify_magiclink_token(&recovery_token).unwrap(), None);
        // ...and the genuine recovery redemption still works afterward,
        // proving the cross-space attempt didn't consume it.
        assert_eq!(
            store.verify_recovery_token(&recovery_token).unwrap(),
            Some("iris".to_string())
        );
    }

    #[test]
    fn revoke_all_sessions_for_user_is_idempotent_and_scoped() {
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        store
            .apply(&AuthStmt::CreateUser {
                name: "omar".to_string(),
                superuser: false,
                password: Some("pw".to_string()),
            })
            .unwrap();
        store
            .apply(&AuthStmt::CreateUser {
                name: "priya".to_string(),
                superuser: false,
                password: Some("pw".to_string()),
            })
            .unwrap();
        let (omar_raw1, _) = store.create_session("omar").unwrap();
        let (omar_raw2, _) = store.create_session("omar").unwrap();
        let (priya_raw, _) = store.create_session("priya").unwrap();

        store.revoke_all_sessions_for_user("omar").unwrap();
        assert!(store.verify_session(&omar_raw1).is_none());
        assert!(store.verify_session(&omar_raw2).is_none());
        // priya's session is untouched — the revoke is scoped to "omar" only.
        assert_eq!(store.verify_session(&priya_raw), Some("priya".to_string()));

        // Idempotent: calling again with nothing left to revoke is a no-op,
        // not an error.
        assert!(store.revoke_all_sessions_for_user("omar").is_ok());
    }

    #[test]
    fn auth_state_debug_redacts_recovery_and_magiclink_tokens() {
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        let (recovery_token, _) = store.create_recovery_token("quinn").unwrap();
        let (magiclink_token, _) = store.create_magiclink_token("quinn").unwrap();
        let debug_str = format!("{:?}", store.lock());
        assert!(!debug_str.contains(&recovery_token));
        assert!(!debug_str.contains(&magiclink_token));
        assert!(debug_str.contains("redacted"));
    }

    // ── item 129 (Workstream I3): encrypt-at-rest secrets vault ─────────

    /// A fixed, deterministic test key (never a real deployment key) — via
    /// [`crate::vault::Vault::for_test`], not `UNIDB_MASTER_KEY` env-var
    /// plumbing, so these tests never race other tests' process-global env
    /// state under parallel `cargo test`.
    fn test_vault() -> crate::vault::Vault {
        let vault = crate::vault::Vault::for_test([42u8; 32]);
        assert!(vault.is_enabled());
        vault
    }

    #[test]
    fn set_secret_then_get_secret_roundtrips_the_plaintext() {
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        let vault = test_vault();
        store
            .set_secret("oauth.google.client_secret", "hunter2-oauth-secret", &vault)
            .unwrap();
        let got = store
            .get_secret("oauth.google.client_secret", &vault)
            .unwrap();
        assert_eq!(got, Some("hunter2-oauth-secret".to_string()));
    }

    #[test]
    fn get_secret_returns_none_when_never_stored() {
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        let vault = test_vault();
        assert_eq!(store.get_secret("never.stored", &vault).unwrap(), None);
    }

    #[test]
    fn has_secret_and_secret_names_never_expose_the_value() {
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        let vault = test_vault();
        assert!(!store.has_secret("oauth.github.client_secret"));
        store
            .set_secret("oauth.github.client_secret", "top-secret", &vault)
            .unwrap();
        assert!(store.has_secret("oauth.github.client_secret"));
        let names = store.secret_names();
        assert_eq!(names, vec!["oauth.github.client_secret".to_string()]);
        assert!(!names.iter().any(|n| n.contains("top-secret")));
    }

    #[test]
    fn set_secret_fails_closed_when_vault_disabled() {
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        let disabled = crate::vault::Vault::disabled();
        assert!(store
            .set_secret("oauth.google.client_secret", "plaintext", &disabled)
            .is_err());
        // Nothing was persisted — a disabled vault must never fall back to
        // storing a secret unencrypted.
        assert!(!store.has_secret("oauth.google.client_secret"));
    }

    #[test]
    fn get_secret_fails_closed_when_vault_key_changes() {
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        let vault_a = test_vault();
        store
            .set_secret("oauth.google.client_secret", "hunter2", &vault_a)
            .unwrap();

        // A different master key (simulating a rotated/wrong key at a later
        // startup) must fail to decrypt — never silently return plaintext,
        // never silently fall back to `None`.
        let vault_b = crate::vault::Vault::for_test([7u8; 32]);
        let err = store
            .get_secret("oauth.google.client_secret", &vault_b)
            .unwrap_err();
        assert!(format!("{err}").contains("decrypt"));
    }

    #[test]
    fn stored_secret_ciphertext_never_appears_in_persisted_roles_json_or_debug() {
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        let vault = test_vault();
        store
            .set_secret(
                "oauth.google.client_secret",
                "extremely-sensitive-plaintext-value",
                &vault,
            )
            .unwrap();

        // Debug of the in-memory store must redact to a count, never the
        // ciphertext blob (which itself would aid an attacker even though
        // it isn't the plaintext).
        let debug_str = format!("{:?}", store.lock());
        assert!(!debug_str.contains("extremely-sensitive-plaintext-value"));
        assert!(debug_str.contains("secrets"));
        assert!(debug_str.contains("redacted"));

        // The persisted roles.json must never contain the plaintext either.
        let on_disk = std::fs::read_to_string(dir.path().join("roles.json")).unwrap();
        assert!(!on_disk.contains("extremely-sensitive-plaintext-value"));
        assert!(on_disk.contains("oauth.google.client_secret"));
    }

    #[test]
    fn pre_129_roles_json_without_secrets_key_still_loads() {
        // Back-compat: a `roles.json` written before item 129 has no
        // `secrets` key at all. `#[serde(default)]` must make it load as an
        // empty map, not fail to open.
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("roles.json"),
            r#"{"users":{"alice":true},"roles":[],"memberships":{},"table_grants":{}}"#,
        )
        .unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        assert!(store.is_superuser("alice"));
        assert!(store.secret_names().is_empty());
        assert!(!store.has_secret("anything"));
    }

    // ── item 140: realtime channel-authorization policies ──────────────

    #[test]
    fn channel_policy_no_match_allows_off_denies_on() {
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        // No policy stored at all.
        assert!(store.check_channel_policy(
            "room:1",
            ChannelOp::Subscribe,
            &["authenticated".into()],
            false
        ));
        assert!(!store.check_channel_policy(
            "room:1",
            ChannelOp::Subscribe,
            &["authenticated".into()],
            true
        ));
    }

    #[test]
    fn channel_policy_matched_topic_enforces_regardless_of_require_authz() {
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        store
            .set_channel_policy(
                "room:*",
                ChannelOp::Subscribe,
                BTreeSet::from(["member".to_string()]),
            )
            .unwrap();
        // A caller with the role is allowed whether or not enforce mode is on.
        assert!(store.check_channel_policy(
            "room:42",
            ChannelOp::Subscribe,
            &["member".into()],
            false
        ));
        assert!(store.check_channel_policy(
            "room:42",
            ChannelOp::Subscribe,
            &["member".into()],
            true
        ));
        // A caller without it is denied either way (a matched policy is
        // always enforced).
        assert!(!store.check_channel_policy(
            "room:42",
            ChannelOp::Subscribe,
            &["authenticated".into()],
            false
        ));
        assert!(!store.check_channel_policy(
            "room:42",
            ChannelOp::Subscribe,
            &["authenticated".into()],
            true
        ));
    }

    #[test]
    fn channel_policy_most_specific_pattern_wins() {
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        store
            .set_channel_policy(
                "room:*",
                ChannelOp::Subscribe,
                BTreeSet::from(["member".to_string()]),
            )
            .unwrap();
        store
            .set_channel_policy(
                "room:42",
                ChannelOp::Subscribe,
                BTreeSet::from(["vip".to_string()]),
            )
            .unwrap();
        // The exact "room:42" policy overrides the "room:*" glob for that topic.
        assert!(store.check_channel_policy(
            "room:42",
            ChannelOp::Subscribe,
            &["vip".into()],
            false
        ));
        assert!(!store.check_channel_policy(
            "room:42",
            ChannelOp::Subscribe,
            &["member".into()],
            false
        ));
        // A different topic under the glob still uses the less-specific policy.
        assert!(store.check_channel_policy(
            "room:7",
            ChannelOp::Subscribe,
            &["member".into()],
            false
        ));
    }

    #[test]
    fn channel_policy_all_op_governs_every_operation_but_specific_op_wins_tie() {
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        store
            .set_channel_policy("t", ChannelOp::All, BTreeSet::from(["anyrole".to_string()]))
            .unwrap();
        // `All` governs publish/subscribe/presence alike.
        assert!(store.check_channel_policy("t", ChannelOp::Publish, &["anyrole".into()], false));
        assert!(store.check_channel_policy("t", ChannelOp::Presence, &["anyrole".into()], false));

        // An explicit same-pattern `Subscribe` policy overrides the `All`
        // catch-all for `Subscribe` specifically (documented tie-break).
        store
            .set_channel_policy(
                "t",
                ChannelOp::Subscribe,
                BTreeSet::from(["subrole".to_string()]),
            )
            .unwrap();
        assert!(!store.check_channel_policy("t", ChannelOp::Subscribe, &["anyrole".into()], false));
        assert!(store.check_channel_policy("t", ChannelOp::Subscribe, &["subrole".into()], false));
        // Publish is untouched — still governed by the `All` policy.
        assert!(store.check_channel_policy("t", ChannelOp::Publish, &["anyrole".into()], false));
    }

    #[test]
    fn channel_policy_upsert_replaces_remove_is_idempotent_and_persists() {
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        store
            .set_channel_policy("t", ChannelOp::Publish, BTreeSet::from(["a".to_string()]))
            .unwrap();
        store
            .set_channel_policy("t", ChannelOp::Publish, BTreeSet::from(["b".to_string()]))
            .unwrap();
        let policies = store.list_channel_policies();
        assert_eq!(policies.len(), 1, "upsert must replace, not duplicate");
        assert_eq!(policies[0].roles, BTreeSet::from(["b".to_string()]));

        // Removing an unknown pair is a no-op, not an error.
        store
            .remove_channel_policy("does-not-exist", ChannelOp::Publish)
            .unwrap();
        assert_eq!(store.list_channel_policies().len(), 1);

        store
            .remove_channel_policy("t", ChannelOp::Publish)
            .unwrap();
        assert!(store.list_channel_policies().is_empty());

        // Reopening the store from disk must see the same (empty) state —
        // proves persistence round-trips, not just the in-memory map.
        drop(store);
        let reopened = RoleStore::open(dir.path()).unwrap();
        assert!(reopened.list_channel_policies().is_empty());
    }

    #[test]
    fn pre_140_roles_json_without_channel_policies_key_still_loads() {
        // Back-compat: a `roles.json` written before item 140 has no
        // `channel_policies` key. `#[serde(default)]` must make it load as
        // an empty list, not fail to open.
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("roles.json"),
            r#"{"users":{"alice":true},"roles":[],"memberships":{},"table_grants":{}}"#,
        )
        .unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        assert!(store.is_superuser("alice"));
        assert!(store.list_channel_policies().is_empty());
    }
}
