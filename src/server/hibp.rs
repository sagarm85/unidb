//! HaveIBeenPwned "Pwned Passwords" leaked-password check (item 143, part
//! 1). Complements argon2id credential storage (unchanged by this module)
//! with a *breach* check at every password-set point: `POST /auth/signup`,
//! admin `POST`/`PATCH /auth/admin/users` (item 142), and `POST
//! /auth/verify` (password reset, item 138).
//!
//! **k-anonymity, by construction.** The Pwned Passwords range API
//! (<https://haveibeenpwned.com/API/v3#PwnedPasswords>) is free, needs no
//! API key, and never sees the password itself: the client computes the
//! **SHA-1** of the password, sends only the first 5 hex characters (the
//! "prefix" — one of 2^20 buckets), and the API returns every breached
//! suffix in that bucket as `SUFFIX:count` lines. The match against the
//! remaining 35 hex characters (the "suffix") happens **locally**, in
//! [`HibpConfig::check`] — the full password, its full hash, and even the
//! full SHA-1 digest never leave this process.
//!
//! **SHA-1 here is HIBP's bucketing key ONLY.** It is never used for
//! credential storage — passwords are still hashed with argon2id via
//! [`crate::authz::RoleStore::set_password`]/`create_user_with_password`,
//! completely independent of this module. Do not read "this module computes
//! a SHA-1 of the password" as "this is how unidb stores passwords" — it
//! isn't; see the module-level warning above repeated once more because
//! it's the one fact about this file most likely to be misread out of
//! context.
//!
//! **Opt-in, fail-open.** Mirrors `captcha.rs`'s shape (env-gated, mock-
//! overridable outbound-verification check) with one deliberate reversal:
//! CAPTCHA fails *closed* once enabled (a misconfigured/unreachable
//! verifier blocks the request); this fails **open** — an unreachable HIBP
//! endpoint must never lock a legitimate user out of signup/password-reset
//! over an external outage. `UNIDB_PASSWORD_HIBP_CHECK` (default **off**)
//! opts in; `UNIDB_PASSWORD_HIBP_URL` overrides the base URL (default
//! `https://api.pwnedpasswords.com`), the seam tests point at a local mock
//! with zero real network. A fail-closed toggle is a possible follow-up
//! (documented in `docs/backlog/143_auth_hardening_hibp_oauth_presets.md`),
//! not built here.

use std::time::Duration;

use sha1::{Digest, Sha1};

const DEFAULT_HIBP_URL: &str = "https://api.pwnedpasswords.com";

/// Leaked-password gate held on [`crate::server::AppState`]. Disabled by
/// default ([`HibpConfig::disabled`]) — every password-set handler is a
/// no-op call to [`Self::is_compromised`] (always `false`, no outbound
/// request) until `UNIDB_PASSWORD_HIBP_CHECK` opts in.
#[derive(Clone)]
pub struct HibpConfig {
    enabled: bool,
    base_url: String,
    http: reqwest::Client,
}

impl Default for HibpConfig {
    fn default() -> Self {
        Self::disabled()
    }
}

impl HibpConfig {
    /// The safe, back-compat default — no endpoint ever contacted.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            base_url: DEFAULT_HIBP_URL.to_string(),
            http: build_http_client(),
        }
    }

    /// Explicit construction — used by tests to point at a local mock range
    /// server without touching process-global environment variables.
    pub fn new(enabled: bool, base_url: impl Into<String>) -> Self {
        Self {
            enabled,
            base_url: base_url.into(),
            http: build_http_client(),
        }
    }

    /// `UNIDB_PASSWORD_HIBP_CHECK` (default off — any of `1`/`true`/`TRUE`/
    /// `True` turns it on, anything else including unset stays off) /
    /// `UNIDB_PASSWORD_HIBP_URL` (default [`DEFAULT_HIBP_URL`]).
    pub fn from_env() -> Self {
        let enabled = std::env::var("UNIDB_PASSWORD_HIBP_CHECK")
            .ok()
            .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE" | "True"))
            .unwrap_or(false);
        let base_url = std::env::var("UNIDB_PASSWORD_HIBP_URL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_HIBP_URL.to_string());
        Self {
            enabled,
            base_url,
            http: build_http_client(),
        }
    }

    /// Whether the check is active at all — used only for a startup log
    /// line, not on any request path.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// The gate every password-set handler calls: `true` only on a
    /// **confirmed** breach match. `false` when disabled, and `false`
    /// (fail **open**, with a `tracing::warn!`) when the endpoint could not
    /// be reached or its response could not be parsed — see the module doc.
    pub async fn is_compromised(&self, password: &str) -> bool {
        if !self.enabled {
            return false;
        }
        match self.check(password).await {
            Ok(compromised) => compromised,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "HIBP leaked-password check failed (endpoint unreachable or its response \
                     was unparseable) — failing OPEN, not blocking this password-set operation"
                );
                false
            }
        }
    }

    /// Compute the SHA-1 hex digest of `password` (bucketing key only — see
    /// the module doc), `GET <base_url>/range/<5-char prefix>`, and check
    /// whether the remaining 35-char suffix appears in the `SUFFIX:count`
    /// response lines. Only the prefix ever leaves this process.
    async fn check(&self, password: &str) -> Result<bool, String> {
        let digest = Sha1::digest(password.as_bytes());
        let hex = to_upper_hex(digest.as_slice());
        // A SHA-1 hex digest is always 40 characters; `split_at(5)` is
        // in-bounds unconditionally.
        let (prefix, suffix) = hex.split_at(5);
        let url = format!("{}/range/{prefix}", self.base_url.trim_end_matches('/'));
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("HIBP range request failed: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("HIBP endpoint returned {status}"));
        }
        let body = resp
            .text()
            .await
            .map_err(|e| format!("malformed HIBP response body: {e}"))?;
        for line in body.lines() {
            let Some((line_suffix, _count)) = line.trim().split_once(':') else {
                continue;
            };
            if line_suffix.eq_ignore_ascii_case(suffix) {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// Uppercase hex encoding — HIBP's range API returns suffixes uppercase;
/// hand-rolled rather than pulling in a `hex` crate for one call site
/// (CLAUDE.md: no new dependency for something this small).
fn to_upper_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02X}"));
    }
    s
}

/// Outbound HTTP client for the range call, with a bounded timeout — same
/// shape/rationale as `oauth::build_http_client`/`captcha::build_http_client`
/// (a slow/unresponsive endpoint must fail the request, not hang it, so
/// fail-open actually triggers instead of stalling the handler); never
/// `unwrap`/`expect`.
fn build_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_by_default_and_is_enabled_reports_it() {
        let cfg = HibpConfig::disabled();
        assert!(!cfg.is_enabled());
    }

    #[test]
    fn explicit_construction_reports_enabled() {
        let cfg = HibpConfig::new(true, "http://example.invalid");
        assert!(cfg.is_enabled());
    }

    #[test]
    fn to_upper_hex_matches_known_sha1_vector() {
        // SHA-1("password") — a well-known test vector, uppercased.
        let digest = Sha1::digest(b"password");
        let hex = to_upper_hex(digest.as_slice());
        assert_eq!(hex, "5BAA61E4C9B93F3F0682250B6CF8331B7EE68FD");
        assert_eq!(hex.len(), 40);
    }

    #[tokio::test]
    async fn disabled_check_never_reports_compromised_without_network() {
        // Base URL is deliberately unroutable; disabled short-circuits
        // before any request would be attempted.
        let cfg = HibpConfig::new(false, "http://127.0.0.1:1");
        assert!(!cfg.is_compromised("whatever").await);
    }
}
