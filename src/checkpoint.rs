// Checkpoint: flush dirty pages → write checkpoint WAL record → update control
// file → truncate WAL to the checkpoint LSN (D3, D5).

use std::path::Path;
use std::sync::Mutex;

use crate::{
    bufferpool::BufferPool,
    control::{self, ControlData},
    error::Result,
    format::{Lsn, Xid},
    wal::Wal,
};

/// `next_xid` is the transaction manager's current next-xid-to-issue
/// (`TransactionManager::next_xid()`), persisted into the control file
/// here — **must** be captured before truncation, since after this call
/// the WAL may no longer contain any `WAL_TXN_BEGIN` record for
/// `Engine::open`'s recovery scan to find. See `format.rs`'s v2->v3 note
/// and `control.rs`'s module doc for the bug this closes.
/// `wal_retain_lsn` (P6.b) is the WAL retention floor demanded by replication
/// slots — the minimum `restart_lsn` across all slots. The WAL is truncated to
/// `min(checkpoint_lsn, wal_retain_lsn)` so a slot's un-streamed segments are
/// never deleted. `Lsn::MAX` means "no slot floor" (truncate freely to the
/// checkpoint LSN).
pub fn run(
    pool: &BufferPool,
    wal: &Wal,
    control_path: &Path,
    control: &Mutex<ControlData>,
    next_xid: Xid,
    wal_retain_lsn: Lsn,
) -> Result<()> {
    tracing::info!("checkpoint started");

    // 0. C1 durability-claim audit — checkpoint is a **standalone** operation
    //    (no enclosing user transaction whose commit `sync_up_to` would cover
    //    it), so it self-syncs. Under the commit-time-fsync default, statement
    //    mini-txns may have appended WAL records that are not yet durable; force
    //    them durable before `flush_all` so (a) D5 lets every dirty page reach
    //    disk (page LSN <= durable frontier) and (b) the checkpoint reflects a
    //    durable log. Syncing appended-but-uncommitted records is harmless —
    //    recovery undoes any incomplete transaction regardless. Cheap when
    //    nothing is pending (the WAL is already at its frontier).
    wal.sync()?;

    // 1. Flush all dirty pages. D5 is enforced inside flush_page. (No `control`
    //    lock held here — this fsyncs, and the P5.e invariant forbids holding
    //    the control lock across an fsync.)
    pool.flush_all(wal.durable_lsn())?;

    // P1.a: with every dirty page now durably flushed, the on-disk image of
    // every page is clean, so the current interval's full-page images are no
    // longer needed. Reset FPI tracking — the next modification of each page
    // opens a new interval and logs a fresh full-page image.
    pool.clear_fpi_tracking();

    // 2. Write checkpoint record to WAL and fsync (again, no `control` lock).
    let ckpt_lsn = wal.log_checkpoint()?;

    // 3. Update control file with new checkpoint LSN, WAL tail, and xid. Lock
    //    `control` only for this small, fsync-free critical section.
    {
        let mut control = control.lock().unwrap_or_else(|e| e.into_inner());
        control.checkpoint_lsn = ckpt_lsn;
        control.wal_tail_lsn = wal.current_lsn();
        control.next_xid = next_xid;
        control::write(control_path, &control)?;
    }

    // 4. Truncate WAL: records before the truncation floor are now redundant.
    //    The floor is the checkpoint LSN, held back by any replication slot's
    //    retained position (P6.b) so a consumer's WAL is never removed early.
    let truncate_to = ckpt_lsn.min(wal_retain_lsn);
    wal.truncate_before(truncate_to)?;

    tracing::info!(
        checkpoint_lsn = ckpt_lsn,
        truncate_to,
        "checkpoint complete"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bufferpool::BufferPool;
    use crate::control;
    use crate::format::{DEFAULT_PAGE_SIZE, INVALID_LSN};
    use crate::heap::Heap;
    use crate::wal::Wal;
    use tempfile::tempdir;

    #[test]
    fn checkpoint_runs_and_updates_control() {
        let dir = tempdir().unwrap();
        let ctrl_path = dir.path().join("control");
        let ctrl = Mutex::new(control::create(&ctrl_path, DEFAULT_PAGE_SIZE).unwrap());
        let pool =
            BufferPool::open(&dir.path().join("data.db"), DEFAULT_PAGE_SIZE as usize, 16).unwrap();
        let wal = Wal::open(&dir.path().join("db.wal"), INVALID_LSN).unwrap();
        let heap = Heap::new(DEFAULT_PAGE_SIZE as usize);

        heap.insert(b"checkpoint_test", 1, &pool, &wal).unwrap();

        run(&pool, &wal, &ctrl_path, &ctrl, 7, Lsn::MAX).unwrap();
        let ckpt_lsn = ctrl.lock().unwrap().checkpoint_lsn;
        assert!(ckpt_lsn > INVALID_LSN);

        // Verify control file on disk matches.
        let on_disk = control::read(&ctrl_path).unwrap();
        assert_eq!(on_disk.checkpoint_lsn, ckpt_lsn);
        assert_eq!(on_disk.next_xid, 7);
    }
}
