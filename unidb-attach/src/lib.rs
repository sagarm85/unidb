//! `unidb-attach`: blocking HTTP client for the UniDB REST server (M8).
//!
//! # Quick start
//!
//! ```no_run
//! use unidb_attach::AttachClient;
//!
//! let client = AttachClient::new("http://localhost:7777", "<jwt>").unwrap();
//! let results = client.execute_sql("CREATE TABLE t (id INT, name TEXT)").unwrap();
//! ```
//!
//! # API shape vs. embedded `Engine`
//!
//! Each method is a **one-shot call**: the server wraps it in a single
//! `begin -> execute -> commit` internally (see `handlers.rs`).  There are
//! no explicit `begin`/`commit` calls here — multi-request transaction
//! sessions don't exist over HTTP.  Multi-statement atomicity is available
//! by passing `;`-separated SQL to `execute_sql`, exactly as the REST API
//! documents.  This is a deliberate design difference from the embedded
//! `Engine`, not an oversight.
//!
//! # Known limitations (v1)
//!
//! - No multi-request transaction sessions.
//! - `vacuum_events`, `set_rls_policy`, and `flush` are not exposed — the
//!   server has no REST routes for them.
//! - Blocking I/O: each call blocks the calling thread.

#![forbid(unsafe_code)]

use reqwest::blocking::Client;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use thiserror::Error;

// ── Error type ───────────────────────────────────────────────────────────────

/// All errors that an `AttachClient` call can return.
///
/// Named variants map 1-to-1 with the server's `code` field in its error
/// response body (see `server/error.rs`).  `Http` and `Json` cover
/// network-level and deserialization failures that have no server-side
/// equivalent.
#[derive(Debug, Error)]
pub enum AttachError {
    /// Network or transport error (connection refused, timeout, TLS, …).
    #[error("network error: {0}")]
    Http(#[from] reqwest::Error),

    /// Unexpected response shape — JSON deserialization failed.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// The `jwt_token` string could not be encoded as an HTTP header value.
    #[error("invalid JWT token: not a valid header value")]
    InvalidToken,

    // ── server-side error codes (mirrors server/error.rs::map_status) ───────
    #[error("table not found: {0}")]
    TableNotFound(String),

    #[error("column not found: {0}")]
    ColumnNotFound(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("table already exists: {0}")]
    TableAlreadyExists(String),

    #[error("write conflict: {0}")]
    WriteConflict(String),

    #[error("serialization failure: {0}")]
    SerializationFailure(String),

    #[error("SQL parse error: {0}")]
    SqlParse(String),

    #[error("SQL plan error: {0}")]
    SqlPlan(String),

    #[error("SQL unsupported: {0}")]
    SqlUnsupported(String),

    /// Catch-all for server error codes not listed above (e.g. `INTERNAL_ERROR`).
    #[error("server error {status} ({code}): {message}")]
    Api {
        status: u16,
        code: String,
        message: String,
    },
}

impl AttachError {
    fn from_api(status: u16, code: &str, message: String) -> Self {
        match code {
            "TABLE_NOT_FOUND" => Self::TableNotFound(message),
            "COLUMN_NOT_FOUND" => Self::ColumnNotFound(message),
            "NOT_FOUND" => Self::NotFound(message),
            "TABLE_ALREADY_EXISTS" => Self::TableAlreadyExists(message),
            "WRITE_CONFLICT" => Self::WriteConflict(message),
            "SERIALIZATION_FAILURE" => Self::SerializationFailure(message),
            "SQL_PARSE_ERROR" => Self::SqlParse(message),
            "SQL_PLAN_ERROR" => Self::SqlPlan(message),
            "SQL_UNSUPPORTED" => Self::SqlUnsupported(message),
            _ => Self::Api {
                status,
                code: code.to_string(),
                message,
            },
        }
    }
}

// ── Public types ─────────────────────────────────────────────────────────────

/// Row location returned by `insert` and `update`.
///
/// Mirrors `unidb::heap::RowId`'s wire format (`{"page_id": u32, "slot": u16}`).
/// Defined independently here to avoid pulling in the full `unidb` dependency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RowId {
    pub page_id: u32,
    pub slot: u16,
}

/// One statement's result, decoded from the server's `{"type": "..."}` envelope.
#[derive(Debug)]
pub enum ExecResult {
    CreatedTable,
    CreatedIndex,
    Inserted {
        count: u64,
    },
    Updated {
        count: u64,
    },
    Deleted {
        count: u64,
    },
    /// Each inner `Vec<Json>` is one row; values are in column declaration order.
    Rows(Vec<Vec<Json>>),
}

/// Secondary index kind. Serialization matches `unidb::catalog::IndexKind`'s
/// default serde form (`"Hnsw"` / `"FullText"` / `"BTree"`). Does not
/// include `Csr` (M7) — that variant is engine-managed only, never settable
/// via `CREATE INDEX`/`POST /indexes`, so there is nothing for a REST client
/// to ever send or receive for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IndexKind {
    Hnsw,
    FullText,
    BTree,
}

/// One resolved edge, returned by `edges_from`.
///
/// Mirrors `unidb::graph::edges::Edge`'s wire format.
#[derive(Debug, Deserialize)]
pub struct EdgeResult {
    pub row_id: RowId,
    pub to_id: i64,
    pub edge_type: String,
    /// Raw JSON text, same representation as `Literal::Json` in the engine.
    pub props: String,
}

// ── Internal helpers ──────────────────────────────────────────────────────────

fn check_response(
    resp: reqwest::blocking::Response,
) -> Result<reqwest::blocking::Response, AttachError> {
    if resp.status().is_success() {
        return Ok(resp);
    }
    let status = resp.status().as_u16();
    // Parse `{"error": "...", "code": "..."}`. On any parse failure fall
    // through to the generic catch-all with an empty message.
    let body: Json = resp.json().unwrap_or(Json::Null);
    let code = body["code"].as_str().unwrap_or("UNKNOWN");
    let message = body["error"].as_str().unwrap_or("").to_string();
    Err(AttachError::from_api(status, code, message))
}

fn decode_exec_result(v: &Json) -> Result<ExecResult, AttachError> {
    match v["type"].as_str().unwrap_or("") {
        "created_table" => Ok(ExecResult::CreatedTable),
        "created_index" => Ok(ExecResult::CreatedIndex),
        "inserted" => Ok(ExecResult::Inserted {
            count: v["count"].as_u64().unwrap_or(0),
        }),
        "updated" => Ok(ExecResult::Updated {
            count: v["count"].as_u64().unwrap_or(0),
        }),
        "deleted" => Ok(ExecResult::Deleted {
            count: v["count"].as_u64().unwrap_or(0),
        }),
        "rows" => {
            let rows = v["rows"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .map(|row| row.as_array().cloned().unwrap_or_default())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            Ok(ExecResult::Rows(rows))
        }
        other => Err(AttachError::Api {
            status: 200,
            code: "UNKNOWN_RESULT_TYPE".to_string(),
            message: format!("unknown result type: {other}"),
        }),
    }
}

fn decode_exec_results(body: &Json) -> Result<Vec<ExecResult>, AttachError> {
    body["results"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(decode_exec_result)
                .collect::<Result<Vec<_>, _>>()
        })
        .unwrap_or_else(|| Ok(vec![]))
}

fn decode_row_id(v: &Json) -> Result<RowId, AttachError> {
    let page_id = v["page_id"].as_u64().ok_or_else(|| AttachError::Api {
        status: 200,
        code: "BAD_ROW_ID".to_string(),
        message: "missing page_id in row_id response".to_string(),
    })? as u32;
    let slot = v["slot"].as_u64().ok_or_else(|| AttachError::Api {
        status: 200,
        code: "BAD_ROW_ID".to_string(),
        message: "missing slot in row_id response".to_string(),
    })? as u16;
    Ok(RowId { page_id, slot })
}

// ── Client ────────────────────────────────────────────────────────────────────

/// Blocking HTTP client for a running UniDB REST server.
///
/// Cheaply cloneable — the inner `reqwest::blocking::Client` shares its
/// connection pool across clones.
#[derive(Clone)]
pub struct AttachClient {
    base_url: String,
    http: Client,
}

impl AttachClient {
    /// Create a new client pointed at `base_url` (e.g. `"http://localhost:7777"`).
    ///
    /// `jwt_token` is the pre-signed JWT string produced by `scripts/gen_jwt.sh`.
    /// It is attached as `Authorization: Bearer <token>` on every request.
    pub fn new(
        base_url: impl Into<String>,
        jwt_token: impl AsRef<str>,
    ) -> Result<Self, AttachError> {
        let mut headers = HeaderMap::new();
        let auth = format!("Bearer {}", jwt_token.as_ref());
        let header_val = HeaderValue::from_str(&auth).map_err(|_| AttachError::InvalidToken)?;
        headers.insert(AUTHORIZATION, header_val);
        let http = Client::builder().default_headers(headers).build()?;
        Ok(Self {
            base_url: base_url.into(),
            http,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url.trim_end_matches('/'), path)
    }

    // ── SQL / Cypher ──────────────────────────────────────────────────────────

    /// Execute one or more `;`-separated SQL statements atomically.
    ///
    /// Returns one `ExecResult` per statement, in order.
    pub fn execute_sql(&self, sql: &str) -> Result<Vec<ExecResult>, AttachError> {
        let resp = self
            .http
            .post(self.url("/sql"))
            .json(&serde_json::json!({ "sql": sql }))
            .send()?;
        let body: Json = check_response(resp)?.json()?;
        decode_exec_results(&body)
    }

    /// Execute a Cypher `MATCH … WHERE … RETURN …` query.
    pub fn execute_cypher(&self, query: &str) -> Result<Vec<ExecResult>, AttachError> {
        let resp = self
            .http
            .post(self.url("/cypher"))
            .json(&serde_json::json!({ "query": query }))
            .send()?;
        let body: Json = check_response(resp)?.json()?;
        decode_exec_results(&body)
    }

    // ── Raw row CRUD ──────────────────────────────────────────────────────────
    //
    // These wrap the low-level `/rows` routes for callers working directly
    // with hand-encoded bytes (e.g. benchmarks).  Most callers will use
    // `execute_sql` instead.

    /// Insert raw bytes, returning the new row's location.
    pub fn insert(&self, data: Vec<u8>) -> Result<RowId, AttachError> {
        let resp = self
            .http
            .post(self.url("/rows"))
            .header(CONTENT_TYPE, "application/octet-stream")
            .body(data)
            .send()?;
        let body: Json = check_response(resp)?.json()?;
        decode_row_id(&body["row_id"])
    }

    /// Fetch the raw bytes for a row by its `RowId`.
    pub fn get(&self, row_id: RowId) -> Result<Vec<u8>, AttachError> {
        let resp = self
            .http
            .get(self.url(&format!("/rows/{}/{}", row_id.page_id, row_id.slot)))
            .send()?;
        let bytes = check_response(resp)?.bytes()?;
        Ok(bytes.to_vec())
    }

    /// Overwrite a row's bytes.  Returns the new `RowId` (MVCC always creates
    /// a fresh physical version; the old `RowId` is no longer valid after this
    /// call succeeds).
    pub fn update(&self, row_id: RowId, data: Vec<u8>) -> Result<RowId, AttachError> {
        let resp = self
            .http
            .put(self.url(&format!("/rows/{}/{}", row_id.page_id, row_id.slot)))
            .header(CONTENT_TYPE, "application/octet-stream")
            .body(data)
            .send()?;
        let body: Json = check_response(resp)?.json()?;
        decode_row_id(&body["row_id"])
    }

    /// Delete a row by its `RowId`.
    pub fn delete(&self, row_id: RowId) -> Result<(), AttachError> {
        let resp = self
            .http
            .delete(self.url(&format!("/rows/{}/{}", row_id.page_id, row_id.slot)))
            .send()?;
        check_response(resp)?;
        Ok(())
    }

    // ── Graph ─────────────────────────────────────────────────────────────────

    /// Create a directed edge `from_id → to_id` of the given type. `props`
    /// is arbitrary JSON metadata; pass `serde_json::json!({})` if none.
    /// Returns the new edge's `RowId` (needed for `delete_edge`).
    pub fn create_edge(
        &self,
        from_id: i64,
        to_id: i64,
        edge_type: &str,
        props: Json,
    ) -> Result<RowId, AttachError> {
        let resp = self
            .http
            .post(self.url("/edges"))
            .json(&serde_json::json!({
                "from_id": from_id,
                "to_id": to_id,
                "edge_type": edge_type,
                "props": props,
            }))
            .send()?;
        let body: Json = check_response(resp)?.json()?;
        decode_row_id(&body["row_id"])
    }

    /// Delete the edge at `row_id`. `from_id` must match the edge's `from_id`
    /// (the engine uses it to invalidate the in-memory index entry).
    pub fn delete_edge(&self, row_id: RowId, from_id: i64) -> Result<(), AttachError> {
        let resp = self
            .http
            .delete(self.url(&format!("/edges/{}/{}", row_id.page_id, row_id.slot)))
            .json(&serde_json::json!({ "from_id": from_id }))
            .send()?;
        check_response(resp)?;
        Ok(())
    }

    /// Return all edges originating from `from_id`, filtered by the caller's
    /// MVCC snapshot.
    pub fn edges_from(&self, from_id: i64) -> Result<Vec<EdgeResult>, AttachError> {
        let resp = self
            .http
            .get(self.url(&format!("/edges/from/{}", from_id)))
            .send()?;
        let body: Json = check_response(resp)?.json()?;
        let edges: Vec<EdgeResult> = serde_json::from_value(body["edges"].clone())?;
        Ok(edges)
    }

    // ── Secondary indexes ─────────────────────────────────────────────────────

    /// Mark a column as indexed. Pass `kind = None` to remove an existing index.
    pub fn set_column_index(
        &self,
        table: &str,
        column: &str,
        kind: Option<IndexKind>,
    ) -> Result<(), AttachError> {
        let resp = self
            .http
            .post(self.url("/indexes"))
            .json(&serde_json::json!({
                "table": table,
                "column": column,
                "kind": kind,
            }))
            .send()?;
        check_response(resp)?;
        Ok(())
    }

    /// Return the raw JSON status for an index, or `None` if no index exists
    /// for that `(table, column)` pair.
    ///
    /// Typical values: `"Ready"` or `{"Building": {"rows_done": N}}`.
    pub fn index_status(&self, table: &str, column: &str) -> Result<Option<Json>, AttachError> {
        let resp = self
            .http
            .get(self.url(&format!("/indexes/{}/{}/status", table, column)))
            .send()?;
        let body: Json = check_response(resp)?.json()?;
        let status = body["status"].clone();
        if status.is_null() {
            Ok(None)
        } else {
            Ok(Some(status))
        }
    }

    // ── Events ────────────────────────────────────────────────────────────────

    /// Opt a table into event capture. Must be called before events appear on
    /// `GET /events/subscribe` for that table.
    pub fn enable_events(&self, table: &str) -> Result<(), AttachError> {
        let resp = self
            .http
            .post(self.url(&format!("/tables/{}/events", table)))
            .send()?;
        check_response(resp)?;
        Ok(())
    }

    /// Acknowledge all events up to (and including) `up_to_seq` for
    /// `consumer`. Those events will not be replayed on a future subscribe.
    pub fn ack_events(&self, consumer: &str, up_to_seq: i64) -> Result<(), AttachError> {
        let resp = self
            .http
            .post(self.url("/events/ack"))
            .json(&serde_json::json!({
                "consumer": consumer,
                "up_to_seq": up_to_seq,
            }))
            .send()?;
        check_response(resp)?;
        Ok(())
    }

    // ── Operational ───────────────────────────────────────────────────────────

    /// Flush all dirty pages, write a checkpoint record, and truncate the WAL.
    pub fn checkpoint(&self) -> Result<(), AttachError> {
        let resp = self.http.post(self.url("/checkpoint")).send()?;
        check_response(resp)?;
        Ok(())
    }
}
