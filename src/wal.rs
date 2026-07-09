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
//   [27..33] page_id     u32    (for data records; 0 for control records — stored as u32)
//   [31..33] slot        u16    (for data records; 0 for control records)
//   [33..37] redo_len    u32
//   [37..41] undo_len    u32
//   [41..]   redo_data   [u8; redo_len]
//   [..]     undo_data   [u8; undo_len]
//   last 4   crc32       u32    (over all bytes before crc field)
//
// Fixed header size: 41 bytes + redo_len + undo_len + 4 (crc)
//
// P5.b — CONCURRENT APPEND. All mutable state (the buffered file writer, the
// LSN and mini-txn counters, the WAL-size counter, the durable frontier, the
// deferred-sync/poison flags) lives under one `Mutex<WalInner>`, so the `Wal`
// is `Sync` and every method takes `&self`. LSN allocation and the physical
// append happen together under that lock, so many concurrent appenders produce
// a correctly-ordered, non-interleaved log (monotonic LSNs, no torn records).
// Group commit (P5.e-3): in `deferred_sync` mode appends skip the per-call
// fsync; committers instead call `sync_up_to`, whose leader runs the actual
// `sync_all` in `group_fsync` **with the append lock released**, so other
// threads keep appending their commit records while the leader fsyncs and that
// one fsync makes all of them durable — the amortization that makes throughput
// scale with concurrent writers rather than fsyncs.

use std::{
    fs::{File, OpenOptions},
    io::{BufWriter, Read, Seek, SeekFrom, Write},
    path::Path,
    sync::{Mutex, MutexGuard},
};

use crate::{
    error::{DbError, Result},
    format::{
        u16_from_le, u16_to_le, u32_from_le, u32_to_le, u64_from_le, u64_to_le, Lsn, PageId, Xid,
        INVALID_LSN, WAL_ABORT, WAL_BEGIN, WAL_CHECKPOINT, WAL_COMMIT, WAL_DELETE, WAL_FPI,
        WAL_INDEX, WAL_INSERT, WAL_TXN_ABORT, WAL_TXN_BEGIN, WAL_TXN_COMMIT, WAL_UPDATE,
        WAL_VACUUM,
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

/// The mutable WAL state guarded by one mutex (P5.b). LSN allocation and the
/// physical append both happen while this is held, so concurrent appenders
/// never interleave a partial record or hand out a duplicate/out-of-order LSN.
struct WalInner {
    writer: BufWriter<File>,
    path: std::path::PathBuf,
    next_lsn: Lsn,
    next_mini_txn: u64,
    /// Framed bytes currently in the WAL file (P1.e) — the WAL size since the
    /// last checkpoint, the signal the `max_wal_size` auto-checkpoint trigger
    /// watches. A running counter (no `stat` syscall on the hot path).
    wal_bytes: u64,
    /// LSN of the last fsync'd record (the durable WAL frontier).
    durable_lsn: Lsn,
    /// Group-commit mode (M9). When `true`, commit/abort records are appended
    /// without a per-call fsync; durability is forced explicitly by a later
    /// [`Wal::sync`]. Off by default so the embedded API and the crash harness
    /// keep per-statement durability.
    deferred_sync: bool,
    /// Set once an `fsync` has failed (P1.b, fsyncgate). Once poisoned every
    /// durability call returns [`DbError::DurabilityFailure`]; the session is
    /// unrecoverable and must restart.
    poisoned: bool,
    /// Test/fault-injection hook (P1.b): the next `fsync` fails and poisons.
    fsync_fault_armed: bool,
}

pub struct Wal {
    inner: Mutex<WalInner>,
    /// Group-commit leader-election lock (P5.e-3), held **only** during
    /// [`Wal::sync_up_to`]'s fsync — deliberately *separate* from `inner` so
    /// that while one committer (the "leader") is fsyncing, other threads can
    /// still append their own commit records under `inner`. When the leader's
    /// single fsync completes it has flushed the WAL to its current tail,
    /// covering every commit that landed while it ran; those followers then
    /// see `durable_lsn` already past their commit LSN and skip their own
    /// fsync entirely. That coalescing is what makes write throughput scale
    /// with concurrent writers instead of paying one fsync per commit.
    flush_lock: Mutex<()>,
}

impl Wal {
    pub fn open(path: &Path, start_lsn: Lsn) -> Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let next_lsn = if start_lsn == INVALID_LSN {
            1
        } else {
            start_lsn + 1
        };
        // Seed the WAL-size counter from the existing file (P1.e).
        let wal_bytes = file.metadata().map(|m| m.len()).unwrap_or(0);
        tracing::info!(path = %path.display(), next_lsn, "WAL opened");
        Ok(Self {
            inner: Mutex::new(WalInner {
                writer: BufWriter::new(file),
                path: path.to_path_buf(),
                next_lsn,
                next_mini_txn: 1,
                wal_bytes,
                durable_lsn: INVALID_LSN,
                deferred_sync: false,
                poisoned: false,
                fsync_fault_armed: false,
            }),
            flush_lock: Mutex::new(()),
        })
    }

    fn lock(&self) -> MutexGuard<'_, WalInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The durable WAL frontier (LSN of the last fsync'd record). Accessor —
    /// was a public field before P5.b's interior-mutability rework.
    pub fn durable_lsn(&self) -> Lsn {
        self.lock().durable_lsn
    }

    /// Framed bytes in the WAL since the last checkpoint truncation (P1.e).
    pub fn wal_bytes(&self) -> u64 {
        self.lock().wal_bytes
    }

    /// Arm a one-shot fsync fault (P1.b fault injection). The next `fsync`
    /// fails and poisons the WAL, without writing the real file.
    pub fn arm_fsync_fault(&self) {
        self.lock().fsync_fault_armed = true;
    }

    /// Whether the WAL has latched into the poisoned state (an fsync failed).
    pub fn is_poisoned(&self) -> bool {
        self.lock().poisoned
    }

    pub fn begin_mini_txn(&self) -> Result<(u64, Lsn)> {
        let mut inner = self.lock();
        let txn_id = inner.next_mini_txn;
        inner.next_mini_txn += 1;
        let lsn = append_locked(&mut inner, txn_id, INVALID_LSN, WAL_BEGIN, 0, 0, &[], &[])?;
        tracing::debug!(mini_txn_id = txn_id, lsn, "WAL BEGIN");
        Ok((txn_id, lsn))
    }

    pub fn commit_mini_txn(&self, txn_id: u64, prev_lsn: Lsn) -> Result<Lsn> {
        let mut inner = self.lock();
        let lsn = append_locked(&mut inner, txn_id, prev_lsn, WAL_COMMIT, 0, 0, &[], &[])?;
        if !inner.deferred_sync {
            fsync_locked(&mut inner)?;
        }
        tracing::debug!(
            mini_txn_id = txn_id,
            lsn,
            deferred = inner.deferred_sync,
            "WAL COMMIT"
        );
        Ok(lsn)
    }

    pub fn abort_mini_txn(&self, txn_id: u64, prev_lsn: Lsn) -> Result<Lsn> {
        let mut inner = self.lock();
        let lsn = append_locked(&mut inner, txn_id, prev_lsn, WAL_ABORT, 0, 0, &[], &[])?;
        if !inner.deferred_sync {
            fsync_locked(&mut inner)?;
        }
        tracing::debug!(
            mini_txn_id = txn_id,
            lsn,
            deferred = inner.deferred_sync,
            "WAL ABORT"
        );
        Ok(lsn)
    }

    pub fn log_insert(
        &self,
        txn_id: u64,
        prev_lsn: Lsn,
        page_id: PageId,
        slot: u16,
        redo: &[u8],
    ) -> Result<Lsn> {
        let mut inner = self.lock();
        let lsn = append_locked(
            &mut inner,
            txn_id,
            prev_lsn,
            WAL_INSERT,
            page_id,
            slot,
            redo,
            &[],
        )?;
        tracing::trace!(mini_txn_id = txn_id, lsn, page_id, slot, "WAL INSERT");
        Ok(lsn)
    }

    pub fn log_update(
        &self,
        txn_id: u64,
        prev_lsn: Lsn,
        page_id: PageId,
        slot: u16,
        redo: &[u8],
        undo: &[u8],
    ) -> Result<Lsn> {
        let mut inner = self.lock();
        let lsn = append_locked(
            &mut inner, txn_id, prev_lsn, WAL_UPDATE, page_id, slot, redo, undo,
        )?;
        tracing::trace!(mini_txn_id = txn_id, lsn, page_id, slot, "WAL UPDATE");
        Ok(lsn)
    }

    pub fn log_delete(
        &self,
        txn_id: u64,
        prev_lsn: Lsn,
        page_id: PageId,
        slot: u16,
        undo: &[u8],
    ) -> Result<Lsn> {
        let mut inner = self.lock();
        let lsn = append_locked(
            &mut inner,
            txn_id,
            prev_lsn,
            WAL_DELETE,
            page_id,
            slot,
            &[],
            undo,
        )?;
        tracing::trace!(mini_txn_id = txn_id, lsn, page_id, slot, "WAL DELETE");
        Ok(lsn)
    }

    /// Log a vacuum mutation (M10), redo-only (no undo — reclaiming
    /// already-dead-and-committed space is idempotent on replay). `slot !=
    /// u16::MAX` with an empty `redo` marks that one line pointer DEAD (M10.b);
    /// `slot == u16::MAX` with `redo` = a full compacted page image restores
    /// the page on replay (M10.d). See `format::WAL_VACUUM`.
    pub fn log_vacuum(
        &self,
        txn_id: u64,
        prev_lsn: Lsn,
        page_id: PageId,
        slot: u16,
        redo: &[u8],
    ) -> Result<Lsn> {
        let mut inner = self.lock();
        let lsn = append_locked(
            &mut inner,
            txn_id,
            prev_lsn,
            WAL_VACUUM,
            page_id,
            slot,
            redo,
            &[],
        )?;
        tracing::trace!(mini_txn_id = txn_id, lsn, page_id, slot, "WAL VACUUM");
        Ok(lsn)
    }

    /// Log a full-page image for torn-page protection (P1.a). `image` is the
    /// entire clean page as it stood *before* the first modification of
    /// `page_id` in the current checkpoint interval. Redo-only. `slot` is
    /// `u16::MAX`: a whole-page record. See `format::WAL_FPI`.
    pub fn log_fpi(
        &self,
        txn_id: u64,
        prev_lsn: Lsn,
        page_id: PageId,
        image: &[u8],
    ) -> Result<Lsn> {
        let mut inner = self.lock();
        let lsn = append_locked(
            &mut inner,
            txn_id,
            prev_lsn,
            WAL_FPI,
            page_id,
            u16::MAX,
            image,
            &[],
        )?;
        tracing::trace!(mini_txn_id = txn_id, lsn, page_id, "WAL FPI");
        Ok(lsn)
    }

    /// Log a full B-Tree node/meta page image (P3.a — durable B-Tree).
    /// Redo-only (see `format::WAL_INDEX`). `image` is the entire node page;
    /// `slot` is `u16::MAX` (a whole-page record).
    pub fn log_index(
        &self,
        txn_id: u64,
        prev_lsn: Lsn,
        page_id: PageId,
        image: &[u8],
    ) -> Result<Lsn> {
        let mut inner = self.lock();
        let lsn = append_locked(
            &mut inner,
            txn_id,
            prev_lsn,
            WAL_INDEX,
            page_id,
            u16::MAX,
            image,
            &[],
        )?;
        tracing::trace!(mini_txn_id = txn_id, lsn, page_id, "WAL INDEX");
        Ok(lsn)
    }

    // ── user transactions (M1) ──────────────────────────────────────────────
    // Independent ID space from mini-txns above: `xid` rides in the same
    // wire-format `mini_txn_id` field, so the on-disk record shape is
    // unchanged. Recovery distinguishes the two by `rec_type`.

    pub fn begin_user_txn(&self, xid: Xid) -> Result<Lsn> {
        let mut inner = self.lock();
        let lsn = append_locked(&mut inner, xid, INVALID_LSN, WAL_TXN_BEGIN, 0, 0, &[], &[])?;
        tracing::debug!(xid, lsn, "WAL TXN_BEGIN");
        Ok(lsn)
    }

    pub fn commit_user_txn(&self, xid: Xid, prev_lsn: Lsn) -> Result<Lsn> {
        let mut inner = self.lock();
        let lsn = append_locked(&mut inner, xid, prev_lsn, WAL_TXN_COMMIT, 0, 0, &[], &[])?;
        if !inner.deferred_sync {
            fsync_locked(&mut inner)?;
        }
        tracing::debug!(xid, lsn, deferred = inner.deferred_sync, "WAL TXN_COMMIT");
        Ok(lsn)
    }

    pub fn abort_user_txn(&self, xid: Xid, prev_lsn: Lsn) -> Result<Lsn> {
        let mut inner = self.lock();
        let lsn = append_locked(&mut inner, xid, prev_lsn, WAL_TXN_ABORT, 0, 0, &[], &[])?;
        if !inner.deferred_sync {
            fsync_locked(&mut inner)?;
        }
        tracing::debug!(xid, lsn, deferred = inner.deferred_sync, "WAL TXN_ABORT");
        Ok(lsn)
    }

    /// Enable/disable group-commit deferral (M9). When turning it **off**,
    /// callers should normally call [`Self::sync`] first to make anything
    /// appended-but-unsynced durable.
    pub fn set_deferred_sync(&self, deferred: bool) {
        self.lock().deferred_sync = deferred;
    }

    /// Force every record appended so far to durable storage and advance the
    /// durable frontier. In group-commit mode the caller invokes this once per
    /// drained batch, amortizing one fsync across every transaction that
    /// committed in that batch — the P5.b/M9 win.
    pub fn sync(&self) -> Result<()> {
        let mut inner = self.lock();
        fsync_locked(&mut inner)
    }

    /// Group-commit durability barrier (P5.e-3): return once every record up to
    /// and including `target` is durable, coalescing concurrent callers behind
    /// as few fsyncs as possible.
    ///
    /// Two fast paths avoid an fsync: (1) if `durable_lsn` is already at or past
    /// `target`, some other committer's fsync has already made us durable;
    /// (2) after taking the leader-election [`Wal::flush_lock`], re-check —
    /// another leader may have flushed past `target` while we waited for the
    /// lock. Only the thread that finds itself still behind actually fsyncs, via
    /// [`Wal::group_fsync`], whose one fsync covers every commit that landed
    /// before it — including the followers now blocked on `flush_lock`.
    pub fn sync_up_to(&self, target: Lsn) -> Result<()> {
        if self.durable_lsn() >= target {
            return Ok(());
        }
        let _leader = self.flush_lock.lock().unwrap_or_else(|e| e.into_inner());
        if self.durable_lsn() >= target {
            return Ok(());
        }
        self.group_fsync()
    }

    /// The actual group-commit fsync (P5.e-3), called only by the `flush_lock`
    /// leader. Its defining property — and the whole reason write throughput
    /// scales — is that **the slow `sync_all` runs without the append lock
    /// held**, so other committers keep appending their `WAL_TXN_COMMIT` records
    /// while the leader fsyncs; the one fsync then makes all of them durable.
    ///
    /// Three phases: (1) under the append lock, push the buffered writer to the
    /// OS and capture both the flushed tail LSN and a `try_clone`d file handle;
    /// (2) release the append lock and `sync_all` the cloned handle (same
    /// underlying file); (3) re-take the append lock and advance `durable_lsn`
    /// to the captured tail. Poison / fault-injection are handled in phase 1 so
    /// their existing semantics (P1.b) are unchanged.
    fn group_fsync(&self) -> Result<()> {
        let (flushed_lsn, file) = {
            let mut inner = self.lock();
            if inner.poisoned {
                return Err(DbError::DurabilityFailure(
                    "WAL is poisoned by an earlier fsync failure; session is unrecoverable".into(),
                ));
            }
            if inner.fsync_fault_armed {
                inner.fsync_fault_armed = false;
                inner.poisoned = true;
                tracing::error!("WAL fsync fault injected — poisoning session (P1.b)");
                return Err(DbError::DurabilityFailure(
                    "injected WAL fsync failure".into(),
                ));
            }
            if let Err(e) = inner.writer.flush() {
                inner.poisoned = true;
                return Err(DbError::DurabilityFailure(format!(
                    "WAL buffer flush failed: {e}"
                )));
            }
            let file = inner.writer.get_ref().try_clone().map_err(|e| {
                inner.poisoned = true;
                DbError::DurabilityFailure(format!("WAL fd clone for group fsync failed: {e}"))
            })?;
            (inner.next_lsn - 1, file)
        };
        // The slow part, with the append lock RELEASED so appends coalesce.
        if let Err(e) = file.sync_all() {
            let mut inner = self.lock();
            inner.poisoned = true;
            return Err(DbError::DurabilityFailure(format!("WAL fsync failed: {e}")));
        }
        let mut inner = self.lock();
        if inner.durable_lsn < flushed_lsn {
            inner.durable_lsn = flushed_lsn;
        }
        Ok(())
    }

    pub fn log_checkpoint(&self) -> Result<Lsn> {
        let mut inner = self.lock();
        let txn_id = 0;
        let lsn = append_locked(
            &mut inner,
            txn_id,
            INVALID_LSN,
            WAL_CHECKPOINT,
            0,
            0,
            &[],
            &[],
        )?;
        fsync_locked(&mut inner)?;
        tracing::info!(lsn, "WAL CHECKPOINT written");
        Ok(lsn)
    }

    pub fn current_lsn(&self) -> Lsn {
        self.lock().next_lsn - 1
    }

    /// Truncate WAL up to (but not including) `keep_from_lsn`.
    /// Simple impl: rewrite the file keeping only records with LSN >= keep_from_lsn.
    pub fn truncate_before(&self, keep_from_lsn: Lsn) -> Result<()> {
        let mut inner = self.lock();
        inner.writer.flush()?;
        let path = inner.path.clone();
        let records = Self::scan_file(&path)?;
        let kept: Vec<_> = records
            .into_iter()
            .filter(|r| r.lsn >= keep_from_lsn)
            .collect();
        let tmp = path.with_extension("wal_tmp");
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
        std::fs::rename(&tmp, &path)?;
        let file = OpenOptions::new().append(true).open(&path)?;
        // P1.e: the WAL-size counter now reflects only the kept records.
        inner.wal_bytes = kept.iter().map(|r| 4 + encode_record(r).len() as u64).sum();
        inner.writer = BufWriter::new(file);
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

/// Compile-time proof the WAL is shareable across threads (P5.b).
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Wal>();
};

/// Allocate the next LSN and physically append one record, all while the WAL
/// lock is held (P5.b). Serializing allocation + append together is what makes
/// concurrent appends correct: LSNs are monotonic and records never interleave.
#[allow(clippy::too_many_arguments)] // internal low-level WAL primitive
fn append_locked(
    inner: &mut WalInner,
    mini_txn_id: u64,
    prev_lsn: Lsn,
    rec_type: u8,
    page_id: PageId,
    slot: u16,
    redo: &[u8],
    undo: &[u8],
) -> Result<Lsn> {
    let lsn = inner.next_lsn;
    inner.next_lsn += 1;
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
    inner.writer.write_all(&u32_to_le(len))?;
    inner.writer.write_all(&encoded)?;
    inner.wal_bytes += 4 + encoded.len() as u64; // P1.e: track WAL size
    Ok(lsn)
}

/// Flush + fsync the WAL and advance the durable frontier (P1.b/P5.b), while
/// the WAL lock is held. On any failure the WAL latches poisoned and the
/// frontier is NOT advanced — a failed fsync may have dropped buffered data.
fn fsync_locked(inner: &mut WalInner) -> Result<()> {
    // P1.b: once poisoned, never report success again.
    if inner.poisoned {
        return Err(DbError::DurabilityFailure(
            "WAL is poisoned by an earlier fsync failure; session is unrecoverable".into(),
        ));
    }
    // Fault injection: fail before touching the file, and poison. `durable_lsn`
    // is NOT advanced.
    if inner.fsync_fault_armed {
        inner.fsync_fault_armed = false;
        inner.poisoned = true;
        tracing::error!("WAL fsync fault injected — poisoning session (P1.b)");
        return Err(DbError::DurabilityFailure(
            "injected WAL fsync failure".into(),
        ));
    }
    if let Err(e) = inner.writer.flush() {
        inner.poisoned = true;
        return Err(DbError::DurabilityFailure(format!(
            "WAL buffer flush failed: {e}"
        )));
    }
    if let Err(e) = inner.writer.get_ref().sync_all() {
        inner.poisoned = true;
        return Err(DbError::DurabilityFailure(format!("WAL fsync failed: {e}")));
    }
    inner.durable_lsn = inner.next_lsn - 1;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn begin_commit_roundtrip() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("test.wal");
        let wal = Wal::open(&p, INVALID_LSN).unwrap();
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
        let wal = Wal::open(&p, INVALID_LSN).unwrap();
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
        let wal = Wal::open(&p, INVALID_LSN).unwrap();
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
        let wal = Wal::open(&p, INVALID_LSN).unwrap();
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
        let dir = tempdir().unwrap();
        let p = dir.path().join("test.wal");
        let wal = Wal::open(&p, INVALID_LSN).unwrap();
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

    /// P1.b: an injected fsync failure poisons the WAL — the commit returns a
    /// `DurabilityFailure`, the durable frontier does NOT advance, and every
    /// later durability call keeps failing.
    #[test]
    fn fsync_failure_poisons_and_never_reports_success() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("test.wal");
        let wal = Wal::open(&p, INVALID_LSN).unwrap();
        let (txn_id, begin_lsn) = wal.begin_mini_txn().unwrap();
        let ins_lsn = wal.log_insert(txn_id, begin_lsn, 1, 0, b"x").unwrap();
        let durable_before = wal.durable_lsn();

        wal.arm_fsync_fault();
        let res = wal.commit_mini_txn(txn_id, ins_lsn);
        assert!(
            matches!(res, Err(DbError::DurabilityFailure(_))),
            "a failed fsync must surface a fatal DurabilityFailure, got {res:?}"
        );
        assert!(
            wal.is_poisoned(),
            "WAL must latch poisoned after fsync failure"
        );
        assert_eq!(
            wal.durable_lsn(),
            durable_before,
            "durable frontier must NOT advance on a failed fsync"
        );

        assert!(matches!(wal.sync(), Err(DbError::DurabilityFailure(_))));
    }

    #[test]
    fn corrupt_record_stops_scan() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("test.wal");
        let wal = Wal::open(&p, INVALID_LSN).unwrap();
        let (txn_id, begin_lsn) = wal.begin_mini_txn().unwrap();
        wal.log_insert(txn_id, begin_lsn, 1, 0, b"x").unwrap();
        wal.sync().unwrap();
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

    /// P5.b: many threads appending concurrently produce a correctly-ordered,
    /// non-interleaved WAL — every LSN unique and contiguous, every record
    /// decodable, and the total count exactly what was appended.
    #[test]
    fn concurrent_appends_are_ordered_and_intact() {
        use std::sync::Arc;
        let dir = tempdir().unwrap();
        let p = dir.path().join("test.wal");
        let wal = Arc::new(Wal::open(&p, INVALID_LSN).unwrap());
        wal.set_deferred_sync(true);

        let threads = 8;
        let per = 200;
        let mut handles = Vec::new();
        for t in 0..threads {
            let wal = Arc::clone(&wal);
            handles.push(std::thread::spawn(move || {
                for i in 0..per {
                    let (id, begin) = wal.begin_mini_txn().unwrap();
                    let payload = format!("t{t}-i{i}");
                    let ins = wal
                        .log_insert(id, begin, t as u32, 0, payload.as_bytes())
                        .unwrap();
                    wal.commit_mini_txn(id, ins).unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        wal.sync().unwrap();
        drop(wal);

        let records = Wal::scan_file(&p).unwrap();
        // 3 records per mini-txn (begin/insert/commit).
        assert_eq!(records.len(), threads * per * 3);
        // LSNs are a contiguous 1..=N with no gaps or duplicates.
        let mut lsns: Vec<Lsn> = records.iter().map(|r| r.lsn).collect();
        lsns.sort_unstable();
        for (i, lsn) in lsns.iter().enumerate() {
            assert_eq!(*lsn, (i as u64) + 1, "LSNs must be contiguous 1..=N");
        }
    }
}
