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
fn map_status(err: &DbError) -> (StatusCode, &'static str) {
    match err {
        DbError::TableNotFound(_) => (StatusCode::NOT_FOUND, "TABLE_NOT_FOUND"),
        DbError::ColumnNotFound { .. } => (StatusCode::NOT_FOUND, "COLUMN_NOT_FOUND"),
        DbError::NoVisibleVersion { .. } => (StatusCode::NOT_FOUND, "NOT_FOUND"),

        DbError::TableAlreadyExists(_) => (StatusCode::CONFLICT, "TABLE_ALREADY_EXISTS"),
        DbError::WriteConflict { .. } => (StatusCode::CONFLICT, "WRITE_CONFLICT"),
        DbError::SerializationFailure { .. } => (StatusCode::CONFLICT, "SERIALIZATION_FAILURE"),
        DbError::Deadlock { .. } => (StatusCode::CONFLICT, "DEADLOCK"),

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
