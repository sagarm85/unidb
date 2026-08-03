//! `DbError` -> HTTP mapping. `ApiError` is a newtype, not an
//! `impl IntoResponse for DbError` directly on `crate::error::DbError` —
//! `error.rs` is used by the default, non-`server` build too and must stay
//! completely axum-agnostic.

use axum::{http::StatusCode, response::IntoResponse, Json};
use serde::Serialize;

use crate::error::DbError;
use crate::server::{cursor::CursorError, txn_session::SessionError};

/// A response-ready error: either an engine [`DbError`] (mapped through
/// [`map_status`]) or a server-layer error with its own status + code —
/// transaction-session and cursor failures (R1/R4) are HTTP-protocol
/// concepts the engine's error enum deliberately knows nothing about.
pub enum ApiError {
    Db(DbError),
    Api {
        status: StatusCode,
        code: &'static str,
        message: String,
    },
}

impl ApiError {
    /// A `400 Bad Request` with a server-layer code (bad header syntax,
    /// oversized batch, …).
    pub fn bad_request(code: &'static str, message: impl Into<String>) -> Self {
        ApiError::Api {
            status: StatusCode::BAD_REQUEST,
            code,
            message: message.into(),
        }
    }

    /// A `409 Conflict` — used for bucket-not-empty and other precondition
    /// failures at the storage layer.
    pub fn conflict(code: &'static str, message: impl Into<String>) -> Self {
        ApiError::Api {
            status: StatusCode::CONFLICT,
            code,
            message: message.into(),
        }
    }

    /// A `503 Service Unavailable` — used when the storage service is not
    /// configured (item 31's 503 contract).
    pub fn service_unavailable(code: &'static str, message: impl Into<String>) -> Self {
        ApiError::Api {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code,
            message: message.into(),
        }
    }

    /// A `500 Internal Server Error` for a server-side failure that isn't a
    /// `DbError` (e.g. a log-file read error behind `GET /logs`).
    pub fn internal(code: &'static str, message: impl Into<String>) -> Self {
        ApiError::Api {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code,
            message: message.into(),
        }
    }
}

/// Map `StorageApiError` → HTTP status for the `/storage/*` handlers (item 31).
/// `StorageApiError` is the trait-layer error type from `crate::storage_api` —
/// no `unidb-storage` dep needed here.
impl From<crate::storage_api::StorageApiError> for ApiError {
    fn from(err: crate::storage_api::StorageApiError) -> Self {
        use crate::storage_api::StorageApiError as SE;
        match err {
            SE::Engine(msg) => ApiError::internal("INTERNAL_ERROR", msg),
            SE::NotFound(msg) => ApiError::Api {
                status: StatusCode::NOT_FOUND,
                code: "STORAGE_NOT_FOUND",
                message: msg,
            },
            SE::BucketNotEmpty(name) => ApiError::Api {
                status: StatusCode::CONFLICT,
                code: "BUCKET_NOT_EMPTY",
                message: format!("bucket '{name}' still contains objects"),
            },
            SE::Config(msg) => ApiError::Api {
                status: StatusCode::SERVICE_UNAVAILABLE,
                code: "STORAGE_CONFIG_ERROR",
                message: msg,
            },
            SE::Store(msg) => ApiError::Api {
                status: StatusCode::BAD_GATEWAY,
                code: "OBJECT_STORE_ERROR",
                message: msg,
            },
            // F1 (item 120, Workstream F): per-object authorization denial —
            // not the owner, the bucket isn't public, and the caller doesn't
            // bypass like a superuser/service_role.
            SE::Forbidden(msg) => ApiError::Api {
                status: StatusCode::FORBIDDEN,
                code: "STORAGE_FORBIDDEN",
                message: msg,
            },
            SE::Join => ApiError::internal("INTERNAL_ERROR", "storage task join failure"),
        }
    }
}

impl From<DbError> for ApiError {
    fn from(err: DbError) -> Self {
        ApiError::Db(err)
    }
}

impl From<SessionError> for ApiError {
    fn from(err: SessionError) -> Self {
        let (status, code, message) = match err {
            SessionError::NotFound(xid) => (
                StatusCode::NOT_FOUND,
                "TXN_NOT_FOUND",
                format!("no open transaction session {xid} (finished, expired, or never begun — session ids do not survive a restart)"),
            ),
            SessionError::Busy(xid) => (
                StatusCode::CONFLICT,
                "TXN_BUSY",
                format!("transaction session {xid} is executing another request; a session runs one statement at a time"),
            ),
            SessionError::Forbidden(xid) => (
                StatusCode::FORBIDDEN,
                "TXN_FORBIDDEN",
                format!("transaction session {xid} belongs to a different principal"),
            ),
        };
        ApiError::Api {
            status,
            code,
            message,
        }
    }
}

/// Item 131 (Workstream I2): CAPTCHA gate failure -> HTTP. `TokenRequired`
/// is a client-request-shape problem (`400`); `Failed` deliberately covers
/// both "wrong/expired token" and "verifier misconfigured/unreachable" with
/// one uniform outward status/code — see `captcha.rs`'s module doc for why
/// that's the correct fail-closed, no-oracle posture.
impl From<crate::server::captcha::CaptchaError> for ApiError {
    fn from(err: crate::server::captcha::CaptchaError) -> Self {
        use crate::server::captcha::CaptchaError as CE;
        match err {
            CE::TokenRequired => ApiError::Api {
                status: StatusCode::BAD_REQUEST,
                code: "CAPTCHA_TOKEN_REQUIRED",
                message: "captcha_token is required".into(),
            },
            CE::Failed => ApiError::Api {
                status: StatusCode::FORBIDDEN,
                code: "CAPTCHA_FAILED",
                message: "captcha verification failed".into(),
            },
        }
    }
}

impl From<CursorError> for ApiError {
    fn from(err: CursorError) -> Self {
        let (status, code, message) = match err {
            CursorError::NotFound(id) => (
                StatusCode::NOT_FOUND,
                "CURSOR_NOT_FOUND",
                format!("no open cursor {id} (exhausted, expired, or never created)"),
            ),
            CursorError::Forbidden(id) => (
                StatusCode::FORBIDDEN,
                "CURSOR_FORBIDDEN",
                format!("cursor {id} belongs to a different principal"),
            ),
        };
        ApiError::Api {
            status,
            code,
            message,
        }
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
    code: &'static str,
}

/// Maps a `DbError` to `(HTTP status, machine-readable code)`. Client-facing
/// variants are listed individually and exhaustively; everything else
/// (low-level storage/recovery errors a well-formed request should never
/// trigger) falls into one grouped `_` catch-all mapped to 500 — documented
/// here explicitly so a future `DbError` addition that *should* get its own
/// 4xx status doesn't silently default to 500 unnoticed.
pub(crate) fn map_status(err: &DbError) -> (StatusCode, &'static str) {
    match err {
        DbError::TableNotFound(_) => (StatusCode::NOT_FOUND, "TABLE_NOT_FOUND"),
        DbError::ColumnNotFound { .. } => (StatusCode::NOT_FOUND, "COLUMN_NOT_FOUND"),
        DbError::NoVisibleVersion { .. } => (StatusCode::NOT_FOUND, "NOT_FOUND"),
        // Item 148: named types (CREATE TYPE/CREATE DOMAIN) mirror the
        // TableNotFound/TableAlreadyExists status mapping exactly.
        DbError::UnknownType(_) => (StatusCode::NOT_FOUND, "UNKNOWN_TYPE"),

        DbError::TableAlreadyExists(_) => (StatusCode::CONFLICT, "TABLE_ALREADY_EXISTS"),
        DbError::TypeAlreadyExists(_) => (StatusCode::CONFLICT, "TYPE_ALREADY_EXISTS"),
        DbError::TypeInUse { .. } => (StatusCode::CONFLICT, "TYPE_IN_USE"),
        DbError::WriteConflict { .. } => (StatusCode::CONFLICT, "WRITE_CONFLICT"),
        DbError::SerializationFailure { .. } => (StatusCode::CONFLICT, "SERIALIZATION_FAILURE"),
        DbError::Deadlock { .. } => (StatusCode::CONFLICT, "DEADLOCK"),

        // A malformed named-type definition (bad name, shadowed builtin,
        // empty/duplicate enum labels, bad domain base type) is a client
        // request error, same class as SqlPlan/SqlUnsupported above.
        DbError::InvalidNamedType(_) => (StatusCode::BAD_REQUEST, "INVALID_NAMED_TYPE"),

        // Resource control (P5.f): the query hit its time budget or was
        // cancelled — both are request-scoped, not server faults.
        DbError::QueryTimeout { .. } => (StatusCode::REQUEST_TIMEOUT, "QUERY_TIMEOUT"),
        DbError::QueryCancelled => (StatusCode::REQUEST_TIMEOUT, "QUERY_CANCELLED"),

        DbError::SqlParse(_) => (StatusCode::BAD_REQUEST, "SQL_PARSE_ERROR"),
        DbError::SqlPlan(_) => (StatusCode::BAD_REQUEST, "SQL_PLAN_ERROR"),
        DbError::SqlUnsupported(_) => (StatusCode::BAD_REQUEST, "SQL_UNSUPPORTED"),
        // Constraint violations (M11) are client errors — the request asked
        // to write data the schema forbids.
        DbError::NotNullViolation { .. } => (StatusCode::BAD_REQUEST, "NOT_NULL_VIOLATION"),
        DbError::UniqueViolation { .. } => (StatusCode::CONFLICT, "UNIQUE_VIOLATION"),
        DbError::CheckViolation { .. } => (StatusCode::BAD_REQUEST, "CHECK_VIOLATION"),
        DbError::ForeignKeyViolation { .. } => (StatusCode::BAD_REQUEST, "FOREIGN_KEY_VIOLATION"),
        DbError::TxnNotActive { .. } => (StatusCode::BAD_REQUEST, "TXN_NOT_ACTIVE"),
        DbError::TxnAlreadyFinished { .. } => (StatusCode::BAD_REQUEST, "TXN_ALREADY_FINISHED"),
        DbError::BadPageSize(_) => (StatusCode::BAD_REQUEST, "BAD_PAGE_SIZE"),

        // Replication slot management (P6.b): a bad slot request (duplicate/
        // unknown name) is a client error, not a server fault.
        DbError::Replication(_) => (StatusCode::BAD_REQUEST, "REPLICATION_ERROR"),

        // Authorization (P6.e): a bad users/roles/GRANT statement is a client
        // error; a missing privilege is 403 Forbidden.
        DbError::Authz(_) => (StatusCode::BAD_REQUEST, "AUTHZ_ERROR"),
        DbError::PermissionDenied(_) => (StatusCode::FORBIDDEN, "PERMISSION_DENIED"),

        // Item 126 (Workstream I4): no HTTP route drives `apply_migrations`
        // today (it's a CLI/embedded-API-only surface) — mapped for match
        // exhaustiveness against a future migrations route. A bad migration
        // file / checksum drift / failing statement is a request-shaped
        // problem, not a server fault.
        DbError::Migration(_) => (StatusCode::BAD_REQUEST, "MIGRATION_ERROR"),

        // Item 144: a malformed cron expression is a client-supplied
        // registration error, not a server fault.
        DbError::InvalidCronSchedule(_) => (StatusCode::BAD_REQUEST, "INVALID_CRON_SCHEDULE"),

        // Item 147: a malformed stored-function registration is a
        // client-supplied error, not a server fault.
        DbError::InvalidFunctionDef(_) => (StatusCode::BAD_REQUEST, "INVALID_FUNCTION_DEF"),

        // Durability failure (P1.b, fsyncgate) is fatal for the session — the
        // engine can no longer guarantee writes reach disk and must be
        // restarted. 503 signals the service is (temporarily) unable to handle
        // the request, distinct from a generic 500, and mirrors how
        // EngineUnavailable is a process-restart condition.
        DbError::DurabilityFailure(_) => (StatusCode::SERVICE_UNAVAILABLE, "DURABILITY_FAILURE"),

        // Low-level storage/recovery/transport errors a well-formed client
        // request should never trigger.
        DbError::Io(_)
        | DbError::BadMagic { .. }
        | DbError::BadVersion(_)
        | DbError::ChecksumMismatch { .. }
        | DbError::WalCorrupt { .. }
        | DbError::BufferPoolFull
        | DbError::PageNotFound { .. }
        | DbError::HeapFull { .. }
        | DbError::SlotOutOfRange { .. }
        | DbError::TupleDeleted { .. }
        | DbError::Recovery(_)
        | DbError::ControlFileCorrupt(_)
        | DbError::CatalogCorrupt(_)
        | DbError::EngineUnavailable => (StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR"),
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let (status, code, error) = match self {
            ApiError::Db(err) => {
                let (status, code) = map_status(&err);
                (status, code, err.to_string())
            }
            ApiError::Api {
                status,
                code,
                message,
            } => (status, code, message),
        };
        (status, Json(ErrorBody { error, code })).into_response()
    }
}
