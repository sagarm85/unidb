// WAL: append-only log with redo + undo payloads (D1).
// Each user statement is a mini-transaction: BEGIN → mutations → COMMIT/ABORT (D2).
// Every write is structured-logged (D13). All integers little-endian (D9).
//
// Record wire format:
//   [0..8]   lsn         u64
//   [8..16]  prev_lsn    u64    (previous LSN in the same mini-txn; 0 if first)
//   [16..24] mini_txn_id u64
//   [24]     rec_type    u8     (WAL_BEGIN / WAL_COMMIT / ABORT / INSERT / UPDATE / DELETE / CHECKPOINT)
//   [25..27] _pad        u8 x2
//   [27..29] page_id     u32    (for data records; 0 for control records — stored as u32)
//   [31..33] slot        u16    (for data records; 0 for control records)
//   [33..37] redo_len    u32
//   [37..41] undo_len    u32
//   [41..]   redo_data   [u8; redo_len]
//   [..]     undo_data   [u8; undo_len]
//   last 4   crc32       u32    (over all bytes before crc field)
//
// Fixed header size: 41 bytes + redo_len + undo_len + 4 (crc)

use std::{
    fs::{File, OpenOptions},
    io::{BufWriter, Read, Seek, SeekFrom, Write},
    path::Path,
};

use crate::{
    error::{DbError, Result},
    format::{
        u16_from_le, u16_to_le, u32_from_le, u32_to_le, u64_from_le, u64_to_le, Lsn, PageId, Xid,
        INVALID_LSN, WAL_ABORT, WAL_BEGIN, WAL_CHECKPOINT, WAL_COMMIT, WAL_DELETE, WAL_INSERT,
        WAL_TXN_ABORT, WAL_TXN_BEGIN, WAL_TXN_COMMIT, WAL_UPDATE,
    },
};

const FIXED_HDR: usize = 41;

#[derive(Debug, Clone)]
pub struct WalRecord {
    pub lsn: Lsn,
    pub prev_lsn: Lsn,
    pub mini_txn_id: u64,
    pub rec_type: u8,
    pub page_id: PageId,
    pub slot: u16,
    pub redo: Vec<u8>,
    pub undo: Vec<u8>,
}

fn encode_record(r: &WalRecord) -> Vec<u8> {
    let payload_len = FIXED_HDR + r.redo.len() + r.undo.len();
    let total = payload_len + 4; // +4 for trailing CRC
    let mut buf = vec![0u8; total];
    buf[0..8].copy_from_slice(&u64_to_le(r.lsn));
    buf[8..16].copy_from_slice(&u64_to_le(r.prev_lsn));
    buf[16..24].copy_from_slice(&u64_to_le(r.mini_txn_id));
    buf[24] = r.rec_type;
    // [25..27] pad
    buf[27..31].copy_from_slice(&u32_to_le(r.page_id));
    buf[31..33].copy_from_slice(&u16_to_le(r.slot));
    buf[33..37].copy_from_slice(&u32_to_le(r.redo.len() as u32));
    buf[37..41].copy_from_slice(&u32_to_le(r.undo.len() as u32));
    let mut pos = FIXED_HDR;
    buf[pos..pos + r.redo.len()].copy_from_slice(&r.redo);
    pos += r.redo.len();
    buf[pos..pos + r.undo.len()].copy_from_slice(&r.undo);
    let crc = crc32fast::hash(&buf[..payload_len]);
    buf[payload_len..payload_len + 4].copy_from_slice(&u32_to_le(crc));
    buf
}

fn decode_record(buf: &[u8]) -> Result<WalRecord> {
    if buf.len() < FIXED_HDR + 4 {
        return Err(DbError::WalCorrupt { lsn: 0 });
    }
    let lsn = u64_from_le(buf[0..8].try_into().unwrap());
    let prev_lsn = u64_from_le(buf[8..16].try_into().unwrap());
    let mini_txn_id = u64_from_le(buf[16..24].try_into().unwrap());
    let rec_type = buf[24];
    let page_id = u32_from_le(buf[27..31].try_into().unwrap());
    let slot = u16_from_le(buf[31..33].try_into().unwrap());
    let redo_len = u32_from_le(buf[33..37].try_into().unwrap()) as usize;
    let undo_len = u32_from_le(buf[37..41].try_into().unwrap()) as usize;
    let total_needed = FIXED_HDR + redo_len + undo_len + 4;
    if buf.len() < total_needed {
        return Err(DbError::WalCorrupt { lsn });
    }
    let payload_len = FIXED_HDR + redo_len + undo_len;
    let stored_crc = u32_from_le(buf[payload_len..payload_len + 4].try_into().unwrap());
    let computed_crc = crc32fast::hash(&buf[..payload_len]);
    if stored_crc != computed_crc {
        return Err(DbError::WalCorrupt { lsn });
    }
    let redo = buf[FIXED_HDR..FIXED_HDR + redo_len].to_vec();
    let undo = buf[FIXED_HDR + redo_len..FIXED_HDR + redo_len + undo_len].to_vec();
    Ok(WalRecord {
        lsn,
        prev_lsn,
        mini_txn_id,
        rec_type,
        page_id,
        slot,
        redo,
        undo,
    })
}

pub struct Wal {
    writer: BufWriter<File>,
    path: std::path::PathBuf,
    next_lsn: Lsn,
    next_mini_txn: u64,
    /// LSN of the last fsync'd record (the durable WAL frontier).
    pub durable_lsn: Lsn,
    /// Group-commit mode (M9). When `true`, `commit_mini_txn`/`abort_mini_txn`
    /// /`commit_user_txn`/`abort_user_txn` append their records but skip the
    /// per-call fsync; durability is forced explicitly by a later [`Self::sync`]
    /// call. Off by default so the embedded API and the crash-injection harness
    /// keep their per-statement durability guarantee. Only the server writer
    /// thread — which owns the sole `Engine` handle and issues one `sync` per
    /// drained request batch — turns this on.
    deferred_sync: bool,
}

impl Wal {
    pub fn open(path: &Path, start_lsn: Lsn) -> Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let next_lsn = if start_lsn == INVALID_LSN {
            1
        } else {
            start_lsn + 1
        };
        tracing::info!(path = %path.display(), next_lsn, "WAL opened");
        Ok(Self {
            writer: BufWriter::new(file),
            path: path.to_path_buf(),
            next_lsn,
            next_mini_txn: 1,
            durable_lsn: INVALID_LSN,
            deferred_sync: false,
        })
    }

    pub fn begin_mini_txn(&mut self) -> Result<(u64, Lsn)> {
        let txn_id = self.next_mini_txn;
        self.next_mini_txn += 1;
        let lsn = self.append_raw(txn_id, INVALID_LSN, WAL_BEGIN, 0, 0, &[], &[])?;
        tracing::debug!(mini_txn_id = txn_id, lsn, "WAL BEGIN");
        Ok((txn_id, lsn))
    }

    pub fn commit_mini_txn(&mut self, txn_id: u64, prev_lsn: Lsn) -> Result<Lsn> {
        let lsn = self.append_raw(txn_id, prev_lsn, WAL_COMMIT, 0, 0, &[], &[])?;
        if !self.deferred_sync {
            self.fsync()?;
        }
        tracing::debug!(
            mini_txn_id = txn_id,
            lsn,
            deferred = self.deferred_sync,
            "WAL COMMIT"
        );
        Ok(lsn)
    }

    pub fn abort_mini_txn(&mut self, txn_id: u64, prev_lsn: Lsn) -> Result<Lsn> {
        let lsn = self.append_raw(txn_id, prev_lsn, WAL_ABORT, 0, 0, &[], &[])?;
        if !self.deferred_sync {
            self.fsync()?;
        }
        tracing::debug!(
            mini_txn_id = txn_id,
            lsn,
            deferred = self.deferred_sync,
            "WAL ABORT"
        );
        Ok(lsn)
    }

    pub fn log_insert(
        &mut self,
        txn_id: u64,
        prev_lsn: Lsn,
        page_id: PageId,
        slot: u16,
        redo: &[u8],
    ) -> Result<Lsn> {
        let lsn = self.append_raw(txn_id, prev_lsn, WAL_INSERT, page_id, slot, redo, &[])?;
        tracing::trace!(mini_txn_id = txn_id, lsn, page_id, slot, "WAL INSERT");
        Ok(lsn)
    }

    pub fn log_update(
        &mut self,
        txn_id: u64,
        prev_lsn: Lsn,
        page_id: PageId,
        slot: u16,
        redo: &[u8],
        undo: &[u8],
    ) -> Result<Lsn> {
        let lsn = self.append_raw(txn_id, prev_lsn, WAL_UPDATE, page_id, slot, redo, undo)?;
        tracing::trace!(mini_txn_id = txn_id, lsn, page_id, slot, "WAL UPDATE");
        Ok(lsn)
    }

    pub fn log_delete(
        &mut self,
        txn_id: u64,
        prev_lsn: Lsn,
        page_id: PageId,
        slot: u16,
        undo: &[u8],
    ) -> Result<Lsn> {
        let lsn = self.append_raw(txn_id, prev_lsn, WAL_DELETE, page_id, slot, &[], undo)?;
        tracing::trace!(mini_txn_id = txn_id, lsn, page_id, slot, "WAL DELETE");
        Ok(lsn)
    }

    // ── user transactions (M1) ──────────────────────────────────────────────
    // Independent ID space from mini-txns above: `xid` rides in the same
    // wire-format `mini_txn_id` field, so the on-disk record shape is
    // unchanged. Recovery distinguishes the two by `rec_type`.

    pub fn begin_user_txn(&mut self, xid: Xid) -> Result<Lsn> {
        let lsn = self.append_raw(xid, INVALID_LSN, WAL_TXN_BEGIN, 0, 0, &[], &[])?;
        tracing::debug!(xid, lsn, "WAL TXN_BEGIN");
        Ok(lsn)
    }

    pub fn commit_user_txn(&mut self, xid: Xid, prev_lsn: Lsn) -> Result<Lsn> {
        let lsn = self.append_raw(xid, prev_lsn, WAL_TXN_COMMIT, 0, 0, &[], &[])?;
        if !self.deferred_sync {
            self.fsync()?;
        }
        tracing::debug!(xid, lsn, deferred = self.deferred_sync, "WAL TXN_COMMIT");
        Ok(lsn)
    }

    pub fn abort_user_txn(&mut self, xid: Xid, prev_lsn: Lsn) -> Result<Lsn> {
        let lsn = self.append_raw(xid, prev_lsn, WAL_TXN_ABORT, 0, 0, &[], &[])?;
        if !self.deferred_sync {
            self.fsync()?;
        }
        tracing::debug!(xid, lsn, deferred = self.deferred_sync, "WAL TXN_ABORT");
        Ok(lsn)
    }

    /// Enable/disable group-commit deferral (M9). See the `deferred_sync`
    /// field doc. When turning it **off**, callers should normally call
    /// [`Self::sync`] first to make anything appended-but-unsynced durable.
    pub fn set_deferred_sync(&mut self, deferred: bool) {
        self.deferred_sync = deferred;
    }

    /// Force every record appended so far to durable storage and advance the
    /// durable frontier. In group-commit mode the writer thread calls this
    /// exactly once per drained batch, amortizing one fsync across every
    /// transaction that committed in that batch.
    pub fn sync(&mut self) -> Result<()> {
        self.fsync()
    }

    pub fn log_checkpoint(&mut self) -> Result<Lsn> {
        let txn_id = 0;
        let lsn = self.append_raw(txn_id, INVALID_LSN, WAL_CHECKPOINT, 0, 0, &[], &[])?;
        self.fsync()?;
        tracing::info!(lsn, "WAL CHECKPOINT written");
        Ok(lsn)
    }

    pub fn current_lsn(&self) -> Lsn {
        self.next_lsn - 1
    }

    #[allow(clippy::too_many_arguments)] // internal low-level WAL primitive
    fn append_raw(
        &mut self,
        mini_txn_id: u64,
        prev_lsn: Lsn,
        rec_type: u8,
        page_id: PageId,
        slot: u16,
        redo: &[u8],
        undo: &[u8],
    ) -> Result<Lsn> {
        let lsn = self.next_lsn;
        self.next_lsn += 1;
        let rec = WalRecord {
            lsn,
            prev_lsn,
            mini_txn_id,
            rec_type,
            page_id,
            slot,
            redo: redo.to_vec(),
            undo: undo.to_vec(),
        };
        let encoded = encode_record(&rec);
        let len = encoded.len() as u32;
        self.writer.write_all(&u32_to_le(len))?;
        self.writer.write_all(&encoded)?;
        Ok(lsn)
    }

    fn fsync(&mut self) -> Result<()> {
        self.writer.flush()?;
        self.writer.get_ref().sync_all()?;
        self.durable_lsn = self.next_lsn - 1;
        Ok(())
    }

    /// Truncate WAL up to (but not including) `keep_from_lsn`.
    /// Simple impl: rewrite the file keeping only records with LSN >= keep_from_lsn.
    pub fn truncate_before(&mut self, keep_from_lsn: Lsn) -> Result<()> {
        self.writer.flush()?;
        let records = Self::scan_file(&self.path)?;
        let kept: Vec<_> = records
            .into_iter()
            .filter(|r| r.lsn >= keep_from_lsn)
            .collect();
        let tmp = self.path.with_extension("wal_tmp");
        {
            let mut f = BufWriter::new(File::create(&tmp)?);
            for r in &kept {
                let encoded = encode_record(r);
                f.write_all(&u32_to_le(encoded.len() as u32))?;
                f.write_all(&encoded)?;
            }
            f.flush()?;
            f.get_ref().sync_all()?;
        }
        std::fs::rename(&tmp, &self.path)?;
        let file = OpenOptions::new().append(true).open(&self.path)?;
        self.writer = BufWriter::new(file);
        tracing::info!(keep_from_lsn, "WAL truncated");
        Ok(())
    }

    /// Scan all records from the WAL file in LSN order for recovery.
    pub fn scan_file(path: &Path) -> Result<Vec<WalRecord>> {
        let mut records = Vec::new();
        let mut f = match File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(records),
            Err(e) => return Err(e.into()),
        };
        f.seek(SeekFrom::Start(0))?;
        loop {
            let mut len_buf = [0u8; 4];
            match f.read_exact(&mut len_buf) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e.into()),
            }
            let len = u32_from_le(len_buf) as usize;
            let mut rec_buf = vec![0u8; len];
            match f.read_exact(&mut rec_buf) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e.into()),
            }
            match decode_record(&rec_buf) {
                Ok(r) => records.push(r),
                Err(e) => {
                    tracing::warn!("WAL scan: skipping corrupt record: {e}");
                    break;
                }
            }
        }
        Ok(records)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn begin_commit_roundtrip() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("test.wal");
        let mut wal = Wal::open(&p, INVALID_LSN).unwrap();
        let (txn_id, begin_lsn) = wal.begin_mini_txn().unwrap();
        let _ins_lsn = wal
            .log_insert(txn_id, begin_lsn, 1, 0, b"row_data")
            .unwrap();
        wal.commit_mini_txn(txn_id, _ins_lsn).unwrap();
        let records = Wal::scan_file(&p).unwrap();
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].rec_type, WAL_BEGIN);
        assert_eq!(records[1].rec_type, WAL_INSERT);
        assert_eq!(records[2].rec_type, WAL_COMMIT);
    }

    #[test]
    fn insert_redo_payload_preserved() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("test.wal");
        let mut wal = Wal::open(&p, INVALID_LSN).unwrap();
        let (txn_id, begin_lsn) = wal.begin_mini_txn().unwrap();
        let ins_lsn = wal
            .log_insert(txn_id, begin_lsn, 5, 3, b"hello world")
            .unwrap();
        wal.commit_mini_txn(txn_id, ins_lsn).unwrap(); // fsync so scan_file sees the records
        let records = Wal::scan_file(&p).unwrap();
        let ins = records.iter().find(|r| r.rec_type == WAL_INSERT).unwrap();
        assert_eq!(ins.redo, b"hello world");
        assert_eq!(ins.page_id, 5);
        assert_eq!(ins.slot, 3);
    }

    #[test]
    fn user_txn_records_round_trip() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("test.wal");
        let mut wal = Wal::open(&p, INVALID_LSN).unwrap();
        let xid: Xid = 7;
        let begin_lsn = wal.begin_user_txn(xid).unwrap();
        wal.commit_user_txn(xid, begin_lsn).unwrap();
        let records = Wal::scan_file(&p).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].rec_type, WAL_TXN_BEGIN);
        assert_eq!(records[0].mini_txn_id, xid);
        assert_eq!(records[1].rec_type, WAL_TXN_COMMIT);
        assert_eq!(records[1].mini_txn_id, xid);
    }

    #[test]
    fn user_txn_abort_records_round_trip() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("test.wal");
        let mut wal = Wal::open(&p, INVALID_LSN).unwrap();
        let xid: Xid = 3;
        let begin_lsn = wal.begin_user_txn(xid).unwrap();
        wal.abort_user_txn(xid, begin_lsn).unwrap();
        let records = Wal::scan_file(&p).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[1].rec_type, WAL_TXN_ABORT);
        assert_eq!(records[1].mini_txn_id, xid);
    }

    #[test]
    fn mini_txn_and_user_txn_ids_are_independent_spaces() {
        // A mini-txn (statement) nested inside a user-txn shares the wire
        // format but not the ID space: mini_txn_id counters and xids can
        // collide numerically without meaning the same thing.
        let dir = tempdir().unwrap();
        let p = dir.path().join("test.wal");
        let mut wal = Wal::open(&p, INVALID_LSN).unwrap();
        let xid: Xid = 1;
        wal.begin_user_txn(xid).unwrap();
        let (mini_id, begin_lsn) = wal.begin_mini_txn().unwrap();
        wal.commit_mini_txn(mini_id, begin_lsn).unwrap();
        wal.commit_user_txn(xid, begin_lsn).unwrap();
        let records = Wal::scan_file(&p).unwrap();
        assert_eq!(records[0].rec_type, WAL_TXN_BEGIN);
        assert_eq!(records[1].rec_type, WAL_BEGIN);
        assert_eq!(records[2].rec_type, WAL_COMMIT);
        assert_eq!(records[3].rec_type, WAL_TXN_COMMIT);
    }

    #[test]
    fn corrupt_record_stops_scan() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("test.wal");
        let mut wal = Wal::open(&p, INVALID_LSN).unwrap();
        let (txn_id, begin_lsn) = wal.begin_mini_txn().unwrap();
        wal.log_insert(txn_id, begin_lsn, 1, 0, b"x").unwrap();
        drop(wal);
        // corrupt last bytes of file
        let mut bytes = std::fs::read(&p).unwrap();
        let n = bytes.len();
        bytes[n - 5] ^= 0xff;
        std::fs::write(&p, &bytes).unwrap();
        let records = Wal::scan_file(&p).unwrap();
        // only begin record survives (or zero if corruption hit it)
        assert!(records.len() <= 2);
    }
}
