// Event queue (M4): a WAL-derived event stream is a dead end — WAL records
// carry no table identifier and `checkpoint.rs::run()` truncates
// unconditionally with zero reader-awareness. Instead, events are copied
// into an ordinary, durable heap table (`__events__`) at write time,
// synchronously, under the writing transaction's own xid — see
// `sql/executor.rs::send_event_capture` for where that copy happens.
// `__events__`/`__consumers__` are stored exactly like `__edges__` (M3):
// ordinary rows in a synthetic system table, with full MVCC versioning, WAL
// durability, and ordinary `SELECT * FROM __events__` queryability for
// free, since `TableDef` has no "kind" field distinguishing system vs. user
// tables.
//
// Note: `poll_events`/`ack_events`/`vacuum_events` (Engine methods in
// lib.rs) are bespoke Rust methods, not `execute_sql`-routed plans, so they
// bypass `apply_rls` entirely by construction — the same precedent already
// established by `edges_from` (M3).

pub mod payload;

use crate::{
    bufferpool::BufferPool,
    catalog::{Catalog, CatalogCtx, ColumnDef, ColumnType, TableDef},
    error::{DbError, Result},
    format::Xid,
    heap::{Heap, RowId},
    mvcc::Snapshot,
    sql::{executor, logical::Literal},
};

pub const EVENTS_TABLE: &str = "__events__";
pub const CONSUMERS_TABLE: &str = "__consumers__";

/// One event, as returned by `Engine::poll_events`.
// `serde::Serialize` for the M5 REST/SSE server — see `heap::RowId`'s doc
// comment for why this isn't feature-gated.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Event {
    pub seq: i64,
    pub xid: i64,
    pub table_name: String,
    pub op: String,
    pub payload: serde_json::Value,
}

pub fn events_table_def() -> TableDef {
    TableDef {
        name: EVENTS_TABLE.to_string(),
        columns: vec![
            ColumnDef {
                name: "seq".to_string(),
                ty: ColumnType::Int64,
                index: None,
                index_root: None,
                dropped: false,
                constraints: Default::default(),
            },
            ColumnDef {
                name: "xid".to_string(),
                ty: ColumnType::Int64,
                index: None,
                index_root: None,
                dropped: false,
                constraints: Default::default(),
            },
            ColumnDef {
                name: "table_name".to_string(),
                ty: ColumnType::Text,
                index: None,
                index_root: None,
                dropped: false,
                constraints: Default::default(),
            },
            ColumnDef {
                name: "op".to_string(),
                ty: ColumnType::Text,
                index: None,
                index_root: None,
                dropped: false,
                constraints: Default::default(),
            },
            ColumnDef {
                name: "payload".to_string(),
                ty: ColumnType::Json,
                index: None,
                index_root: None,
                dropped: false,
                constraints: Default::default(),
            },
        ],
        pages: Vec::new(),
        rls_policy: None,
        events_enabled: false,
        serial_next: Default::default(),
        constraints: Default::default(),
    }
}

pub fn consumers_table_def() -> TableDef {
    TableDef {
        name: CONSUMERS_TABLE.to_string(),
        columns: vec![
            ColumnDef {
                name: "consumer_name".to_string(),
                ty: ColumnType::Text,
                index: None,
                index_root: None,
                dropped: false,
                constraints: Default::default(),
            },
            ColumnDef {
                name: "offset".to_string(),
                ty: ColumnType::Int64,
                index: None,
                index_root: None,
                dropped: false,
                constraints: Default::default(),
            },
        ],
        pages: Vec::new(),
        rls_policy: None,
        events_enabled: false,
        serial_next: Default::default(),
        constraints: Default::default(),
    }
}

pub fn event_row(
    seq: i64,
    xid: i64,
    table_name: &str,
    op: &str,
    payload: &serde_json::Value,
) -> Vec<Literal> {
    vec![
        Literal::Int(seq),
        Literal::Int(xid),
        Literal::Text(table_name.to_string()),
        Literal::Text(op.to_string()),
        Literal::Json(payload.to_string()),
    ]
}

pub fn consumer_row(consumer_name: &str, offset: i64) -> Vec<Literal> {
    vec![
        Literal::Text(consumer_name.to_string()),
        Literal::Int(offset),
    ]
}

/// Idempotent: create `__events__`/`__consumers__` if they don't already
/// exist. Called once from `Engine::open()`, before any user transaction
/// begins — mirrors `graph::edges::ensure_edges_table` exactly.
pub fn ensure_queue_tables(catalog: &mut Catalog, ctx: &mut CatalogCtx) -> Result<()> {
    match catalog.lookup(EVENTS_TABLE) {
        Ok(_) => {}
        Err(DbError::TableNotFound(_)) => catalog.create_table(events_table_def(), ctx)?,
        Err(e) => return Err(e),
    }
    match catalog.lookup(CONSUMERS_TABLE) {
        Ok(_) => Ok(()),
        Err(DbError::TableNotFound(_)) => catalog.create_table(consumers_table_def(), ctx),
        Err(e) => Err(e),
    }
}

/// Find `consumer`'s durable offset row in `__consumers__`, if it has ever
/// acked. `None` means the consumer has never called `ack_events` — treated
/// as offset 0 by the caller (`Engine::poll_events`), purely in-memory, not
/// written here.
pub fn find_consumer_offset(
    heap: &Heap,
    snapshot: &Snapshot,
    xid: Xid,
    pool: &BufferPool,
    consumer: &str,
) -> Result<Option<(RowId, i64)>> {
    let columns = &consumers_table_def().columns;
    for (row_id, bytes) in heap.scan(snapshot, xid, pool)? {
        let row = executor::decode_row(&bytes, columns)?;
        if let (Literal::Text(name), Literal::Int(offset)) = (&row[0], &row[1]) {
            if name == consumer {
                return Ok(Some((row_id, *offset)));
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{control, format::DEFAULT_PAGE_SIZE};

    fn setup(
        dir: &std::path::Path,
    ) -> (
        crate::bufferpool::BufferPool,
        crate::wal::Wal,
        std::path::PathBuf,
        std::sync::Mutex<crate::control::ControlData>,
    ) {
        let control_path = dir.join("control");
        let control = control::create(&control_path, DEFAULT_PAGE_SIZE).unwrap();
        let pool = crate::bufferpool::BufferPool::open(
            &dir.join("data.db"),
            DEFAULT_PAGE_SIZE as usize,
            64,
        )
        .unwrap();
        let wal = crate::wal::Wal::open(&dir.join("db.wal"), crate::format::INVALID_LSN).unwrap();
        (pool, wal, control_path, std::sync::Mutex::new(control))
    }

    #[test]
    fn ensure_queue_tables_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let (pool, wal, cp, control) = setup(dir.path());
        let mut catalog = Catalog::new();
        let mut ctx = CatalogCtx {
            pool: &pool,
            wal: &wal,
            control_path: &cp,
            control: &control,
            page_size: DEFAULT_PAGE_SIZE as usize,
        };
        ensure_queue_tables(&mut catalog, &mut ctx).unwrap();
        ensure_queue_tables(&mut catalog, &mut ctx).unwrap();
        assert_eq!(catalog.lookup(EVENTS_TABLE).unwrap().columns.len(), 5);
        assert_eq!(catalog.lookup(CONSUMERS_TABLE).unwrap().columns.len(), 2);
    }
}
