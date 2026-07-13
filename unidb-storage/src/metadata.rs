//! Bucket/object metadata as **ordinary unidb tables**. Everything here runs on
//! the synchronous engine (called from a blocking task) via `execute_sql_params`
//! so raw keys/values can never break out into SQL. Object *bytes* live
//! elsewhere (a LOB or the object store); these tables are the transactional
//! source of truth for *where* and *what state*.

use unidb::format::Xid;
use unidb::sql::logical::Literal;
use unidb::{DbError, Engine, SqlResult};

pub const BUCKETS_TABLE: &str = "buckets";
pub const OBJECTS_TABLE: &str = "objects";
/// Storage-native dead-letter table (dogfood). Deliberately **compact** (4
/// columns) rather than reusing `unidb_dispatch::dlq`'s 8-column schema: unidb's
/// whole catalog (every `TableDef`) is persisted as a **single ~8 KiB page
/// blob**, and buckets + a 10-column objects table + the 8-column dispatch DLQ
/// overflows it (measured `HeapFull { size: 8883 }`). See
/// `docs/design/storage_service.md` §4 (dated correction, 2026-07-13).
pub const DLQ_TABLE: &str = "object_dlq";

/// Object lifecycle status (the outbox state machine).
pub mod status {
    /// Bytes are present and metadata is authoritative.
    pub const READY: &str = "ready";
    /// Metadata committed (outbox event emitted); bytes not yet confirmed.
    pub const PENDING: &str = "pending";
    /// Upload never completed within grace — compensated, dead-lettered.
    pub const FAILED: &str = "failed";
}

/// Storage tier for an object's bytes.
pub mod tier {
    /// Small object stored as an engine LOB (ACID-inline).
    pub const INLINE: &str = "inline";
    /// Large object stored in the S3-wire object store.
    pub const S3: &str = "s3";
}

/// A row of the `objects` table. Note there is **no** `storage_key` column: the
/// physical store key is always `"<bucket>/<object_key>"`
/// ([`crate::service::storage_key`]), so it is derived, never stored — that
/// saved column is what keeps the schema under the single-page catalog ceiling.
#[derive(Debug, Clone)]
pub struct ObjectRow {
    pub bucket: String,
    pub object_key: String,
    pub size: i64,
    pub etag: Option<String>,
    pub content_type: Option<String>,
    pub tier: String,
    pub status: String,
    pub lob_id: Option<i64>,
    pub created_by: Option<String>,
    pub created_at_ms: i64,
}

/// Current epoch milliseconds — the `created_at_ms` clock the reconciler ages
/// pending rows against.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Idempotently create **all** storage tables (`buckets`, `objects`,
/// `object_dlq`). A pre-existing table is fine; anything other than "already
/// exists" propagates.
///
/// **Create everything up front, before any data.** unidb persists the whole
/// catalog as one ~8 KiB page blob and it is only rewritten on a *catalog*
/// mutation (DDL / `enable_events`). Doing all DDL before rows are written keeps
/// the persisted blob at its small creation size; a lazy `CREATE TABLE` at
/// runtime would re-serialize a catalog whose in-memory state has grown and can
/// overflow. See `docs/design/storage_service.md` §4.
pub fn ensure_schema(engine: &Engine, xid: Xid) -> Result<(), DbError> {
    ddl(
        engine,
        xid,
        &format!(
            "CREATE TABLE {BUCKETS_TABLE} \
             (name TEXT, created_by TEXT, created_at_ms INT)"
        ),
    )?;
    ddl(
        engine,
        xid,
        &format!(
            "CREATE TABLE {OBJECTS_TABLE} (\
                bucket TEXT, object_key TEXT, size INT, etag TEXT, \
                content_type TEXT, tier TEXT, status TEXT, lob_id INT, \
                created_by TEXT, created_at_ms INT)"
        ),
    )?;
    ddl(
        engine,
        xid,
        &format!(
            "CREATE TABLE {DLQ_TABLE} \
             (bucket TEXT, object_key TEXT, error TEXT, at_ms INT)"
        ),
    )?;
    Ok(())
}

/// Insert a compensation dead-letter row (dogfood). Called by the reconciler
/// when an upload is compensated to `failed`; the table is pre-created by
/// [`ensure_schema`] so this never does DDL.
pub fn insert_dead_letter(
    engine: &Engine,
    xid: Xid,
    bucket: &str,
    object_key: &str,
    error: &str,
) -> Result<(), DbError> {
    let sql = format!(
        "INSERT INTO {DLQ_TABLE} (bucket, object_key, error, at_ms) VALUES ($1, $2, $3, $4)"
    );
    engine.execute_sql_params(
        xid,
        &sql,
        &[
            Literal::Text(bucket.to_string()),
            Literal::Text(object_key.to_string()),
            Literal::Text(error.to_string()),
            Literal::Int(now_ms()),
        ],
    )?;
    Ok(())
}

fn ddl(engine: &Engine, xid: Xid, sql: &str) -> Result<(), DbError> {
    match engine.execute_sql(xid, sql) {
        Ok(_) => Ok(()),
        Err(DbError::TableAlreadyExists(_)) => Ok(()),
        Err(e) => Err(e),
    }
}

/// Insert a bucket. Duplicate names are the caller's concern (kept simple —
/// no unique index yet); the service checks existence first.
pub fn insert_bucket(
    engine: &Engine,
    xid: Xid,
    name: &str,
    created_by: Option<&str>,
) -> Result<(), DbError> {
    let sql = format!(
        "INSERT INTO {BUCKETS_TABLE} (name, created_by, created_at_ms) VALUES ($1, $2, $3)"
    );
    engine.execute_sql_params(
        xid,
        &sql,
        &[
            Literal::Text(name.to_string()),
            opt_text(created_by),
            Literal::Int(now_ms()),
        ],
    )?;
    Ok(())
}

pub fn bucket_exists(engine: &Engine, xid: Xid, name: &str) -> Result<bool, DbError> {
    let sql = format!("SELECT name FROM {BUCKETS_TABLE} WHERE name = $1");
    let rows = rows_of(engine.execute_sql_params(xid, &sql, &[Literal::Text(name.to_string())])?);
    Ok(!rows.is_empty())
}

/// Insert an `objects` row. When events are enabled on `objects`, this emits the
/// atomic "upload-pending"/"ready" outbox event in the same transaction.
pub fn insert_object(engine: &Engine, xid: Xid, row: &ObjectRow) -> Result<(), DbError> {
    let sql = format!(
        "INSERT INTO {OBJECTS_TABLE} \
            (bucket, object_key, size, etag, content_type, tier, status, \
             lob_id, created_by, created_at_ms) \
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)"
    );
    engine.execute_sql_params(
        xid,
        &sql,
        &[
            Literal::Text(row.bucket.clone()),
            Literal::Text(row.object_key.clone()),
            Literal::Int(row.size),
            opt_text(row.etag.as_deref()),
            opt_text(row.content_type.as_deref()),
            Literal::Text(row.tier.clone()),
            Literal::Text(row.status.clone()),
            opt_int(row.lob_id),
            opt_text(row.created_by.as_deref()),
            Literal::Int(row.created_at_ms),
        ],
    )?;
    Ok(())
}

/// Flip a pending row to `ready` and stamp its confirmed etag/size.
pub fn mark_ready(
    engine: &Engine,
    xid: Xid,
    bucket: &str,
    object_key: &str,
    etag: Option<&str>,
    size: i64,
) -> Result<(), DbError> {
    let sql = format!(
        "UPDATE {OBJECTS_TABLE} SET status = $1, etag = $2, size = $3 \
         WHERE bucket = $4 AND object_key = $5"
    );
    engine.execute_sql_params(
        xid,
        &sql,
        &[
            Literal::Text(status::READY.to_string()),
            opt_text(etag),
            Literal::Int(size),
            Literal::Text(bucket.to_string()),
            Literal::Text(object_key.to_string()),
        ],
    )?;
    Ok(())
}

/// Compensation: flip a pending row to `failed`.
pub fn mark_failed(
    engine: &Engine,
    xid: Xid,
    bucket: &str,
    object_key: &str,
) -> Result<(), DbError> {
    let sql =
        format!("UPDATE {OBJECTS_TABLE} SET status = $1 WHERE bucket = $2 AND object_key = $3");
    engine.execute_sql_params(
        xid,
        &sql,
        &[
            Literal::Text(status::FAILED.to_string()),
            Literal::Text(bucket.to_string()),
            Literal::Text(object_key.to_string()),
        ],
    )?;
    Ok(())
}

pub fn delete_object_row(
    engine: &Engine,
    xid: Xid,
    bucket: &str,
    object_key: &str,
) -> Result<(), DbError> {
    let sql = format!("DELETE FROM {OBJECTS_TABLE} WHERE bucket = $1 AND object_key = $2");
    engine.execute_sql_params(
        xid,
        &sql,
        &[
            Literal::Text(bucket.to_string()),
            Literal::Text(object_key.to_string()),
        ],
    )?;
    Ok(())
}

/// Look up one object by `(bucket, object_key)`.
pub fn lookup_object(
    engine: &Engine,
    xid: Xid,
    bucket: &str,
    object_key: &str,
) -> Result<Option<ObjectRow>, DbError> {
    let sql = format!("{SELECT_COLS} WHERE bucket = $1 AND object_key = $2");
    let rows = rows_of(engine.execute_sql_params(
        xid,
        &sql,
        &[
            Literal::Text(bucket.to_string()),
            Literal::Text(object_key.to_string()),
        ],
    )?);
    Ok(rows.into_iter().next().map(decode_object))
}

/// All pending objects (the reconciler's confirm/compensate work list).
pub fn list_pending(engine: &Engine, xid: Xid) -> Result<Vec<ObjectRow>, DbError> {
    let sql = format!("{SELECT_COLS} WHERE status = $1");
    let rows = rows_of(engine.execute_sql_params(
        xid,
        &sql,
        &[Literal::Text(status::PENDING.to_string())],
    )?);
    Ok(rows.into_iter().map(decode_object).collect())
}

/// Every S3-tier object row (any status) — the reference set the orphan sweep
/// compares the store listing against.
pub fn list_s3_objects(engine: &Engine, xid: Xid) -> Result<Vec<ObjectRow>, DbError> {
    let sql = format!("{SELECT_COLS} WHERE tier = $1");
    let rows =
        rows_of(engine.execute_sql_params(xid, &sql, &[Literal::Text(tier::S3.to_string())])?);
    Ok(rows.into_iter().map(decode_object).collect())
}

const SELECT_COLS: &str = "SELECT bucket, object_key, size, etag, content_type, tier, status, \
     lob_id, created_by, created_at_ms FROM objects";

fn decode_object(r: Vec<Literal>) -> ObjectRow {
    ObjectRow {
        bucket: as_text(&r[0]),
        object_key: as_text(&r[1]),
        size: as_int(&r[2]),
        etag: as_opt_text(&r[3]),
        content_type: as_opt_text(&r[4]),
        tier: as_text(&r[5]),
        status: as_text(&r[6]),
        lob_id: as_opt_int(&r[7]),
        created_by: as_opt_text(&r[8]),
        created_at_ms: as_int(&r[9]),
    }
}

// ── Literal <-> Rust helpers ────────────────────────────────────────────────

fn rows_of(results: Vec<SqlResult>) -> Vec<Vec<Literal>> {
    for res in results {
        if let SqlResult::Rows { rows, .. } = res {
            return rows;
        }
    }
    Vec::new()
}

fn opt_text(v: Option<&str>) -> Literal {
    match v {
        Some(s) => Literal::Text(s.to_string()),
        None => Literal::Null,
    }
}

fn opt_int(v: Option<i64>) -> Literal {
    match v {
        Some(i) => Literal::Int(i),
        None => Literal::Null,
    }
}

fn as_text(l: &Literal) -> String {
    match l {
        Literal::Text(s) => s.clone(),
        _ => String::new(),
    }
}

fn as_opt_text(l: &Literal) -> Option<String> {
    match l {
        Literal::Text(s) => Some(s.clone()),
        _ => None,
    }
}

fn as_int(l: &Literal) -> i64 {
    match l {
        Literal::Int(i) => *i,
        _ => 0,
    }
}

fn as_opt_int(l: &Literal) -> Option<i64> {
    match l {
        Literal::Int(i) => Some(*i),
        _ => None,
    }
}
