//! Dead-letter table (item 20, E2b) — **dogfood**: when a webhook exhausts its
//! retries, the undelivered event is written back into *unidb itself* as an
//! ordinary user table, not to some external store. That is the whole point of
//! the engine — the dispatcher's own durability lives in the same log as the
//! data it dispatches.

use unidb::{sql::logical::Literal, DbError, Engine};

use crate::sink::SinkError;

/// One failed delivery, as stored in the dead-letter table.
pub struct DeadLetter<'a> {
    pub seq: i64,
    pub xid: i64,
    pub table_name: &'a str,
    pub op: &'a str,
    pub sink: &'a str,
    pub attempts: i64,
    pub error: &'a str,
    pub payload: &'a serde_json::Value,
}

/// Idempotently create the dead-letter table. Columns mirror an [`Event`] plus
/// the delivery-failure context (`sink`, `attempts`, `error`). A pre-existing
/// table is fine — anything other than "already exists" propagates.
///
/// [`Event`]: unidb::queue::Event
pub fn ensure_dlq_table(engine: &Engine, table: &str, xid: u64) -> Result<(), DbError> {
    let sql = format!(
        "CREATE TABLE {table} (\
            seq INT, xid INT, table_name TEXT, op TEXT, \
            sink TEXT, attempts INT, error TEXT, payload JSON)"
    );
    match engine.execute_sql(xid, &sql) {
        Ok(_) => Ok(()),
        Err(DbError::TableAlreadyExists(_)) => Ok(()),
        Err(e) => Err(e),
    }
}

/// Insert one dead-letter row using bound parameters (`$n`) so the raw JSON
/// payload and arbitrary error text can never break out into SQL.
pub fn insert_dead_letter(
    engine: &Engine,
    table: &str,
    xid: u64,
    dl: &DeadLetter<'_>,
) -> Result<(), DbError> {
    let sql = format!(
        "INSERT INTO {table} \
            (seq, xid, table_name, op, sink, attempts, error, payload) \
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8)"
    );
    let params = [
        Literal::Int(dl.seq),
        Literal::Int(dl.xid),
        Literal::Text(dl.table_name.to_string()),
        Literal::Text(dl.op.to_string()),
        Literal::Text(dl.sink.to_string()),
        Literal::Int(dl.attempts),
        Literal::Text(dl.error.to_string()),
        Literal::Json(dl.payload.to_string()),
    ];
    engine.execute_sql_params(xid, &sql, &params)?;
    Ok(())
}

impl From<DbError> for SinkError {
    fn from(e: DbError) -> Self {
        SinkError::new(format!("dead-letter write failed: {e}"))
    }
}
