// Read replica + failover (P6.c).
//
// A `Replica` is the consumer side of WAL shipping (P6.b), built on the standard
// streaming-replication model: **a physical base snapshot of the primary + the
// primary's WAL streamed and applied incrementally on top.**
//
//   * `init_from_base(dir, base)` seeds the replica from a consistent copy of the
//     primary's files (data.db + control + WAL segments) — the base backup.
//   * `apply_stream(bytes, control)` writes the shipped records verbatim into the
//     replica's local WAL and applies them with the crash-recovery redo path
//     **on top of** the existing page store (no wipe). Incremental redo on a
//     populated data file is exactly normal crash recovery, so it is proven
//     correct and idempotent (LSN-gated).
//
// Why a base is required: this engine does not log full-page images for freshly
// allocated pages (a documented P1.a limitation — "fresh pages aren't FPI
// covered"), so heap/catalog pages cannot be reconstructed from the WAL alone
// into an empty file. The base copy supplies those page bytes; the WAL stream
// then carries the incremental committed changes. This is exactly how real
// streaming replicas work (base backup + WAL), and it dovetails with the P6.d
// base-backup machinery.
//
// Shipping happens at commit boundaries (the primary ships after commits, and
// group commit fsyncs commit records), so a stream never ends mid-transaction —
// the replica therefore only ever applies complete, committed transactions and
// never has to undo. **Failover** is `promote()`: open the directory as a normal
// read-write `Engine`, keeping every acknowledged commit that was applied.
//
// **Known v1 limitation (documented, not silent):** because fresh pages aren't
// full-page-image covered, an INSERT that allocates a *brand-new* heap page
// *after* the base cannot be reconstructed by incremental redo alone — the row's
// page bytes never existed in the replica's file. Incremental apply is therefore
// correct for changes that land on pages already present in the base (the common
// steady-state case); a workload that keeps allocating new pages needs a periodic
// **re-base** (a fresh `init_from_base`). Closing this fully means FPI-covering
// fresh pages on the primary — tracked as follow-up work.

use std::path::{Path, PathBuf};

use crate::{
    control,
    error::{DbError, Result},
    format::{Lsn, INVALID_LSN},
    recovery,
    replication::PrimaryControl,
    wal::{decode_stream, Wal},
    Engine,
};

pub struct Replica {
    dir: PathBuf,
    page_size: usize,
    pool_capacity: usize,
    /// Highest LSN durably present in the replica's local WAL — the point to
    /// request the next shipping batch from (`ship_wal(applied_lsn)`).
    applied_lsn: Lsn,
}

/// Copy the durable files that make up a database directory (the control file,
/// the page store, and every WAL segment) from `src` into `dst`. Used to seed a
/// replica from a consistent base snapshot of a (quiescent or checkpointed)
/// primary. A minimal base backup; P6.d makes it online/streaming.
pub fn copy_db_dir(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for name in ["control", "data.db"] {
        let s = src.join(name);
        if s.exists() {
            std::fs::copy(&s, dst.join(name))?;
        }
    }
    // The WAL is a directory of segment files (P6.a).
    let wal_src = src.join("db.wal");
    let wal_dst = dst.join("db.wal");
    std::fs::create_dir_all(&wal_dst)?;
    if let Ok(rd) = std::fs::read_dir(&wal_src) {
        for entry in rd {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                std::fs::copy(entry.path(), wal_dst.join(entry.file_name()))?;
            }
        }
    }
    Ok(())
}

impl Replica {
    /// Initialize a replica at `dir` from a base snapshot of a primary at
    /// `base`. Copies the primary's files, then reads the local state so
    /// subsequent `apply_stream` calls extend it.
    pub fn init_from_base(dir: &Path, base: &Path) -> Result<Self> {
        copy_db_dir(base, dir)?;
        Self::open(dir)
    }

    /// Open an already-initialized replica directory (e.g. after a restart). The
    /// directory must already hold a base + WAL (see [`Replica::init_from_base`]);
    /// `applied_lsn` resumes from the local WAL tail.
    pub fn open(dir: &Path) -> Result<Self> {
        let ctrl_p = dir.join("control");
        if !ctrl_p.exists() {
            return Err(DbError::Replication(
                "replica directory is not initialized (no control file — call init_from_base)"
                    .into(),
            ));
        }
        let cd = control::read(&ctrl_p)?;
        let page_size = cd.page_size as usize;
        let wal_dir = dir.join("db.wal");
        let applied_lsn = Wal::scan_file(&wal_dir)?
            .iter()
            .map(|r| r.lsn)
            .max()
            .unwrap_or(INVALID_LSN);
        tracing::info!(dir = %dir.display(), applied_lsn, "replica opened");
        Ok(Self {
            dir: dir.to_path_buf(),
            page_size,
            pool_capacity: 4096,
            applied_lsn,
        })
    }

    /// The LSN the replica has durably received — its lag frontier vs. the
    /// primary's tail, and the `from_lsn` for the next shipping request.
    pub fn applied_lsn(&self) -> Lsn {
        self.applied_lsn
    }

    /// Apply a shipped WAL byte stream (P6.b `ship_wal` output): persist the new
    /// records to the local WAL and redo them onto the page store. Adopts the
    /// primary's `catalog_root` + `next_xid` (control-file state not carried in
    /// the WAL). Returns the new applied LSN. Idempotent — a stream carrying only
    /// already-applied records is a no-op (safe to retry after a crash).
    pub fn apply_stream(&mut self, bytes: &[u8], control: PrimaryControl) -> Result<Lsn> {
        let records: Vec<_> = decode_stream(bytes)?
            .into_iter()
            .filter(|r| r.lsn > self.applied_lsn)
            .collect();
        // Always adopt the primary control (a DDL-only batch may relocate the
        // catalog root with no records past our frontier).
        self.adopt_control(control)?;
        if records.is_empty() {
            return Ok(self.applied_lsn);
        }
        let wal_dir = self.dir.join("db.wal");
        let wal = Wal::open(&wal_dir, self.applied_lsn)?;
        wal.write_shipped(&records)?;
        drop(wal);
        self.applied_lsn = records.iter().map(|r| r.lsn).max().unwrap();
        // Incremental redo on top of the existing page store (normal crash
        // recovery — no wipe): committed records are applied, LSN-gated so
        // already-present pages are skipped.
        recovery::recover(
            &self.dir.join("control"),
            &self.dir.join("data.db"),
            &wal_dir,
            self.page_size,
            self.pool_capacity,
        )?;
        tracing::debug!(applied_lsn = self.applied_lsn, "replica applied stream");
        Ok(self.applied_lsn)
    }

    /// Adopt the primary's catalog root + next-xid into the replica's control
    /// file (the catalog *content* rides the WAL; its root pointer + xid counter
    /// are control-file state that must travel alongside).
    fn adopt_control(&self, control: PrimaryControl) -> Result<()> {
        let ctrl_p = self.dir.join("control");
        let mut cd = control::read(&ctrl_p)?;
        cd.catalog_root = control.catalog_root;
        if control.next_xid > cd.next_xid {
            cd.next_xid = control.next_xid;
        }
        control::write(&ctrl_p, &cd)?;
        Ok(())
    }

    /// Open a read-only-usable `Engine` over the replica's applied state. (v1
    /// opens a full `Engine`; treat it as read-only — writing would diverge from
    /// the primary. A hard read-only guard is a follow-up.)
    pub fn read_engine(&self) -> Result<Engine> {
        Engine::open(&self.dir, 0)
    }

    /// **Failover**: promote this replica to a read-write primary. Consumes the
    /// replica and returns a normal `Engine` over its directory — every
    /// acknowledged commit that was shipped and applied survives.
    pub fn promote(self) -> Result<Engine> {
        tracing::info!(dir = %self.dir.display(), applied_lsn = self.applied_lsn, "replica promoted to primary");
        Engine::open(&self.dir, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn row_count(rows: &crate::sql::executor::ExecResult) -> usize {
        match rows {
            crate::sql::executor::ExecResult::Rows { rows: r, .. } => r.len(),
            other => panic!("expected rows, got {other:?}"),
        }
    }

    /// Assert `rid` reads back as `expected` through `engine`.
    fn assert_row(engine: &Engine, rid: crate::heap::RowId, expected: &[u8]) {
        let x = engine.begin().unwrap();
        assert_eq!(engine.get(x, rid).unwrap(), expected, "row {rid:?}");
        engine.commit(x).unwrap();
    }

    /// Assert `rid` is NOT visible through `engine` (was never durably applied).
    fn assert_absent(engine: &Engine, rid: crate::heap::RowId) {
        let x = engine.begin().unwrap();
        assert!(
            engine.get(x, rid).is_err(),
            "row {rid:?} must be absent (uncommitted tail)"
        );
        engine.commit(x).unwrap();
    }

    /// C3 — WAL shipping is capped at the durable frontier, so a replica can
    /// never get ahead of the primary. An unsynced tail (an open, uncommitted
    /// transaction under the group-committed default) is written to the WAL file
    /// but is *not* durable; it must not ship. When the primary then "crashes"
    /// before fsyncing that tail and restarts, recovery drops it — and the
    /// replica, having only ever received durable records, is a prefix of the
    /// recovered primary (no divergence on failover).
    ///
    /// Uses the raw byte-slice heap (`Engine::insert`/`get`) so the M1 catalog —
    /// which is persisted eagerly and non-MVCC, and whose root can move when a
    /// table allocates pages — does not confound the WAL durable-frontier cap
    /// this test isolates.
    #[test]
    fn shipping_capped_at_durable_lsn_keeps_replica_a_prefix_on_primary_crash() {
        let primary_dir = tempdir().unwrap();
        let replica_dir = tempdir().unwrap();

        // Durable base: row 1, checkpointed to a clean base point.
        let primary = Engine::open(primary_dir.path(), 0).unwrap();
        let xid = primary.begin().unwrap();
        let rid1 = primary.insert(xid, b"durable-row-1").unwrap();
        primary.commit(xid).unwrap();
        primary.checkpoint().unwrap();
        let base_lsn = primary.wal_current_lsn();
        let mut replica = Replica::init_from_base(replica_dir.path(), primary_dir.path()).unwrap();

        // A second durable (committed) row, shipped + applied normally.
        let xid = primary.begin().unwrap();
        let rid2 = primary.insert(xid, b"durable-row-2").unwrap();
        primary.commit(xid).unwrap();
        let stream = primary.ship_wal(base_lsn).unwrap();
        replica
            .apply_stream(&stream, primary.primary_control())
            .unwrap();

        let durable = primary.wal_durable_lsn();

        // The unsynced tail: an open transaction inserting large (~7 KiB) rows,
        // never committed. Big rows overflow the WAL writer's 8 KiB buffer and
        // allocate fresh pages (each logging an 8 KiB FPI), so the records are
        // flushed from the buffer to the OS file — but never fsynced, so they
        // sit on disk PAST the durable frontier.
        let big = vec![b'x'; 7000];
        let xid_uncommitted = primary.begin().unwrap();
        let mut tail_rids = Vec::new();
        for _ in 0..10 {
            tail_rids.push(primary.insert(xid_uncommitted, &big).unwrap());
        }
        assert_eq!(
            primary.wal_durable_lsn(),
            durable,
            "an uncommitted tail must not advance the durable frontier"
        );
        assert!(
            primary.wal_current_lsn() > durable,
            "the uncommitted tail is past the durable frontier"
        );
        // Prove the cap actually filters ON-DISK records (not merely that
        // buffered records never reached the file): the raw WAL on disk holds
        // records past the durable frontier.
        let wal_dir = primary_dir.path().join("db.wal");
        let on_disk_max = Wal::scan_file(&wal_dir)
            .unwrap()
            .iter()
            .map(|r| r.lsn)
            .max()
            .unwrap();
        assert!(
            on_disk_max > durable,
            "uncommitted records must be physically on disk past durable to exercise the cap (on_disk_max={on_disk_max}, durable={durable})"
        );

        // The cap: `ship_wal` returns only records at or below the durable
        // frontier — none of the unsynced tail.
        let stream = primary.ship_wal(replica.applied_lsn()).unwrap();
        for r in decode_stream(&stream).unwrap() {
            assert!(
                r.lsn <= durable,
                "shipped record lsn {} exceeds the durable frontier {durable}",
                r.lsn
            );
        }
        replica
            .apply_stream(&stream, primary.primary_control())
            .unwrap();

        // The replica has the two durable rows and none of the uncommitted tail.
        {
            let re = replica.read_engine().unwrap();
            assert_row(&re, rid1, b"durable-row-1");
            assert_row(&re, rid2, b"durable-row-2");
            for rid in &tail_rids {
                assert_absent(&re, *rid);
            }
        }
        let replica_applied_before_crash = replica.applied_lsn();

        // "Crash" the primary before it ever fsyncs the tail, then restart:
        // recovery undoes the incomplete transaction. The primary's durable
        // state is exactly the two committed rows — a *superset* of nothing the
        // replica lacks, i.e. the replica is a prefix (no divergence).
        drop(primary);
        let primary = Engine::open(primary_dir.path(), 0).unwrap();
        assert_row(&primary, rid1, b"durable-row-1");
        assert_row(&primary, rid2, b"durable-row-2");
        for rid in &tail_rids {
            assert_absent(&primary, *rid);
        }

        // The replica never advanced past what the primary made durable: its
        // applied frontier is at or below the primary's durable frontier, so on
        // failover it can never hold a commit the recovered primary lost.
        assert!(
            replica_applied_before_crash <= durable,
            "replica applied frontier {replica_applied_before_crash} must not exceed the primary's durable frontier {durable}"
        );

        // Re-ship from the recovered primary: the replica catches up and remains
        // a faithful prefix — the two durable rows, still no tail.
        let stream = primary.ship_wal(replica.applied_lsn()).unwrap();
        replica
            .apply_stream(&stream, primary.primary_control())
            .unwrap();
        {
            let re = replica.read_engine().unwrap();
            assert_row(&re, rid1, b"durable-row-1");
            assert_row(&re, rid2, b"durable-row-2");
            for rid in &tail_rids {
                assert_absent(&re, *rid);
            }
        }
    }

    // Base snapshot + incremental WAL apply: the replica serves rows that landed
    // on the primary *after* the base, then promotes to a writable primary.
    #[test]
    fn base_plus_incremental_then_promote() {
        let primary_dir = tempdir().unwrap();
        let replica_dir = tempdir().unwrap();

        // Primary writes an initial row and checkpoints — a clean base point.
        let primary = Engine::open(primary_dir.path(), 0).unwrap();
        let xid = primary.begin().unwrap();
        primary
            .execute_sql(xid, "CREATE TABLE t (id INT, name TEXT)")
            .unwrap();
        primary
            .execute_sql(xid, "INSERT INTO t (id, name) VALUES (1, 'alpha')")
            .unwrap();
        primary.commit(xid).unwrap();
        primary.checkpoint().unwrap();

        // Take a base snapshot and seed the replica from it.
        let base_lsn = primary.wal_current_lsn();
        let mut replica = Replica::init_from_base(replica_dir.path(), primary_dir.path()).unwrap();

        // Primary writes MORE after the base.
        let xid = primary.begin().unwrap();
        primary
            .execute_sql(xid, "INSERT INTO t (id, name) VALUES (2, 'beta')")
            .unwrap();
        primary
            .execute_sql(xid, "INSERT INTO t (id, name) VALUES (3, 'gamma')")
            .unwrap();
        primary.commit(xid).unwrap();

        // Ship the post-base WAL and apply it incrementally.
        let stream = primary.ship_wal(base_lsn).unwrap();
        replica
            .apply_stream(&stream, primary.primary_control())
            .unwrap();

        // The replica serves all three rows (base + shipped).
        {
            let re = replica.read_engine().unwrap();
            let xid = re.begin().unwrap();
            let rows = re.execute_sql(xid, "SELECT id FROM t").unwrap();
            re.commit(xid).unwrap();
            assert_eq!(
                row_count(&rows[0]),
                3,
                "replica must serve base + shipped rows"
            );
        }

        // Failover: promote and keep writing.
        let promoted = replica.promote().unwrap();
        let xid = promoted.begin().unwrap();
        promoted
            .execute_sql(xid, "INSERT INTO t (id, name) VALUES (4, 'delta')")
            .unwrap();
        promoted.commit(xid).unwrap();
        let xid = promoted.begin().unwrap();
        let rows = promoted.execute_sql(xid, "SELECT id FROM t").unwrap();
        promoted.commit(xid).unwrap();
        assert_eq!(
            row_count(&rows[0]),
            4,
            "promoted primary keeps rows and accepts writes"
        );
    }

    // Applying the same stream twice is a no-op (idempotent — crash-safe retry).
    #[test]
    fn apply_is_idempotent() {
        let primary_dir = tempdir().unwrap();
        let replica_dir = tempdir().unwrap();
        let primary = Engine::open(primary_dir.path(), 0).unwrap();
        // Seed a row before the base so `t` already has a data page (the
        // realistic streaming case; a brand-new heap page allocated only after
        // the base is the fresh-page-reconstruction edge, tracked separately).
        let xid = primary.begin().unwrap();
        primary.execute_sql(xid, "CREATE TABLE t (id INT)").unwrap();
        primary
            .execute_sql(xid, "INSERT INTO t (id) VALUES (1)")
            .unwrap();
        primary.commit(xid).unwrap();
        primary.checkpoint().unwrap();

        let base_lsn = primary.wal_current_lsn();
        let mut replica = Replica::init_from_base(replica_dir.path(), primary_dir.path()).unwrap();

        let xid = primary.begin().unwrap();
        primary
            .execute_sql(xid, "INSERT INTO t (id) VALUES (7)")
            .unwrap();
        primary.commit(xid).unwrap();
        let stream = primary.ship_wal(base_lsn).unwrap();
        let control = primary.primary_control();

        let lsn1 = replica.apply_stream(&stream, control).unwrap();
        let lsn2 = replica.apply_stream(&stream, control).unwrap();
        assert_eq!(lsn1, lsn2, "re-applying the same stream must not advance");

        let re = replica.read_engine().unwrap();
        let xid = re.begin().unwrap();
        let rows = re.execute_sql(xid, "SELECT id FROM t").unwrap();
        re.commit(xid).unwrap();
        assert_eq!(
            row_count(&rows[0]),
            2,
            "idempotent apply keeps the base row + exactly one shipped row"
        );
    }
}
