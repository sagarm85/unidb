// Checkpoint: flush dirty pages → write checkpoint WAL record → update control
// file → truncate WAL to the checkpoint LSN (D3, D5).

use std::path::Path;

use crate::{
    bufferpool::BufferPool,
    control::{self, ControlData},
    error::Result,
    wal::Wal,
};

pub fn run(
    pool: &mut BufferPool,
    wal: &mut Wal,
    control_path: &Path,
    control: &mut ControlData,
) -> Result<()> {
    tracing::info!("checkpoint started");

    // 1. Flush all dirty pages. D5 is enforced inside flush_page.
    pool.flush_all(wal.durable_lsn)?;

    // 2. Write checkpoint record to WAL and fsync.
    let ckpt_lsn = wal.log_checkpoint()?;

    // 3. Update control file with new checkpoint LSN and WAL tail.
    control.checkpoint_lsn = ckpt_lsn;
    control.wal_tail_lsn = wal.current_lsn();
    control::write(control_path, control)?;

    // 4. Truncate WAL: records before ckpt_lsn are now redundant.
    wal.truncate_before(ckpt_lsn)?;

    tracing::info!(checkpoint_lsn = ckpt_lsn, "checkpoint complete");
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
        let mut ctrl = control::create(&ctrl_path, DEFAULT_PAGE_SIZE).unwrap();
        let mut pool =
            BufferPool::open(&dir.path().join("data.db"), DEFAULT_PAGE_SIZE as usize, 16)
                .unwrap();
        let mut wal = Wal::open(&dir.path().join("db.wal"), INVALID_LSN).unwrap();
        let mut heap = Heap::new(DEFAULT_PAGE_SIZE as usize);

        heap.insert(b"checkpoint_test", &mut pool, &mut wal).unwrap();

        run(&mut pool, &mut wal, &ctrl_path, &mut ctrl).unwrap();
        assert!(ctrl.checkpoint_lsn > INVALID_LSN);

        // Verify control file on disk matches.
        let on_disk = control::read(&ctrl_path).unwrap();
        assert_eq!(on_disk.checkpoint_lsn, ctrl.checkpoint_lsn);
    }
}
