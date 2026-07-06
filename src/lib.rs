// unsafe_code is denied crate-wide; mmap.rs is the sole exception (CLAUDE.md §4).
#![deny(unsafe_code)]

pub mod bufferpool;
pub mod catalog;
pub mod checkpoint;
pub mod concurrency_hooks;
pub mod control;
pub mod error;
pub mod format;
pub mod heap;
pub mod lockmgr;
pub mod mmap;
pub mod mvcc;
pub mod page;
pub mod recovery;
pub mod sql;
pub mod txn;
pub mod wal;

use std::path::{Path, PathBuf};

use crate::{
    bufferpool::BufferPool,
    catalog::Catalog,
    control::ControlData,
    error::Result,
    format::{Xid, DEFAULT_PAGE_SIZE},
    heap::Heap,
    lockmgr::LockManager,
    sql::{
        executor::{self, ExecCtx, ExecResult},
        logical::{apply_rls, Expr},
        parser::parse_sql,
    },
    txn::{IsolationLevel, TransactionManager, UndoAction},
    wal::Wal,
};

pub use crate::error::DbError;
pub use crate::heap::RowId;
pub use crate::sql::executor::ExecResult as SqlResult;
pub use crate::txn::IsolationLevel as Isolation;

const POOL_CAPACITY: usize = 256;

pub struct Engine {
    control: ControlData,
    pool: BufferPool,
    wal: Wal,
    heap: Heap,
    txn_mgr: TransactionManager,
    lock_mgr: LockManager,
    catalog: Catalog,
    control_path: PathBuf,
    _wal_path: PathBuf,
}

impl Engine {
    /// Open (or create) a database at `dir`. Pass `page_size = 0` to use the default.
    pub fn open(dir: &Path, page_size: u32) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let ctrl_p = dir.join("control");
        let data_p = dir.join("data.db");
        let wal_p = dir.join("db.wal");

        let ps = if page_size == 0 {
            DEFAULT_PAGE_SIZE
        } else {
            page_size
        };
        let control = control::open_or_create(&ctrl_p, ps)?;
        let page_size_usize = control.page_size as usize;

        // Run recovery before opening normal operation.
        if wal_p.exists() && ctrl_p.exists() {
            recovery::recover(&ctrl_p, &data_p, &wal_p, page_size_usize, POOL_CAPACITY)?;
        }

        let mut pool = BufferPool::open(&data_p, page_size_usize, POOL_CAPACITY)?;
        let wal_tail = control.wal_tail_lsn;
        let wal = Wal::open(&wal_p, wal_tail)?;
        let heap = Heap::new(page_size_usize);

        // Resume the xid counter past the highest xid that ever began —
        // reusing an xid would corrupt MVCC visibility for existing tuples
        // (see MEMORY.md's design note).
        let existing_records = if wal_p.exists() {
            Wal::scan_file(&wal_p)?
        } else {
            Vec::new()
        };
        let next_xid = TransactionManager::recover_next_xid(&existing_records);
        let txn_mgr = TransactionManager::with_next_xid(next_xid);

        let catalog = Catalog::load(&control, &mut pool)?;

        tracing::info!(dir = %dir.display(), page_size = control.page_size, next_xid, "engine opened");
        Ok(Self {
            control,
            pool,
            wal,
            heap,
            txn_mgr,
            lock_mgr: LockManager::new(),
            catalog,
            control_path: ctrl_p,
            _wal_path: wal_p,
        })
    }

    /// Parse and execute one or more `;`-separated SQL statements under
    /// `xid`, applying each table's RLS policy (if any) as a planner
    /// rewrite before execution. Returns one result per statement.
    pub fn execute_sql(&mut self, xid: Xid, sql: &str) -> Result<Vec<ExecResult>> {
        let page_size = self.control.page_size as usize;
        let plans = parse_sql(sql)?;
        let mut results = Vec::with_capacity(plans.len());
        for plan in plans {
            let plan = apply_rls(plan, &self.catalog);
            let mut ctx = ExecCtx {
                catalog: &mut self.catalog,
                txn_mgr: &mut self.txn_mgr,
                pool: &mut self.pool,
                wal: &mut self.wal,
                lock_mgr: &mut self.lock_mgr,
                control_path: &self.control_path,
                control: &mut self.control,
                page_size,
                xid,
            };
            results.push(executor::execute(plan, &mut ctx)?);
        }
        Ok(results)
    }

    /// Attach a row-level-security policy to a table (M1: Rust API only,
    /// no `CREATE POLICY` SQL surface — see catalog.rs's module doc).
    pub fn set_rls_policy(&mut self, table: &str, policy: Expr) -> Result<()> {
        let page_size = self.control.page_size as usize;
        let mut ctx = crate::catalog::CatalogCtx {
            pool: &mut self.pool,
            wal: &mut self.wal,
            control_path: &self.control_path,
            control: &mut self.control,
            page_size,
        };
        self.catalog.set_rls_policy(table, policy, &mut ctx)
    }

    /// Begin a new transaction under READ COMMITTED (the default, D10).
    pub fn begin(&mut self) -> Result<Xid> {
        self.begin_with_isolation(IsolationLevel::ReadCommitted)
    }

    pub fn begin_with_isolation(&mut self, isolation: IsolationLevel) -> Result<Xid> {
        self.txn_mgr.begin(isolation, &mut self.wal)
    }

    pub fn commit(&mut self, xid: Xid) -> Result<()> {
        self.txn_mgr.commit(xid, &mut self.wal, &mut self.lock_mgr)
    }

    pub fn abort(&mut self, xid: Xid) -> Result<()> {
        self.txn_mgr.abort(
            xid,
            &mut self.pool,
            &mut self.heap,
            &mut self.wal,
            &mut self.lock_mgr,
        )
    }

    pub fn insert(&mut self, xid: Xid, data: &[u8]) -> Result<RowId> {
        let rid = self.heap.insert(data, xid, &mut self.pool, &mut self.wal)?;
        self.txn_mgr.record_undo(
            xid,
            UndoAction::Insert {
                page_id: rid.page_id,
                slot: rid.slot,
            },
        )?;
        Ok(rid)
    }

    pub fn get(&mut self, xid: Xid, row_id: RowId) -> Result<Vec<u8>> {
        let snapshot = self.txn_mgr.snapshot_for_statement(xid)?;
        self.heap.get(row_id, &snapshot, xid, &mut self.pool)
    }

    /// Update `row_id`, returning the new version's RowId (M1: UPDATE
    /// creates a new tuple version rather than overwriting in place, so the
    /// physical location may change; re-resolve via the returned RowId).
    pub fn update(&mut self, xid: Xid, row_id: RowId, new_data: &[u8]) -> Result<RowId> {
        let new_rid = self.heap.update(
            row_id,
            new_data,
            xid,
            &mut self.pool,
            &mut self.wal,
            &mut self.lock_mgr,
        )?;
        self.txn_mgr.record_undo(
            xid,
            UndoAction::XmaxStamp {
                page_id: row_id.page_id,
                slot: row_id.slot,
            },
        )?;
        self.txn_mgr.record_undo(
            xid,
            UndoAction::Insert {
                page_id: new_rid.page_id,
                slot: new_rid.slot,
            },
        )?;
        Ok(new_rid)
    }

    pub fn delete(&mut self, xid: Xid, row_id: RowId) -> Result<()> {
        self.heap.delete(
            row_id,
            xid,
            &mut self.pool,
            &mut self.wal,
            &mut self.lock_mgr,
        )?;
        self.txn_mgr.record_undo(
            xid,
            UndoAction::XmaxStamp {
                page_id: row_id.page_id,
                slot: row_id.slot,
            },
        )?;
        Ok(())
    }

    pub fn checkpoint(&mut self) -> Result<()> {
        checkpoint::run(
            &mut self.pool,
            &mut self.wal,
            &self.control_path,
            &mut self.control,
        )
    }

    /// Flush all dirty pages without a full checkpoint (used in tests).
    pub fn flush(&mut self) -> Result<()> {
        self.pool.flush_all(self.wal.durable_lsn)
    }
}

/// Initialize a `tracing_subscriber` with `RUST_LOG` env filter.
/// Call once at the start of your binary or test suite.
pub fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn open_insert_get_roundtrip() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        let rid = engine.insert(xid, b"hello world").unwrap();
        let data = engine.get(xid, rid).unwrap();
        assert_eq!(data, b"hello world");
        engine.commit(xid).unwrap();
    }

    #[test]
    fn update_and_verify() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        let rid = engine.insert(xid, b"initial_value").unwrap();
        let new_rid = engine.update(xid, rid, b"updated").unwrap();
        assert_eq!(engine.get(xid, new_rid).unwrap(), b"updated");
        engine.commit(xid).unwrap();
    }

    #[test]
    fn delete_makes_row_gone() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        let rid = engine.insert(xid, b"transient").unwrap();
        engine.delete(xid, rid).unwrap();
        assert!(engine.get(xid, rid).is_err());
        engine.commit(xid).unwrap();
    }

    #[test]
    fn reopen_after_flush_recovers_data() {
        let dir = tempdir().unwrap();
        let rid = {
            let mut engine = Engine::open(dir.path(), 0).unwrap();
            let xid = engine.begin().unwrap();
            let rid = engine.insert(xid, b"durable").unwrap();
            engine.commit(xid).unwrap();
            engine.flush().unwrap();
            rid
        };
        let mut engine2 = Engine::open(dir.path(), 0).unwrap();
        let xid2 = engine2.begin().unwrap();
        assert_eq!(engine2.get(xid2, rid).unwrap(), b"durable");
    }

    #[test]
    fn read_committed_sees_other_txns_committed_write() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        let a = engine.begin().unwrap();
        let rid = engine.insert(a, b"v1").unwrap();
        engine.commit(a).unwrap();

        let b = engine.begin().unwrap();
        assert_eq!(engine.get(b, rid).unwrap(), b"v1");
        engine.commit(b).unwrap();
    }

    #[test]
    fn repeatable_read_does_not_see_write_committed_after_begin() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        let a = engine.begin().unwrap();
        let rid = engine.insert(a, b"v1").unwrap();
        engine.commit(a).unwrap();

        // b begins under RR before a's write... actually a already committed
        // above, so instead: b begins RR, then c writes and commits, and b's
        // fixed snapshot must not see c's write even after it commits.
        let b = engine
            .begin_with_isolation(Isolation::RepeatableRead)
            .unwrap();
        assert_eq!(engine.get(b, rid).unwrap(), b"v1"); // sees a's already-committed write

        let c = engine.begin().unwrap();
        let new_rid = engine.update(c, rid, b"v2").unwrap();
        engine.commit(c).unwrap();

        // b's RR snapshot predates c's commit, so it must still see v1 at
        // the original row_id (walking the version chain stops at v1).
        assert_eq!(engine.get(b, rid).unwrap(), b"v1");
        // A fresh READ COMMITTED transaction sees the new committed version.
        let d = engine.begin().unwrap();
        assert_eq!(engine.get(d, new_rid).unwrap(), b"v2");
        engine.commit(b).unwrap();
        engine.commit(d).unwrap();
    }

    #[test]
    fn rollback_undoes_insert() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        let a = engine.begin().unwrap();
        let rid = engine.insert(a, b"oops").unwrap();
        engine.abort(a).unwrap();

        let b = engine.begin().unwrap();
        assert!(engine.get(b, rid).is_err());
    }

    #[test]
    fn xid_counter_survives_reopen() {
        let dir = tempdir().unwrap();
        let first_xid = {
            let mut engine = Engine::open(dir.path(), 0).unwrap();
            let xid = engine.begin().unwrap();
            engine.insert(xid, b"row").unwrap();
            engine.commit(xid).unwrap();
            engine.flush().unwrap();
            xid
        };
        let mut engine2 = Engine::open(dir.path(), 0).unwrap();
        let next_xid = engine2.begin().unwrap();
        assert!(next_xid > first_xid, "reopened engine must not reuse xids");
    }

    // ── M1.b: SI abort-on-conflict (D12) ────────────────────────────────────

    #[test]
    fn concurrent_update_aborts_second_writer_immediately() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        let setup_xid = engine.begin().unwrap();
        let rid = engine.insert(setup_xid, b"row").unwrap();
        engine.commit(setup_xid).unwrap();

        // Two transactions both try to update the same row. Per D12, SI's
        // conflict handling is "abort immediately," not "block and wait" —
        // the second writer must fail right at the write call, not at
        // commit time (see txn.rs::commit's doc comment: because the lock
        // is held for the whole transaction lifetime, there's no separate
        // race window that a commit-time recheck would need to catch).
        let a = engine.begin().unwrap();
        let new_rid = engine.update(a, rid, b"a-wins").unwrap();

        let b = engine.begin().unwrap();
        let err = engine.update(b, rid, b"b-loses");
        assert!(
            matches!(err, Err(DbError::WriteConflict { .. })),
            "second writer must abort immediately on conflict, got {:?}",
            err
        );

        engine.commit(a).unwrap();
        engine.abort(b).unwrap();

        // a's write is the one that stuck.
        let c = engine.begin().unwrap();
        assert_eq!(engine.get(c, new_rid).unwrap(), b"a-wins");
    }

    #[test]
    fn commit_releases_lock_for_next_writer() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        let setup_xid = engine.begin().unwrap();
        let rid = engine.insert(setup_xid, b"row").unwrap();
        engine.commit(setup_xid).unwrap();

        let a = engine.begin().unwrap();
        let new_rid = engine.update(a, rid, b"a-wins").unwrap();
        engine.commit(a).unwrap();

        // Now that a released its lock, a fresh writer can update the
        // *new* version without any conflict.
        let b = engine.begin().unwrap();
        engine.update(b, new_rid, b"b-after-a").unwrap();
        engine.commit(b).unwrap();
    }

    #[test]
    fn abort_releases_lock_for_next_writer() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        let setup_xid = engine.begin().unwrap();
        let rid = engine.insert(setup_xid, b"row").unwrap();
        engine.commit(setup_xid).unwrap();

        let a = engine.begin().unwrap();
        engine.update(a, rid, b"a-abandoned").unwrap();
        engine.abort(a).unwrap();

        // a's abort released the lock (and undid the write), so b can
        // update the still-live original row.
        let b = engine.begin().unwrap();
        engine.update(b, rid, b"b-wins").unwrap();
        engine.commit(b).unwrap();

        let c = engine.begin().unwrap();
        assert!(engine.get(c, rid).is_err()); // superseded by b's update
    }

    // ── M1.c: SQL end-to-end ─────────────────────────────────────────────────

    #[test]
    fn execute_sql_full_round_trip() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();

        let xid = engine.begin().unwrap();
        engine
            .execute_sql(
                xid,
                "CREATE TABLE accounts (id INT, name TEXT, balance INT)",
            )
            .unwrap();
        engine
            .execute_sql(
                xid,
                "INSERT INTO accounts (id, name, balance) VALUES (1, 'alice', 100)",
            )
            .unwrap();
        engine
            .execute_sql(
                xid,
                "INSERT INTO accounts (id, name, balance) VALUES (2, 'bob', 50)",
            )
            .unwrap();
        engine.commit(xid).unwrap();

        let xid2 = engine.begin().unwrap();
        let results = engine
            .execute_sql(xid2, "SELECT * FROM accounts WHERE id = 1")
            .unwrap();
        assert_eq!(results.len(), 1);
        assert!(matches!(&results[0], SqlResult::Rows(rows) if rows.len() == 1));

        engine
            .execute_sql(xid2, "UPDATE accounts SET balance = 200 WHERE id = 1")
            .unwrap();
        let reselect = engine
            .execute_sql(xid2, "SELECT balance FROM accounts WHERE id = 1")
            .unwrap();
        match &reselect[0] {
            SqlResult::Rows(rows) => {
                assert_eq!(rows[0][0], crate::sql::logical::Literal::Int(200))
            }
            other => panic!("expected Rows, got {other:?}"),
        }

        engine
            .execute_sql(xid2, "DELETE FROM accounts WHERE id = 2")
            .unwrap();
        engine.commit(xid2).unwrap();

        let xid3 = engine.begin().unwrap();
        let remaining = engine.execute_sql(xid3, "SELECT * FROM accounts").unwrap();
        assert!(matches!(&remaining[0], SqlResult::Rows(rows) if rows.len() == 1));
    }

    #[test]
    fn sql_survives_reopen() {
        let dir = tempdir().unwrap();
        {
            let mut engine = Engine::open(dir.path(), 0).unwrap();
            let xid = engine.begin().unwrap();
            engine.execute_sql(xid, "CREATE TABLE t (id INT)").unwrap();
            engine
                .execute_sql(xid, "INSERT INTO t (id) VALUES (7)")
                .unwrap();
            engine.commit(xid).unwrap();
            engine.flush().unwrap();
        }
        let mut engine2 = Engine::open(dir.path(), 0).unwrap();
        let xid = engine2.begin().unwrap();
        let result = engine2.execute_sql(xid, "SELECT * FROM t").unwrap();
        match &result[0] {
            SqlResult::Rows(rows) => assert_eq!(rows.len(), 1),
            other => panic!("expected Rows, got {other:?}"),
        }
    }

    #[test]
    fn rls_policy_filters_rows() {
        let dir = tempdir().unwrap();
        let mut engine = Engine::open(dir.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        engine
            .execute_sql(xid, "CREATE TABLE t (id INT, owner TEXT)")
            .unwrap();
        engine
            .execute_sql(xid, "INSERT INTO t (id, owner) VALUES (1, 'alice')")
            .unwrap();
        engine
            .execute_sql(xid, "INSERT INTO t (id, owner) VALUES (2, 'bob')")
            .unwrap();
        engine.commit(xid).unwrap();

        let policy = crate::sql::logical::Expr::BinOp {
            op: crate::sql::logical::CmpOp::Eq,
            lhs: Box::new(crate::sql::logical::Expr::Column("owner".to_string())),
            rhs: Box::new(crate::sql::logical::Expr::Literal(
                crate::sql::logical::Literal::Text("alice".to_string()),
            )),
        };
        engine.set_rls_policy("t", policy).unwrap();

        let xid2 = engine.begin().unwrap();
        let result = engine.execute_sql(xid2, "SELECT * FROM t").unwrap();
        match &result[0] {
            SqlResult::Rows(rows) => assert_eq!(rows.len(), 1),
            other => panic!("expected Rows, got {other:?}"),
        }
    }
}
