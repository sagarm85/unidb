// Edge storage (M3.a): graph edges are stored as ordinary rows in one
// synthetic system table, `__edges__` — not a new storage path. This is a
// deliberate reuse, not a shortcut: `TableDef` has no "kind" field
// distinguishing system vs. user tables, so `__edges__` gets full MVCC
// versioning, WAL durability, crash recovery, and even ordinary
// `SELECT * FROM __edges__ WHERE from_id = ?` queryability for free, with
// zero new storage-layer code. Row encoding reuses
// `sql::executor::encode_row`/`decode_row` verbatim — no new tag byte.
//
// Per-edge write locking also needs no new code: `RecordId::row(page_id,
// slot)` (lockmgr.rs) already produces a globally-unique lock key across
// every table in the database, since `PageId` is allocated from one shared
// `BufferPool`, not per-table. See MEMORY.md's M3.b design note for the
// verification tests proving this.

use crate::{
    catalog::{Catalog, CatalogCtx, ColumnDef, ColumnType, TableDef},
    error::{DbError, Result},
    heap::RowId,
    sql::logical::Literal,
};

pub const EDGES_TABLE: &str = "__edges__";

/// One resolved edge, returned by traversal (`Engine::edges_from`).
// `serde::Serialize` for the M5 REST server (`GET /edges/from/:id`) — see
// `heap::RowId`'s doc comment for why this isn't feature-gated.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Edge {
    pub row_id: RowId,
    pub to_id: i64,
    pub edge_type: String,
    /// Raw JSON text, same representation as `Literal::Json` elsewhere.
    pub props: String,
}

pub fn edges_table_def() -> TableDef {
    TableDef {
        name: EDGES_TABLE.to_string(),
        columns: vec![
            ColumnDef {
                name: "from_id".to_string(),
                ty: ColumnType::Int64,
                index: None,
                index_root: None,
                unique_index_root: None,
                dropped: false,
                constraints: Default::default(),
                include_cols: Vec::new(),
            },
            ColumnDef {
                name: "to_id".to_string(),
                ty: ColumnType::Int64,
                index: None,
                index_root: None,
                unique_index_root: None,
                dropped: false,
                constraints: Default::default(),
                include_cols: Vec::new(),
            },
            ColumnDef {
                name: "edge_type".to_string(),
                ty: ColumnType::Text,
                index: None,
                index_root: None,
                unique_index_root: None,
                dropped: false,
                constraints: Default::default(),
                include_cols: Vec::new(),
            },
            ColumnDef {
                name: "props".to_string(),
                ty: ColumnType::Json,
                index: None,
                index_root: None,
                unique_index_root: None,
                dropped: false,
                constraints: Default::default(),
                include_cols: Vec::new(),
            },
        ],
        pages: Vec::new(),
        fsm_meta: None,
        rls_policy: None,
        insert_policy: None,
        update_policy: None,
        delete_policy: None,
        update_with_check: None,
        policies: vec![],
        events_enabled: false,
        serial_next: Default::default(),
        constraints: Default::default(),
        generation: 0,
        row_count: 0,
        fill_factor: 100,
    }
}

pub fn edge_row(from_id: i64, to_id: i64, edge_type: &str, props: &str) -> Vec<Literal> {
    vec![
        Literal::Int(from_id),
        Literal::Int(to_id),
        Literal::Text(edge_type.to_string()),
        Literal::Json(props.to_string()),
    ]
}

/// Idempotent: create `__edges__` if it doesn't already exist in the
/// catalog. Called once from `Engine::open()`, before any user transaction
/// begins — so unlike ordinary `CREATE TABLE`, there is no "ran inside a
/// transaction that later aborted" edge case to worry about here (see
/// MEMORY.md's M3.a design note).
pub fn ensure_edges_table(catalog: &mut Catalog, ctx: &mut CatalogCtx) -> Result<()> {
    match catalog.lookup(EDGES_TABLE) {
        Ok(_) => Ok(()),
        Err(DbError::TableNotFound(_)) => catalog.create_table(edges_table_def(), ctx),
        Err(e) => Err(e),
    }
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
    fn ensure_edges_table_is_idempotent() {
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
        ensure_edges_table(&mut catalog, &mut ctx).unwrap();
        ensure_edges_table(&mut catalog, &mut ctx).unwrap();
        assert_eq!(catalog.lookup(EDGES_TABLE).unwrap().columns.len(), 4);
    }
}
