// Backups + point-in-time recovery (P6.d).
//
// Built on the P6.a segmented WAL + P6.c base-snapshot machinery:
//
//   * **Base backup** — a consistent snapshot of the DB directory taken after a
//     checkpoint (data.db flushed, WAL truncated to the checkpoint). It is
//     directly openable (restore-to-base) and is the starting point for PITR.
//   * **WAL archiving** — copy the WAL segment files to an archive directory.
//     Segments are append-only once sealed (P6.a), so archiving is a plain file
//     copy; continuous archiving lets a restore roll forward past the base.
//   * **Restore / PITR** — seed a fresh directory from the base, replay the
//     archived WAL up to a chosen **target LSN** (or the latest available), and
//     recover. Restoring to `None` rolls all the way forward; restoring to
//     `Some(lsn)` stops at that point-in-time.
//
// **v1 scope (honest):** PITR is **by LSN** — commit timestamps aren't in the
// WAL yet, so time-based PITR ("restore to 14:32") is a documented follow-up
// (it needs a timestamp on commit records). And, like the read replica (P6.c),
// roll-forward reconstructs changes to pages **present in the base**; a page
// first allocated after the base isn't full-page-image covered, so a
// long-running roll-forward wants a recent base. Take base backups regularly.

use std::path::Path;

use crate::{
    control,
    error::Result,
    format::{Lsn, INVALID_LSN},
    recovery,
    replication::copy_db_dir,
    wal::Wal,
};

/// Copy every WAL segment file from `wal_dir` into `archive_dir` (creating it),
/// overwriting any existing copy. Returns the number of segments archived.
/// Intended to run against a quiescent WAL (or to be re-run to pick up newly
/// sealed segments); the active segment's tail is captured on the next copy.
pub fn archive_wal_dir(wal_dir: &Path, archive_dir: &Path) -> Result<usize> {
    std::fs::create_dir_all(archive_dir)?;
    let mut n = 0;
    if let Ok(rd) = std::fs::read_dir(wal_dir) {
        for entry in rd {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                std::fs::copy(entry.path(), archive_dir.join(entry.file_name()))?;
                n += 1;
            }
        }
    }
    tracing::info!(archived_segments = n, "WAL archived");
    Ok(n)
}

/// Restore a database into `dest` from a base backup + archived WAL, rolling
/// forward to `target_lsn` (or the latest archived record when `None`).
///
/// Steps: seed `dest` from the base's control + page store, rebuild `dest`'s WAL
/// from the archived segments filtered to `lsn <= target`, then run crash
/// recovery. The result is a consistent database as of the target point.
pub fn restore(base: &Path, archive: &Path, dest: &Path, target_lsn: Option<Lsn>) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    // 1. Seed the page store + control from the base (NOT its WAL — we rebuild
    //    that from the archive below so PITR can stop at an arbitrary LSN).
    for name in ["control", "data.db"] {
        let s = base.join(name);
        if s.exists() {
            std::fs::copy(&s, dest.join(name))?;
        }
    }

    // 2. Gather archived records up to the target, in LSN order.
    let mut records = Wal::scan_file(archive)?;
    records.sort_by_key(|r| r.lsn);
    if let Some(target) = target_lsn {
        records.retain(|r| r.lsn <= target);
    }

    // 3. Rebuild the destination WAL from those records (verbatim LSNs).
    let dest_wal = dest.join("db.wal");
    if dest_wal.exists() {
        std::fs::remove_dir_all(&dest_wal)?;
    }
    let wal = Wal::open(&dest_wal, INVALID_LSN)?;
    wal.write_shipped(&records)?;
    drop(wal);

    // 4. Recover: replay the assembled WAL onto the base page store.
    let cd = control::read(&dest.join("control"))?;
    recovery::recover(
        &dest.join("control"),
        &dest.join("data.db"),
        &dest_wal,
        cd.page_size as usize,
        4096,
    )?;
    tracing::info!(?target_lsn, replayed = records.len(), "restore complete");
    Ok(())
}

/// Take a base backup of the DB directory `src` into `dest`. `src` should be a
/// quiescent / just-checkpointed database (see [`crate::Engine::base_backup`],
/// which checkpoints first). A plain consistent file copy.
pub fn base_backup_dir(src: &Path, dest: &Path) -> Result<()> {
    copy_db_dir(src, dest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Engine;
    use tempfile::tempdir;

    fn count(engine: &Engine, sql: &str) -> usize {
        let xid = engine.begin().unwrap();
        let rows = engine.execute_sql(xid, sql).unwrap();
        engine.commit(xid).unwrap();
        match &rows[0] {
            crate::sql::executor::ExecResult::Rows { rows: r, .. } => r.len(),
            other => panic!("expected rows, got {other:?}"),
        }
    }

    // Backup + PITR-by-LSN drill: restore to an intermediate LSN sees the older
    // state; restore to latest sees everything.
    #[test]
    fn backup_and_pitr_by_lsn() {
        let src = tempdir().unwrap();
        let base = tempdir().unwrap();
        let archive = tempdir().unwrap();

        let engine = Engine::open(src.path(), 0).unwrap();
        // A base with one row already on `t`'s data page, then checkpoint + base
        // backup (all subsequent inserts land on that existing page).
        let xid = engine.begin().unwrap();
        engine.execute_sql(xid, "CREATE TABLE t (id INT)").unwrap();
        engine
            .execute_sql(xid, "INSERT INTO t (id) VALUES (1)")
            .unwrap();
        engine.commit(xid).unwrap();
        engine.base_backup(base.path()).unwrap();

        // Three more committed inserts, capturing the LSN after the 3rd.
        for id in [2, 3] {
            let xid = engine.begin().unwrap();
            engine
                .execute_sql(xid, &format!("INSERT INTO t (id) VALUES ({id})"))
                .unwrap();
            engine.commit(xid).unwrap();
        }
        let target = engine.wal_current_lsn();
        for id in [4, 5] {
            let xid = engine.begin().unwrap();
            engine
                .execute_sql(xid, &format!("INSERT INTO t (id) VALUES ({id})"))
                .unwrap();
            engine.commit(xid).unwrap();
        }
        // Archive the WAL (quiescent).
        engine.archive_wal(archive.path()).unwrap();

        // Restore to the target LSN → rows 1,2,3 only.
        let dest_pit = tempdir().unwrap();
        restore(base.path(), archive.path(), dest_pit.path(), Some(target)).unwrap();
        let restored = Engine::open(dest_pit.path(), 0).unwrap();
        assert_eq!(
            count(&restored, "SELECT id FROM t"),
            3,
            "PITR to target LSN"
        );

        // Restore to latest → all 5 rows.
        let dest_latest = tempdir().unwrap();
        restore(base.path(), archive.path(), dest_latest.path(), None).unwrap();
        let restored = Engine::open(dest_latest.path(), 0).unwrap();
        assert_eq!(count(&restored, "SELECT id FROM t"), 5, "restore to latest");
    }

    // A base backup is directly openable and reflects the state at backup time.
    #[test]
    fn base_backup_is_openable() {
        let src = tempdir().unwrap();
        let base = tempdir().unwrap();
        let engine = Engine::open(src.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        engine.execute_sql(xid, "CREATE TABLE t (id INT)").unwrap();
        engine
            .execute_sql(xid, "INSERT INTO t (id) VALUES (1)")
            .unwrap();
        engine.commit(xid).unwrap();
        engine.base_backup(base.path()).unwrap();

        let restored = Engine::open(base.path(), 0).unwrap();
        assert_eq!(count(&restored, "SELECT id FROM t"), 1);
    }
}
