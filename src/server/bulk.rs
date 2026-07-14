//! `POST /tables/{name}/bulk` — streaming NDJSON bulk-insert endpoint (item 32).
//!
//! **Performance contract**: one transaction for the whole body — begin once,
//! `prepare` the INSERT SQL once, loop `execute_prepared` for each row, commit
//! once. This amortizes the per-row HTTP overhead + per-statement fsync that
//! make the `/sql`-per-row path ~1.5 ms/row (item-32 root-cause attribution:
//! the gap is the HTTP/commit envelope, NOT B-tree cost — the engine inserts
//! ~30 µs/row including B-tree maintenance).
//!
//! **V1 buffering note**: the body is collected into memory before the
//! transaction begins (bounded by `MAX_BULK_BODY_BYTES`). NDJSON is validated
//! up front so a malformed row fails fast without paying a wasted txn begin
//! and undo-log footprint. A follow-up channel-based approach (async body
//! reader → mpsc → blocking engine loop) would remove the ceiling entirely;
//! for now the limit is 512 MiB — well above a 200k-row demo seed at ~16 MB.
//! For loads within this bound the whole-body-txn undo-log footprint already
//! exceeds the body bytes, so the buffer is not the binding OOM constraint.
//!
//! **Atomicity tradeoff**: one large transaction holds the undo log and pins
//! the vacuum horizon for its duration. A 3M-row single batch is a significant
//! footprint; an optional `?chunk=N` commit-every-N mode is a natural follow-
//! up for callers who prefer throughput over strict batch atomicity.

use std::time::Instant;

use axum::{
    body::to_bytes,
    extract::{Path, Request, State},
    http::StatusCode,
    Extension, Json,
};
use serde_json::json;

use crate::{
    server::{auth::CurrentUser, dto::json_to_literal, error::ApiError, AppState},
    sql::logical::Literal,
};

/// 512 MiB body limit. Raises the effective ceiling to ~5–6 M rows at typical
/// NDJSON row sizes (~80 bytes/row). The channel-streaming follow-up removes
/// this limit entirely; it is not needed for the MVP demo workload.
const MAX_BULK_BODY_BYTES: usize = 512 << 20;

/// `POST /tables/{name}/bulk` — JWT-protected NDJSON bulk insert.
///
/// Accepts a body of newline-delimited JSON objects. All objects must share
/// the same key set (the first row's key order becomes the INSERT column
/// order; later rows look up values by key name, so field order within an
/// object does not matter). Missing keys in later rows become `NULL`.
///
/// Response on success: `{ "inserted": N, "errors": 0, "elapsed_ms": M }`.
/// On any error the whole batch is rolled back atomically.
pub async fn post_tables_bulk(
    Extension(_current_user): Extension<CurrentUser>,
    State(state): State<AppState>,
    Path(name): Path<String>,
    request: Request,
) -> std::result::Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let started = Instant::now();

    // Validate table name is a plain SQL identifier — prevents injection
    // before we interpolate it into the prepared INSERT SQL.
    validate_identifier(&name).map_err(|e| ApiError::bad_request("INVALID_TABLE_NAME", e))?;

    // Collect body with size guard. 400 (not 500) so the caller sees
    // "your payload is too large" rather than "server error."
    let body_bytes = to_bytes(request.into_body(), MAX_BULK_BODY_BYTES)
        .await
        .map_err(|e| ApiError::bad_request("BODY_TOO_LARGE", e.to_string()))?;

    // Parse NDJSON before beginning the transaction: fail fast on bad input
    // without a wasted begin + undo-log footprint.
    let json_rows = parse_ndjson(&body_bytes)?;

    if json_rows.is_empty() {
        let elapsed = started.elapsed().as_millis() as u64;
        return Ok((
            StatusCode::OK,
            Json(json!({ "inserted": 0, "errors": 0, "elapsed_ms": elapsed })),
        ));
    }

    let (columns, all_params) = rows_to_params(json_rows)?;

    let inserted = state.engine.bulk_insert(name, columns, all_params).await?;

    let elapsed = started.elapsed().as_millis() as u64;
    Ok((
        StatusCode::OK,
        Json(json!({ "inserted": inserted, "errors": 0, "elapsed_ms": elapsed })),
    ))
}

/// Accept only `[A-Za-z_][A-Za-z0-9_]*` identifiers — the safe subset of
/// SQL unquoted identifiers that cannot carry injection payload.
fn validate_identifier(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("identifier must not be empty".into());
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_alphabetic() && first != '_' {
        return Err(format!(
            "identifier must start with a letter or underscore, got '{first}'"
        ));
    }
    if let Some(bad) = chars.find(|c| !c.is_ascii_alphanumeric() && *c != '_') {
        return Err(format!("identifier contains invalid character '{bad}'"));
    }
    Ok(())
}

/// Decode `bytes` as UTF-8 NDJSON, skipping blank lines.
fn parse_ndjson(bytes: &[u8]) -> Result<Vec<serde_json::Map<String, serde_json::Value>>, ApiError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| ApiError::bad_request("MALFORMED_NDJSON", "body is not valid UTF-8"))?;
    let mut rows = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let obj: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(line).map_err(|e| {
                ApiError::bad_request("MALFORMED_NDJSON", format!("line {}: {e}", i + 1))
            })?;
        rows.push(obj);
    }
    Ok(rows)
}

/// Convert parsed JSON maps to `(column_names, Vec<Vec<Literal>>)`.
///
/// Column order is the first row's key-iteration order (serde_json preserves
/// insertion order via `IndexMap` under the hood). Subsequent rows look up
/// values by column name so their internal key order does not matter; missing
/// keys become `Null`.
fn rows_to_params(
    rows: Vec<serde_json::Map<String, serde_json::Value>>,
) -> Result<(Vec<String>, Vec<Vec<Literal>>), ApiError> {
    let columns: Vec<String> = rows
        .first()
        .expect("rows is non-empty")
        .keys()
        .cloned()
        .collect();
    if columns.is_empty() {
        return Err(ApiError::bad_request(
            "EMPTY_ROW",
            "NDJSON rows must have at least one key",
        ));
    }
    for col in &columns {
        validate_identifier(col).map_err(|e| ApiError::bad_request("INVALID_COLUMN_NAME", e))?;
    }

    let mut all_params = Vec::with_capacity(rows.len());
    for obj in rows {
        let params: Vec<Literal> = columns
            .iter()
            .map(|col| obj.get(col).map(json_to_literal).unwrap_or(Literal::Null))
            .collect();
        all_params.push(params);
    }
    Ok((columns, all_params))
}
