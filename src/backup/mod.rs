// Backups + point-in-time recovery (P6.d + item 28).
//
// Item 28 (R1) adds time-based PITR via a side timeline index (`timeline.rs`).
// The WAL format is unchanged; `restore_to_time` resolves a wall-clock target
// to the highest committed LSN at or before it, then calls `restore`.

pub mod timeline;
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

use self::timeline::{TimelineIndex, TIMELINE_FILE};

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
/// which checkpoints first). A plain consistent file copy, including the
/// timeline index so that marks written before the base are available on restore.
pub fn base_backup_dir(src: &Path, dest: &Path) -> Result<()> {
    copy_db_dir(src, dest)?;
    // Copy the timeline index alongside the page/WAL data. The file may not
    // exist yet (e.g. no transactions since open) — that is fine.
    let src_tl = src.join(TIMELINE_FILE);
    if src_tl.exists() {
        std::fs::copy(&src_tl, dest.join(TIMELINE_FILE))?;
    }
    Ok(())
}

/// Copy the timeline file from `data_dir` into `archive_dir`. Called by
/// `Engine::archive_wal` alongside WAL segment archiving so that both the WAL
/// and the timeline are available for a time-based restore.
pub fn archive_timeline(data_dir: &Path, archive_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(archive_dir)?;
    let src = data_dir.join(TIMELINE_FILE);
    if src.exists() {
        std::fs::copy(&src, archive_dir.join(TIMELINE_FILE))?;
        tracing::debug!("timeline archived to {}", archive_dir.display());
    }
    Ok(())
}

/// Restore a database to the highest committed LSN at or before `target_ts_micros`
/// (Unix epoch microseconds). Reads the timeline index from `archive`, resolves
/// the target timestamp to an LSN, then calls [`restore`] with that LSN.
///
/// **Time is advisory; LSN is authoritative.** The restored database will reflect
/// every transaction committed at or before the resolved LSN, regardless of
/// wall-clock order. Resolution granularity is one mark per committed transaction.
///
/// Returns `Err` if the timeline has no marks at or before `target_ts_micros`
/// (the target is before any recorded commit).
pub fn restore_to_time(
    base: &Path,
    archive: &Path,
    dest: &Path,
    target_ts_micros: u64,
) -> Result<()> {
    let tl_path = archive.join(TIMELINE_FILE);
    let marks = TimelineIndex::load_from(&tl_path);
    let target_lsn = TimelineIndex::resolve(&marks, target_ts_micros).ok_or_else(|| {
        crate::error::DbError::Recovery(format!(
            "no timeline mark at or before ts_micros={target_ts_micros}; \
             earliest available mark ts={}",
            marks.first().map(|m| m.ts_micros).unwrap_or(0)
        ))
    })?;
    tracing::info!(
        target_ts_micros,
        target_lsn,
        marks_loaded = marks.len(),
        "resolving time-PITR target"
    );
    // Copy the timeline into dest so subsequent operations can use it.
    std::fs::create_dir_all(dest)?;
    if tl_path.exists() {
        std::fs::copy(&tl_path, dest.join(TIMELINE_FILE))?;
    }
    restore(base, archive, dest, Some(target_lsn))
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

    // R1 (item 28): restore_to_time resolves a wall-clock target to the correct
    // LSN. Marks are injected deterministically (not timing-based) so the test
    // is stable under any scheduler.
    #[test]
    fn restore_to_time_deterministic_mark_injection() {
        use crate::backup::timeline::{TimelineMark, TIMELINE_FILE};

        let src = tempdir().unwrap();
        let base = tempdir().unwrap();
        let archive = tempdir().unwrap();

        let engine = Engine::open(src.path(), 0).unwrap();
        let xid = engine.begin().unwrap();
        engine.execute_sql(xid, "CREATE TABLE t (id INT)").unwrap();
        engine
            .execute_sql(xid, "INSERT INTO t (id) VALUES (1)")
            .unwrap();
        engine.commit(xid).unwrap();
        // Take the base backup (includes timeline up to this point).
        engine.base_backup(base.path()).unwrap();

        // Two more commits so there are LSNs to restore to.
        let xid = engine.begin().unwrap();
        engine
            .execute_sql(xid, "INSERT INTO t (id) VALUES (2)")
            .unwrap();
        engine.commit(xid).unwrap();
        let lsn_after_row2 = engine.wal_current_lsn();

        let xid = engine.begin().unwrap();
        engine
            .execute_sql(xid, "INSERT INTO t (id) VALUES (3)")
            .unwrap();
        engine.commit(xid).unwrap();
        let lsn_after_row3 = engine.wal_current_lsn();

        engine.archive_wal(archive.path()).unwrap();

        // Inject deterministic marks into the archive's timeline. This bypasses
        // real wall-clock time so the test is not timing-sensitive.
        let tl_path = archive.path().join(TIMELINE_FILE);
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&tl_path)
                .unwrap();
            // ts=1000 → after row 2; ts=2000 → after row 3.
            f.write_all(
                &TimelineMark {
                    ts_micros: 1000,
                    lsn: lsn_after_row2,
                }
                .to_bytes(),
            )
            .unwrap();
            f.write_all(
                &TimelineMark {
                    ts_micros: 2000,
                    lsn: lsn_after_row3,
                }
                .to_bytes(),
            )
            .unwrap();
        }

        // Restore to ts=1000 → should see rows 1 and 2 only.
        let dest_t1 = tempdir().unwrap();
        restore_to_time(base.path(), archive.path(), dest_t1.path(), 1000).unwrap();
        let restored = Engine::open(dest_t1.path(), 0).unwrap();
        assert_eq!(
            count(&restored, "SELECT id FROM t"),
            2,
            "restore_to_time(ts=1000) must yield exactly 2 rows"
        );

        // Restore to ts=2000 → should see all 3 rows.
        let dest_t2 = tempdir().unwrap();
        restore_to_time(base.path(), archive.path(), dest_t2.path(), 2000).unwrap();
        let restored2 = Engine::open(dest_t2.path(), 0).unwrap();
        assert_eq!(
            count(&restored2, "SELECT id FROM t"),
            3,
            "restore_to_time(ts=2000) must yield all 3 rows"
        );

        // A target before all marks must return an error (no valid mark).
        let dest_err = tempdir().unwrap();
        assert!(
            restore_to_time(base.path(), archive.path(), dest_err.path(), 500).is_err(),
            "restore_to_time before any mark must error"
        );
    }
}
