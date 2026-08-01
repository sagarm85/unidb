//! Request/response JSON shapes for the M5 REST server, kept separate from
//! `handlers.rs` so the wire schema is easy to audit in one place — mirrors
//! how `sql/logical.rs` (the "shape" types) is kept separate from
//! `sql/executor.rs` (the "behavior" using them).
//!
//! **Why `Literal`/`ExecResult` don't just derive `Serialize`:** `Literal`
//! already derives `Serialize`/`Deserialize` unconditionally (`sql/
//! logical.rs`) — but that derive is load-bearing for the catalog's
//! on-disk format (`Expr::Literal` is embedded in the serde_json blob
//! `Catalog::persist` writes for RLS policies). Its default enum
//! representation serializes `Literal::Json(s)` as `{"Json": "<raw json
//! text as a string>"}`, which is correct and stable for that internal
//! use but is exactly the "JSON-encoded-as-a-string" shape the REST wire
//! format should *not* have (a client reading `payload.data.status`
//! shouldn't have to parse a string-within-a-string). Rather than risk
//! changing `Literal`'s existing serialization (a breaking change to the
//! catalog's on-disk format) or forking a second `Literal`-shaped type,
//! `literal_to_json` below does the REST-facing conversion explicitly,
//! reusing exactly the same per-variant mapping `queue::payload::
//! row_to_json` already established in M4 for the same reason.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value as Json};

use crate::{
    catalog::{ColumnType, IndexKind, TableDef},
    heap::RowId,
    sql::{executor::ExecResult, logical::Literal},
};

/// `Literal` -> REST-facing JSON, re-parsing `Literal::Json`'s raw text
/// into a real nested `serde_json::Value` rather than leaving it as a
/// JSON-encoded string.
pub fn literal_to_json(lit: &Literal) -> Json {
    match lit {
        Literal::Null => Json::Null,
        Literal::Int(n) => Json::Number(Number::from(*n)),
        Literal::Text(s) => Json::String(s.clone()),
        Literal::Bool(b) => Json::Bool(*b),
        Literal::Json(s) => serde_json::from_str(s).unwrap_or(Json::Null),
        Literal::Vector(v) => Json::Array(
            v.iter()
                .map(|f| {
                    Number::from_f64(*f as f64)
                        .map(Json::Number)
                        .unwrap_or(Json::Null)
                })
                .collect(),
        ),
        // Exact types serialize as strings so JSON's f64 numbers never lose
        // precision (P2.a) — decimal text and canonical UTC timestamp text.
        Literal::Decimal(value, scale) => {
            Json::String(crate::sql::logical::format_decimal(*value, *scale))
        }
        Literal::Timestamp(micros) => Json::String(crate::sql::datetime::format_timestamp(*micros)),
        // P2.b scalar types: floats as JSON numbers; uuid/bytea/date/time as
        // canonical strings.
        Literal::Float(f) => Number::from_f64(*f).map(Json::Number).unwrap_or(Json::Null),
        Literal::Uuid(b) => Json::String(crate::sql::executor::format_uuid(b)),
        Literal::Bytea(b) => Json::String(crate::sql::executor::format_bytea(b)),
        Literal::Date(d) => Json::String(crate::sql::datetime::format_date(*d)),
        Literal::Time(t) => Json::String(crate::sql::datetime::format_time(*t)),
        // Bind placeholders are substituted before execution (P2.e); a result
        // row can never contain one.
        Literal::Param(_) => Json::Null,
    }
}

/// `ExecResult` -> a tagged JSON object (`{"type": "...", ...}`), the
/// response body shape for `/sql` and `/cypher`.
pub fn exec_result_to_json(result: &ExecResult) -> Json {
    let mut obj = Map::new();
    match result {
        ExecResult::CreatedTable => {
            obj.insert("type".into(), Json::String("created_table".into()));
        }
        ExecResult::CreatedIndex => {
            obj.insert("type".into(), Json::String("created_index".into()));
        }
        ExecResult::Inserted { count } => {
            obj.insert("type".into(), Json::String("inserted".into()));
            obj.insert("count".into(), Json::Number(Number::from(*count)));
        }
        ExecResult::Updated { count } => {
            obj.insert("type".into(), Json::String("updated".into()));
            obj.insert("count".into(), Json::Number(Number::from(*count)));
        }
        ExecResult::Deleted { count } => {
            obj.insert("type".into(), Json::String("deleted".into()));
            obj.insert("count".into(), Json::Number(Number::from(*count)));
        }
        ExecResult::Rows { columns, rows } => {
            obj.insert("type".into(), Json::String("rows".into()));
            obj.insert(
                "columns".into(),
                Json::Array(columns.iter().map(|c| Json::String(c.clone())).collect()),
            );
            let json_rows: Vec<Json> = rows
                .iter()
                .map(|row| Json::Array(row.iter().map(literal_to_json).collect()))
                .collect();
            obj.insert("rows".into(), Json::Array(json_rows));
        }
        ExecResult::AlteredTable => {
            obj.insert("type".into(), Json::String("altered_table".into()));
        }
        ExecResult::DroppedTable => {
            obj.insert("type".into(), Json::String("dropped_table".into()));
        }
        ExecResult::Truncated { count } => {
            obj.insert("type".into(), Json::String("truncated".into()));
            obj.insert("count".into(), Json::Number(Number::from(*count)));
        }
    }
    Json::Object(obj)
}

#[derive(Debug, Deserialize)]
pub struct SqlRequest {
    pub sql: String,
    /// Optional positional bind parameters for `$1`, `$2`, ... in `sql`
    /// (P2.e). Absent/empty means the SQL is executed as-is. Supplying params
    /// is the injection-safe way to pass user data.
    #[serde(default)]
    pub params: Vec<Json>,
    /// Optional isolation level for a **one-shot** statement (R2): the
    /// request runs as a single transaction at this level without opening a
    /// session. Rejected on a request that also carries `X-Txn-Id` —
    /// isolation is fixed at `POST /txn/begin` for sessions.
    #[serde(default)]
    pub isolation: Option<IsolationDto>,
    /// `true` (R4) buffers the query's `rows` result server-side and returns
    /// a `cursor_id` instead of the rows, for paging via
    /// `GET /sql/cursor/{id}?limit=N`. Requires the request to produce
    /// exactly one `rows`-shaped result.
    #[serde(default)]
    pub cursor: bool,
}

/// Wire form of [`IsolationLevel`] (R1/R2). Snake-case strings on the wire:
/// `"read_committed"`, `"repeatable_read"`, `"serializable"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IsolationDto {
    ReadCommitted,
    RepeatableRead,
    Serializable,
}

impl IsolationDto {
    pub fn to_engine(self) -> crate::txn::IsolationLevel {
        use crate::txn::IsolationLevel as I;
        match self {
            IsolationDto::ReadCommitted => I::ReadCommitted,
            IsolationDto::RepeatableRead => I::RepeatableRead,
            IsolationDto::Serializable => I::Serializable,
        }
    }

    pub fn from_engine(level: crate::txn::IsolationLevel) -> Self {
        use crate::txn::IsolationLevel as I;
        match level {
            I::ReadCommitted => IsolationDto::ReadCommitted,
            I::RepeatableRead => IsolationDto::RepeatableRead,
            I::Serializable => IsolationDto::Serializable,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            IsolationDto::ReadCommitted => "read_committed",
            IsolationDto::RepeatableRead => "repeatable_read",
            IsolationDto::Serializable => "serializable",
        }
    }
}

/// Body of `POST /txn/begin` (R1). The body itself is optional — an empty
/// body opens a `read_committed` session.
#[derive(Debug, Default, Deserialize)]
pub struct BeginTxnRequest {
    #[serde(default)]
    pub isolation: Option<IsolationDto>,
}

/// Body of `PUT /tables/{table}/rls` (R3): the policy as a SQL predicate
/// string (the same AND-only comparison subset `WHERE` accepts), e.g.
/// `"tenant_id = 7"`. Chosen over a JSON policy DSL so the existing SQL
/// parser is the single grammar — see the backlog spec's blocker note.
#[derive(Debug, Deserialize)]
pub struct RlsRequest {
    pub predicate: String,
}

/// Body of `PUT /config/slow_query_threshold_ms` (item 34, Part A).
/// `threshold_ms: 0` disables slow-query logging; positive values enable it.
#[derive(Debug, Deserialize)]
pub struct SlowQueryThresholdRequest {
    pub threshold_ms: u64,
}

/// Body of `PUT /config/group_commit_window_us` (item 101).
/// `value: 0` disables the group-commit dwell window (default, each commit
/// fsyncs immediately); a positive value enables it.
#[derive(Debug, Deserialize)]
pub struct GroupCommitWindowRequest {
    pub value: u64,
}

/// Query params for `GET /stats/history` (item 34, Part B).
#[derive(Debug, Default, Deserialize)]
pub struct HistoryQuery {
    /// Number of points to return; default 60, max 300.
    pub points: Option<u32>,
    /// Resolution hint echoed back in the response (ms); default 5000.
    pub interval_ms: Option<u64>,
}

/// Body of `POST /rows/batch` (R4): raw row payloads, base64-encoded (rows
/// are opaque bytes and JSON cannot carry them verbatim).
#[derive(Debug, Deserialize)]
pub struct BatchInsertRequest {
    pub rows: Vec<String>,
}

/// Query string of `GET /sql/cursor/{id}` (R4).
#[derive(Debug, Deserialize)]
pub struct CursorQuery {
    /// Rows per page; default 1000, capped at 10 000.
    #[serde(default = "default_cursor_limit")]
    pub limit: usize,
}

fn default_cursor_limit() -> usize {
    1000
}

/// Convert a REST JSON bind-parameter value to a [`Literal`] (P2.e). The value
/// is always treated as *data*: a JSON string becomes `Literal::Text` (later
/// coerced to the target column's type — UUID, TIMESTAMP, etc.), a number
/// becomes `Int` or `Float`, and so on. Objects/arrays are passed through as
/// `Json`/`Vector` for JSON and vector columns respectively.
pub fn json_to_literal(v: &Json) -> Literal {
    match v {
        Json::Null => Literal::Null,
        Json::Bool(b) => Literal::Bool(*b),
        Json::String(s) => Literal::Text(s.clone()),
        Json::Number(n) => {
            if let Some(i) = n.as_i64() {
                Literal::Int(i)
            } else {
                Literal::Float(n.as_f64().unwrap_or(0.0))
            }
        }
        // A JSON array of numbers is a vector literal; anything else round-trips
        // as a JSON-typed value.
        Json::Array(items) if items.iter().all(|x| x.is_number()) => Literal::Vector(
            items
                .iter()
                .map(|x| x.as_f64().unwrap_or(0.0) as f32)
                .collect(),
        ),
        other => Literal::Json(other.to_string()),
    }
}

/// Body of `POST /auth/preview` (item-24 Z6): run `sql` as `as_role` and
/// return the result, including RLS filtering. Superuser-gated.
#[derive(Debug, Deserialize)]
pub struct AuthPreviewRequest {
    pub as_role: String,
    pub sql: String,
}

/// Body of `POST /auth/login` (item 100 issuer; item 121 A2 real
/// authentication). Still gated by `UNIDB_DEV_LOGIN=1` (production issuer
/// configuration is item 121 A5, not yet implemented), but the password is
/// now verified against the stored argon2id credential — this is real
/// authentication, not mere identification.
#[derive(Deserialize)]
pub struct AuthLoginRequest {
    pub username: String,
    pub password: String,
    /// Item 131 (Workstream I2): optional CAPTCHA token from the client-side
    /// widget (Cloudflare Turnstile by default). Only required when
    /// `UNIDB_CAPTCHA_PROTECT` includes `login` — see
    /// `server::captcha`'s module doc; ignored (and safe to omit) otherwise,
    /// so back-compat is unconditional. Never logged.
    #[serde(default)]
    pub captcha_token: Option<String>,
}

/// Manual `Debug`: redacts `password`/`captcha_token` so this request body
/// can never leak either secret via `{:?}` (e.g. request-body logging
/// middleware, an `unwrap_err` printing the value it failed on).
impl std::fmt::Debug for AuthLoginRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthLoginRequest")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field(
                "captcha_token",
                &self.captcha_token.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// Response of `POST /auth/login`, `POST /auth/signup` (item 121, A3), and
/// `POST /auth/refresh` (item 121, A4) — all three hand back the same
/// access+refresh token pair shape.
///
/// `token` is kept as a **deprecated alias for `access_token`** (identical
/// value) so pre-A4 clients that only read `body.token` keep working
/// unchanged; new clients should read `access_token`/`refresh_token`.
pub struct AuthLoginResponse {
    /// Deprecated alias for `access_token` — kept for backward compatibility
    /// with pre-A4 clients (item 121 A1/A2 shipped `{token, expires_in}`).
    pub token: String,
    /// Short-lived (1 h) HS256 JWT — identical value to `token`.
    pub access_token: String,
    /// Long-lived opaque high-entropy refresh token (NOT a JWT). Exchange it
    /// at `POST /auth/refresh` for a new access token; `POST /auth/logout`
    /// revokes it.
    pub refresh_token: String,
    /// Access-token lifetime in seconds (always 3600 today).
    pub expires_in: u64,
}

impl Serialize for AuthLoginResponse {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut s = serializer.serialize_struct("AuthLoginResponse", 4)?;
        s.serialize_field("token", &self.token)?;
        s.serialize_field("access_token", &self.access_token)?;
        s.serialize_field("refresh_token", &self.refresh_token)?;
        s.serialize_field("expires_in", &self.expires_in)?;
        s.end()
    }
}

/// Manual `Debug`: redacts `token`/`access_token`/`refresh_token` — this
/// response body carries live bearer credentials (the refresh token in
/// particular is long-lived), and CLAUDE.md's "nothing secret in logs/
/// Debug" rule for this milestone applies to responses just as much as
/// requests (a `{:?}` of a handler's return value, or a test failure
/// message that debug-prints the body, must not leak a usable credential).
impl std::fmt::Debug for AuthLoginResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthLoginResponse")
            .field("token", &"<redacted>")
            .field("access_token", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

/// Body of `POST /auth/signup` (item 121, A3): create a non-superuser
/// account with a password credential. Gated behind `UNIDB_ALLOW_SIGNUP=1`
/// (default off); returns 404 when disabled, same posture as dev-login.
#[derive(Deserialize)]
pub struct AuthSignupRequest {
    pub username: String,
    pub password: String,
    /// Item 131 (Workstream I2): optional CAPTCHA token, required only when
    /// `UNIDB_CAPTCHA_PROTECT` includes `signup` — see
    /// [`AuthLoginRequest::captcha_token`].
    #[serde(default)]
    pub captcha_token: Option<String>,
}

/// Manual `Debug`: redacts `password`/`captcha_token`, mirroring
/// [`AuthLoginRequest`].
impl std::fmt::Debug for AuthSignupRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthSignupRequest")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field(
                "captcha_token",
                &self.captcha_token.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// Body of `POST /auth/refresh` (item 121, A4): exchange a valid refresh
/// token for a fresh access token (and a rotated refresh token).
#[derive(Deserialize)]
pub struct AuthRefreshRequest {
    pub refresh_token: String,
}

/// Manual `Debug`: redacts `refresh_token` — it is a live bearer credential,
/// same posture as a password.
impl std::fmt::Debug for AuthRefreshRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthRefreshRequest")
            .field("refresh_token", &"<redacted>")
            .finish()
    }
}

/// Body of `POST /auth/logout` (item 121, A4): revoke a refresh-token
/// session. Idempotent — logging out twice, or with an already-invalid
/// token, still succeeds.
#[derive(Deserialize)]
pub struct AuthLogoutRequest {
    pub refresh_token: String,
}

/// Manual `Debug`: redacts `refresh_token`, same posture as
/// [`AuthRefreshRequest`].
impl std::fmt::Debug for AuthLogoutRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthLogoutRequest")
            .field("refresh_token", &"<redacted>")
            .finish()
    }
}

// ── item 138: email transport + password-reset / magic-link flows ─────────

/// Body of `POST /auth/recover` (item 138): request a password-reset email
/// for `email`. Always responds `200` (via [`AuthEmailFlowAck`]) regardless
/// of whether `email` matches a registered account — see `handlers.rs::
/// post_auth_recover`'s doc comment for the no-account-enumeration contract.
#[derive(Deserialize)]
pub struct AuthRecoverRequest {
    pub email: String,
    /// Item 131 (Workstream I2): optional CAPTCHA token, required only when
    /// `UNIDB_CAPTCHA_PROTECT` includes `recover` — see
    /// [`AuthLoginRequest::captcha_token`].
    #[serde(default)]
    pub captcha_token: Option<String>,
}

/// Manual `Debug`: redacts `captcha_token`, same posture as
/// [`AuthLoginRequest`]. `email` is not a secret (same non-secret-identifier
/// posture as `username` elsewhere in this file) — shown as-is.
impl std::fmt::Debug for AuthRecoverRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthRecoverRequest")
            .field("email", &self.email)
            .field(
                "captcha_token",
                &self.captcha_token.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// Body of `POST /auth/magiclink` (item 138): request a magic sign-in link
/// email for `email`. Same always-`200`, no-account-enumeration contract as
/// [`AuthRecoverRequest`].
#[derive(Deserialize)]
pub struct AuthMagicLinkRequest {
    pub email: String,
    /// Required only when `UNIDB_CAPTCHA_PROTECT` includes `magiclink`.
    #[serde(default)]
    pub captcha_token: Option<String>,
}

/// Manual `Debug`: redacts `captcha_token`, mirrors [`AuthRecoverRequest`].
impl std::fmt::Debug for AuthMagicLinkRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthMagicLinkRequest")
            .field("email", &self.email)
            .field(
                "captcha_token",
                &self.captcha_token.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// Response of `POST /auth/recover`/`POST /auth/magiclink` (item 138):
/// deliberately content-free beyond a fixed `ok: true` — the whole point of
/// the no-account-enumeration contract is that a known vs. unknown `email`
/// produces a byte-identical response, so this type carries nothing that
/// could vary between the two cases.
#[derive(Debug, Serialize)]
pub struct AuthEmailFlowAck {
    pub ok: bool,
}

/// Body of `POST /auth/verify` (item 138): redeem a password-recovery token
/// (from the emailed link) for a new password. Unknown/expired/used token
/// maps to a uniform `401` (see the handler's doc comment) — no oracle on
/// *why* it failed.
#[derive(Deserialize)]
pub struct AuthVerifyRequest {
    pub token: String,
    pub new_password: String,
}

/// Manual `Debug`: redacts both fields — `token` is a live single-use bearer
/// credential and `new_password` is, well, a password; same posture as
/// every other auth-secret DTO in this file.
impl std::fmt::Debug for AuthVerifyRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthVerifyRequest")
            .field("token", &"<redacted>")
            .field("new_password", &"<redacted>")
            .finish()
    }
}

/// Body of `POST /auth/magiclink/verify` (item 138): redeem a magic-link
/// login token (from the emailed link) for a real session — response is a
/// plain [`AuthLoginResponse`], the same shape every other login path uses.
#[derive(Deserialize)]
pub struct AuthMagicLinkVerifyRequest {
    pub token: String,
}

/// Manual `Debug`: redacts `token` — a live single-use bearer credential.
impl std::fmt::Debug for AuthMagicLinkVerifyRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthMagicLinkVerifyRequest")
            .field("token", &"<redacted>")
            .finish()
    }
}

/// Response of `GET /auth/meta`.
#[derive(Debug, Serialize)]
pub struct AuthMetaResponse {
    /// `true` when no users are registered — all RLS is inactive and any
    /// valid JWT (or no JWT if middleware is disabled) has full access.
    pub open_mode: bool,
    /// The four grantable privilege operations.
    pub privilege_types: Vec<&'static str>,
    /// The five policy scopes (SELECT/INSERT/UPDATE/DELETE/ALL).
    pub policy_operations: Vec<&'static str>,
    /// All queryable catalog relations.
    pub catalog_tables: Vec<&'static str>,
    /// Whether dev login (`POST /auth/login`) is enabled on this server.
    pub dev_login_enabled: bool,
    /// Whether self-service signup (`POST /auth/signup`, item 121 A3) is
    /// enabled on this server (`UNIDB_ALLOW_SIGNUP=1`).
    pub signup_enabled: bool,
}

/// Response of `GET /auth/whoami`.
#[derive(Debug, Serialize)]
pub struct WhoamiResponse {
    pub user: Option<String>,
    pub is_superuser: bool,
    pub roles: Vec<String>,
    /// Per-table privileges: `[{table, ops: ["SELECT", ...]}]`.
    pub privileges: Vec<WhoamiPrivilege>,
    /// `true` when open/bootstrap mode (user is None or no users configured).
    pub open_mode: bool,
    /// Item 127 (Workstream D4): whether the caller has TOTP MFA enabled.
    /// Never the secret or recovery codes — just the boolean a client UI
    /// needs to show "MFA: on/off". `false` for the implicit superuser
    /// (token without `sub`), which has no named-user MFA state to carry.
    pub mfa_enabled: bool,
}

// ── item 127 (Workstream D4): TOTP-based MFA ───────────────────────────────

/// Response of `POST /auth/mfa/enroll` (authenticated): a fresh, still
/// **pending** TOTP secret plus the `otpauth://` URI a client renders as a
/// QR code. MFA is not yet enabled — the caller must confirm with a live
/// code via `POST /auth/mfa/verify`.
pub struct MfaEnrollResponse {
    /// Base32-encoded 160-bit TOTP secret. Shown to the user exactly once —
    /// never persisted in the clear anywhere the client can re-fetch it.
    pub secret: String,
    /// `otpauth://totp/unidb:<user>?secret=<base32>&issuer=unidb` — feed
    /// directly to a QR-code renderer.
    pub otpauth_url: String,
}

impl Serialize for MfaEnrollResponse {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut s = serializer.serialize_struct("MfaEnrollResponse", 2)?;
        s.serialize_field("secret", &self.secret)?;
        s.serialize_field("otpauth_url", &self.otpauth_url)?;
        s.end()
    }
}

/// Manual `Debug`: redacts `secret` — a TOTP seed is exactly as sensitive as
/// a password (whoever holds it can mint valid codes forever), same posture
/// as [`AuthLoginResponse`]'s bearer-token redaction.
impl std::fmt::Debug for MfaEnrollResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MfaEnrollResponse")
            .field("secret", &"<redacted>")
            .field("otpauth_url", &"<redacted>")
            .finish()
    }
}

/// Body of `POST /auth/mfa/verify` (authenticated): confirm a pending
/// enrollment with a live 6-digit TOTP code.
#[derive(Deserialize)]
pub struct MfaVerifyRequest {
    pub code: String,
}

/// Manual `Debug`: redacts `code` — a live TOTP code is a bearer credential
/// for a ~90 s window, same posture as every other auth-secret DTO here.
impl std::fmt::Debug for MfaVerifyRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MfaVerifyRequest")
            .field("code", &"<redacted>")
            .finish()
    }
}

/// Response of `POST /auth/mfa/verify` on success: MFA is now enabled, and
/// `recovery_codes` are the plaintext one-time codes — shown **exactly
/// once**; only their hashes are ever persisted (see `authz::RoleStore::
/// mfa_confirm`).
#[derive(Serialize)]
pub struct MfaVerifyResponse {
    pub enabled: bool,
    pub recovery_codes: Vec<String>,
}

/// Manual `Debug`: redacts `recovery_codes` to a count — each one is a live,
/// still-unused bearer credential.
impl std::fmt::Debug for MfaVerifyResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MfaVerifyResponse")
            .field("enabled", &self.enabled)
            .field(
                "recovery_codes",
                &format!("<{} redacted>", self.recovery_codes.len()),
            )
            .finish()
    }
}

/// Body of `POST /auth/mfa/disable` (authenticated). `code` is required
/// unless the caller is a superuser (checked server-side, not by this DTO) —
/// see the handler's doc comment.
#[derive(Deserialize)]
pub struct MfaDisableRequest {
    #[serde(default)]
    pub code: Option<String>,
}

/// Manual `Debug`: redacts `code`, same posture as [`MfaVerifyRequest`].
impl std::fmt::Debug for MfaDisableRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MfaDisableRequest")
            .field("code", &self.code.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// Body of `POST /auth/mfa/challenge` (public, no JWT — the challenge itself
/// is the pre-session credential): redeem an MFA login challenge issued by
/// `POST /auth/login` with a TOTP code (or a one-time recovery code) to
/// receive the real session.
#[derive(Deserialize)]
pub struct MfaChallengeRequest {
    pub challenge: String,
    pub code: String,
}

/// Manual `Debug`: redacts both fields — the challenge token is a bearer
/// credential (narrow-window, but still live) and `code` is a TOTP/recovery
/// secret, same posture as every other auth-secret DTO here.
impl std::fmt::Debug for MfaChallengeRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MfaChallengeRequest")
            .field("challenge", &"<redacted>")
            .field("code", &"<redacted>")
            .finish()
    }
}

/// `POST /auth/login`'s response when the authenticating user has MFA
/// enabled: instead of a real session, the caller gets a short-lived,
/// single-use challenge to redeem at `POST /auth/mfa/challenge`. Wraps
/// [`AuthLoginResponse`] so a non-MFA login's wire shape is completely
/// unchanged (back-compat contract) — only an MFA-enabled login's response
/// shape differs, and it differs in an unambiguous way (`mfa_required: true`,
/// no token fields at all).
pub enum AuthLoginOutcome {
    /// A non-MFA login (or a redeemed MFA challenge): the normal
    /// access+refresh token pair, byte-identical wire shape to pre-127.
    Session(AuthLoginResponse),
    /// An MFA-enabled login after a correct password: no tokens yet.
    MfaRequired {
        /// Opaque, single-use challenge token — pass to `POST
        /// /auth/mfa/challenge` alongside the TOTP/recovery code.
        challenge: String,
        /// Seconds until the challenge expires.
        expires_in: u64,
    },
}

impl Serialize for AuthLoginOutcome {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            AuthLoginOutcome::Session(resp) => resp.serialize(serializer),
            AuthLoginOutcome::MfaRequired {
                challenge,
                expires_in,
            } => {
                use serde::ser::SerializeStruct;
                let mut s = serializer.serialize_struct("AuthLoginOutcome", 3)?;
                s.serialize_field("mfa_required", &true)?;
                s.serialize_field("challenge", challenge)?;
                s.serialize_field("expires_in", expires_in)?;
                s.end()
            }
        }
    }
}

/// Manual `Debug`: delegates to [`AuthLoginResponse`]'s own redacted `Debug`
/// for the `Session` case; redacts `challenge` for the `MfaRequired` case —
/// same "no live credential in a `{:?}`" posture as everywhere else in this
/// module.
impl std::fmt::Debug for AuthLoginOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthLoginOutcome::Session(resp) => resp.fmt(f),
            AuthLoginOutcome::MfaRequired { expires_in, .. } => f
                .debug_struct("AuthLoginOutcome::MfaRequired")
                .field("challenge", &"<redacted>")
                .field("expires_in", expires_in)
                .finish(),
        }
    }
}

// ── item 128 (Workstream D1): OAuth 2.0 social login ───────────────────────

/// Query params for `GET /auth/oauth/{provider}/callback` (item 128). `code`
/// is present on a successful provider redirect; `error` is present instead
/// when the user denied consent or the provider otherwise refused
/// (`?error=access_denied&state=...`). `state` should always be present —
/// its absence is treated as an invalid/garbage callback, same posture as
/// an unknown state value.
#[derive(Deserialize)]
pub struct OAuthCallbackQuery {
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

/// Manual `Debug`: redacts `code` (a single-use bearer credential for the
/// token exchange) and `state` (matches a server-side PKCE verifier record),
/// same "no live credential in `{:?}`" posture as every other auth DTO here.
/// `error` is provider-supplied diagnostic text, not a secret — shown as-is.
impl std::fmt::Debug for OAuthCallbackQuery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthCallbackQuery")
            .field("code", &self.code.as_ref().map(|_| "<redacted>"))
            .field("state", &self.state.as_ref().map(|_| "<redacted>"))
            .field("error", &self.error)
            .finish()
    }
}

#[derive(Debug, Serialize)]
pub struct WhoamiPrivilege {
    pub table: String,
    pub ops: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct CypherRequest {
    pub query: String,
}

#[derive(Debug, Serialize)]
pub struct RowIdResponse {
    pub row_id: RowId,
}

#[derive(Debug, Deserialize)]
pub struct CreateEdgeRequest {
    pub from_id: i64,
    pub to_id: i64,
    pub edge_type: String,
    /// Raw JSON, re-serialized to text for `Engine::create_edge`'s `&str`
    /// signature — defaults to `{}` if omitted, matching `create_edge`'s
    /// own tests' convention.
    #[serde(default = "default_props")]
    pub props: Json,
}

fn default_props() -> Json {
    Json::Object(Map::new())
}

#[derive(Debug, Deserialize)]
pub struct DeleteEdgeRequest {
    pub from_id: i64,
}

#[derive(Debug, Deserialize)]
pub struct SetIndexRequest {
    pub table: String,
    pub column: String,
    pub kind: Option<IndexKind>,
}

// ── table introspection (S1, `GET /tables`) ────────────────────────────────

/// One table's schema in the `GET /tables` response. Internal `__…__` tables
/// (`__events__`/`__consumers__`/`__edges__`/`__lobs__`) are omitted entirely
/// by the handler — this shape describes only user tables.
#[derive(Debug, Serialize, PartialEq)]
pub struct TableInfo {
    pub name: String,
    pub columns: Vec<ColumnInfo>,
}

/// One column's schema within [`TableInfo`].
#[derive(Debug, Serialize, PartialEq)]
pub struct ColumnInfo {
    pub name: String,
    /// Human-readable type name (`"int"`, `"text"`, `"vector(384)"`, …).
    #[serde(rename = "type")]
    pub ty: String,
    /// `false` iff the column is `NOT NULL` or `PRIMARY KEY`.
    pub nullable: bool,
    /// The column's secondary-index kind, or `null` if unindexed.
    pub index: Option<&'static str>,
}

/// Render a [`ColumnType`] as the human-readable name used on the wire. Kept in
/// the REST layer (not `catalog.rs`) so the wire vocabulary is owned here — the
/// engine's on-disk enum can evolve without silently changing the API contract.
fn column_type_name(ty: &ColumnType) -> String {
    match ty {
        ColumnType::Int64 => "int".to_string(),
        ColumnType::Text => "text".to_string(),
        ColumnType::Bool => "bool".to_string(),
        ColumnType::Json => "json".to_string(),
        ColumnType::Vector(n) => format!("vector({n})"),
        ColumnType::Decimal(p, s) => format!("decimal({p},{s})"),
        ColumnType::Timestamp => "timestamp".to_string(),
        ColumnType::Float => "float".to_string(),
        ColumnType::Uuid => "uuid".to_string(),
        ColumnType::Bytea => "bytea".to_string(),
        ColumnType::Date => "date".to_string(),
        ColumnType::Time => "time".to_string(),
    }
}

/// Stable wire name for a secondary-index kind. `Hnsw` keeps its historical
/// name even though it is the durable IVF-Flat index since P3.c (matching the
/// catalog/SQL surface, see `catalog::IndexKind`).
fn index_kind_name(kind: IndexKind) -> &'static str {
    match kind {
        IndexKind::Hnsw => "hnsw",
        IndexKind::FullText => "fulltext",
        IndexKind::BTree => "btree",
        IndexKind::Csr => "csr",
    }
}

/// Build the introspection view of one table. Dropped columns (P2.c logical
/// tombstones) are excluded — they are invisible to `SELECT *`, so they are
/// invisible here too.
pub fn table_def_to_info(def: &TableDef) -> TableInfo {
    let columns = def
        .columns
        .iter()
        .filter(|c| !c.dropped)
        .map(|c| ColumnInfo {
            name: c.name.clone(),
            ty: column_type_name(&c.ty),
            nullable: !(c.constraints.not_null || c.constraints.primary_key),
            index: c.index.map(index_kind_name),
        })
        .collect();
    TableInfo {
        name: def.name.clone(),
        columns,
    }
}

/// Whether a table is an internal engine table (`__events__`, `__edges__`,
/// `__lobs__`, `__consumers__`, …) hidden from `GET /tables`. All such tables
/// share the reserved `__…__` naming convention.
pub fn is_internal_table(name: &str) -> bool {
    name.starts_with("__")
}

/// Body of `POST /batch-sql` (item 99): N independent one-shot statements
/// sent in a single HTTP round-trip to amortise the per-request overhead.
/// Each statement is auto-committed independently — there is no shared
/// transaction across the batch.
#[derive(Debug, Deserialize)]
pub struct BatchSqlRequest {
    /// Ordered list of SQL statements to execute (max 256).
    pub statements: Vec<String>,
    /// When `true`, stop at the first error; all remaining slots get a
    /// `null` result and `"skipped"` error string. When `false` (default),
    /// every statement is attempted regardless of earlier failures.
    #[serde(default)]
    pub stop_on_error: bool,
}

/// Response of `POST /batch-sql`: parallel arrays of results and errors,
/// one slot per input statement (in order).
///
/// A slot where execution succeeded carries a non-null result JSON object
/// (same shape as a single entry in `POST /sql`'s `"results"` array) and
/// a `null` error.  A slot where execution failed carries a `null` result
/// and the error string.  A slot that was skipped (because `stop_on_error`
/// was `true` and an earlier slot failed) carries a `null` result and the
/// fixed string `"skipped"`.
#[derive(Debug, Serialize)]
pub struct BatchSqlResponse {
    /// One result JSON value per statement (null on failure / skip).
    pub results: Vec<Option<serde_json::Value>>,
    /// One error string per statement (null on success).
    pub errors: Vec<Option<String>>,
}

#[derive(Debug, Deserialize)]
pub struct AckEventsRequest {
    pub consumer: String,
    pub up_to_seq: i64,
}

// ── replication (P6.b) ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CreateSlotRequest {
    pub name: String,
    /// `true` for a synchronous slot (commit waits for the consumer, P6.c);
    /// defaults to async.
    #[serde(default)]
    pub sync: bool,
}

#[derive(Debug, Deserialize)]
pub struct AdvanceSlotRequest {
    /// The LSN the consumer has durably applied up to.
    pub lsn: u64,
}

#[derive(Debug, Deserialize)]
pub struct StreamQuery {
    /// Ship records strictly after this LSN (default 0 = from the beginning of
    /// retained WAL).
    #[serde(default)]
    pub from_lsn: u64,
}

/// JSON view of a replication slot.
pub fn slot_to_json(info: &crate::replication::SlotInfo) -> serde_json::Value {
    serde_json::json!({
        "name": info.name,
        "restart_lsn": info.restart_lsn,
        "kind": match info.kind {
            crate::replication::SlotKind::Async => "async",
            crate::replication::SlotKind::Sync => "sync",
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn json_to_literal_maps_scalars_as_data() {
        assert_eq!(json_to_literal(&Json::Null), Literal::Null);
        assert_eq!(json_to_literal(&json!(true)), Literal::Bool(true));
        assert_eq!(json_to_literal(&json!(42)), Literal::Int(42));
        assert_eq!(json_to_literal(&json!(1.5)), Literal::Float(1.5));
        // A string — including one that looks like SQL — is plain text data.
        assert_eq!(
            json_to_literal(&json!("'; DROP TABLE t; --")),
            Literal::Text("'; DROP TABLE t; --".to_string())
        );
        // A numeric array is a vector literal.
        assert_eq!(
            json_to_literal(&json!([0.1, 0.2])),
            Literal::Vector(vec![0.1, 0.2])
        );
    }

    #[test]
    fn sql_request_params_default_to_empty() {
        let req: SqlRequest = serde_json::from_value(json!({"sql": "SELECT 1"})).unwrap();
        assert!(req.params.is_empty());
        let req2: SqlRequest =
            serde_json::from_value(json!({"sql": "SELECT $1", "params": [7]})).unwrap();
        assert_eq!(req2.params.len(), 1);
    }

    #[test]
    fn rows_result_carries_column_names() {
        let result = ExecResult::Rows {
            columns: vec!["id".to_string(), "name".to_string()],
            rows: vec![vec![Literal::Int(1), Literal::Text("alice".to_string())]],
        };
        assert_eq!(
            exec_result_to_json(&result),
            json!({
                "type": "rows",
                "columns": ["id", "name"],
                "rows": [[1, "alice"]]
            })
        );
    }
}
