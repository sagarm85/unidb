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
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Condvar, Mutex, MutexGuard,
    },
    thread,
    time::Instant,
};

use crate::metrics::{AtomicHistogram, HistogramSnapshot};

use crate::{
    error::{DbError, Result},
    format::{
        u16_from_le, u16_to_le, u32_from_le, u32_to_le, u64_from_le, u64_to_le, Lsn, PageId, Xid,
        INVALID_LSN, WAL_ABORT, WAL_BEGIN, WAL_CHECKPOINT, WAL_COMMIT, WAL_DELETE, WAL_FPI,
        WAL_HOT_UPDATE, WAL_HOT_XPAGE_BATCH, WAL_HOT_XPAGE_HEAD, WAL_INDEX, WAL_INDEX_INSERT,
        WAL_INSERT, WAL_INSERT_BATCH, WAL_TXN_ABORT, WAL_TXN_BEGIN, WAL_TXN_COMMIT, WAL_UPDATE,
        WAL_VACUUM, WAL_XMAX_BATCH,
    },
};

const FIXED_HDR: usize = 41;

// ── Segmented WAL (P6.a) ──────────────────────────────────────────────────────
//
// The WAL is no longer one ever-growing file that is rewritten to truncate.
// It is a **directory** of fixed-size *segment* files (`seg-<NNNNNNNNNN>.wal`).
// Records append to the highest-numbered (active) segment; when that segment
// fills past `segment_size` the WAL **seals** it (flush + fsync) and **rotates**
// to a fresh segment. Recovery scans every segment in index order; truncation
// deletes **whole consumed segments** (no rewrite). This is what makes cheap
// WAL retention + concurrent WAL readers (replication slots, P6.b) possible.
//
// A record is never split across segments: an oversized record (e.g. an 8 KiB
// full-page image larger than `segment_size`) lands whole in its own segment.
//
// This evolves D6's "single-file for now — WAL may be separate, revisit
// post-M4" note; the *data store* stays a single file. Human sign-off recorded
// in PROGRESS.md (Phase 6, 2026-07-09).

/// Segment file header magic ("WSEG" little-endian).
const SEG_MAGIC: u32 = 0x4745_5357;
/// Segment file format version.
const SEG_VERSION: u16 = 1;
/// Bytes at the head of every segment file: magic(4) + version(2) + pad(2) +
/// base_lsn(8). The record stream follows.
const SEG_HDR: u64 = 16;
/// Default segment size (16 MiB). Overridable at open via
/// `UNIDB_WAL_SEGMENT_BYTES` or [`Wal::open_with_segment_size`].
const DEFAULT_SEGMENT_SIZE: u64 = 16 * 1024 * 1024;
/// Filename prefix + extension for segment files.
const SEG_PREFIX: &str = "seg-";
const SEG_EXT: &str = "wal";

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

/// Path of segment file `idx` inside the WAL directory `dir`.
fn segment_path(dir: &Path, idx: u64) -> PathBuf {
    dir.join(format!("{SEG_PREFIX}{idx:010}.{SEG_EXT}"))
}

/// Parse a segment index out of a filename (`seg-0000000003.wal` → 3).
fn parse_segment_idx(name: &str) -> Option<u64> {
    let rest = name.strip_prefix(SEG_PREFIX)?;
    let num = rest.strip_suffix(&format!(".{SEG_EXT}"))?;
    num.parse::<u64>().ok()
}

/// List the WAL directory's segment files, sorted ascending by index. A missing
/// directory yields an empty list (a brand-new / never-written WAL).
fn list_segments(dir: &Path) -> Result<Vec<(u64, PathBuf)>> {
    let mut out = Vec::new();
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e.into()),
    };
    for entry in rd {
        let entry = entry?;
        if let Some(name) = entry.file_name().to_str() {
            if let Some(idx) = parse_segment_idx(name) {
                out.push((idx, entry.path()));
            }
        }
    }
    out.sort_by_key(|(idx, _)| *idx);
    Ok(out)
}

/// Read the `base_lsn` (the LSN the first record in this segment will carry)
/// from a segment file header. A short/garbled header reports base_lsn 0.
fn read_segment_base_lsn(path: &Path) -> Result<Lsn> {
    let mut f = File::open(path)?;
    let mut hdr = [0u8; SEG_HDR as usize];
    if f.read_exact(&mut hdr).is_err() {
        return Ok(0);
    }
    let magic = u32_from_le(hdr[0..4].try_into().unwrap());
    if magic != SEG_MAGIC {
        return Ok(0);
    }
    Ok(u64_from_le(hdr[8..16].try_into().unwrap()))
}

fn segment_header_bytes(base_lsn: Lsn) -> [u8; SEG_HDR as usize] {
    let mut hdr = [0u8; SEG_HDR as usize];
    hdr[0..4].copy_from_slice(&u32_to_le(SEG_MAGIC));
    hdr[4..6].copy_from_slice(&u16_to_le(SEG_VERSION));
    // [6..8] pad
    hdr[8..16].copy_from_slice(&u64_to_le(base_lsn));
    hdr
}

/// Create a fresh segment file, write its header, and return a writer
/// positioned right after the header (ready to append records).
fn create_segment(dir: &Path, idx: u64, base_lsn: Lsn) -> Result<BufWriter<File>> {
    let path = segment_path(dir, idx);
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)?;
    file.write_all(&segment_header_bytes(base_lsn))?;
    Ok(BufWriter::new(file))
}

// ── Background segment sealer (item 89) ─────────────────────────────────────
//
// On rotation the active segment is fully written but fsync-ing it inline
// blocks the append mutex for a (rare) full F_FULLFSYNC cost. The sealer moves
// that fsync off the append path:
//
//   1. Rotation: flush BufWriter, try_clone the FD, hand `(clone, sealed_lsn)`
//      to the sealer, open segment N+1, return immediately.
//   2. Sealer thread: wakes on condvar, calls sync_all on the cloned FD,
//      then updates `done_lsn` and broadcasts.
//   3. sync_up_to / sync: after handling the active segment as before, if the
//      requested LSN is covered by a segment currently being sealed by the
//      background thread, wait on the condvar until sealer signals done.
//
// The sealer processes at most one segment at a time (rotation is rare — one
// per 16 MiB of WAL); it is a simple Option<work_item> + Condvar, no queue.
// A poisoned WAL propagates into the sealer: if sync_all fails the sealer
// stores the error and the next `seal_done_or_wait` call re-raises it.

struct SealerState {
    /// The segment to be fsynced: (cloned FD, last LSN in that segment).
    pending: Option<(File, Lsn)>,
    /// The highest LSN confirmed durable by the background fsync.
    done_lsn: Lsn,
    /// The sealer thread should exit when this is true and pending is None.
    shutdown: bool,
    /// If the sealer's sync_all failed, the error message is stored here
    /// so the next waiter can propagate it.
    error: Option<String>,
}

struct Sealer {
    state: Arc<(Mutex<SealerState>, Condvar)>,
    _thread: thread::JoinHandle<()>,
}

impl Sealer {
    /// Start the background sealer thread. Returns both the `Sealer` (owns the
    /// thread handle) and a shared `Arc` of the sealer state that can be stored
    /// in `WalInner` so free functions can call `submit` without a `Wal` reference.
    fn start() -> (Self, Arc<(Mutex<SealerState>, Condvar)>) {
        let state: Arc<(Mutex<SealerState>, Condvar)> = Arc::new((
            Mutex::new(SealerState {
                pending: None,
                done_lsn: INVALID_LSN,
                shutdown: false,
                error: None,
            }),
            Condvar::new(),
        ));
        let state_clone = Arc::clone(&state);
        let handle = thread::Builder::new()
            .name("unidb-wal-sealer".into())
            .spawn(move || {
                let (mtx, cv) = &*state_clone;
                loop {
                    let work = {
                        let mut st = mtx.lock().unwrap_or_else(|e| e.into_inner());
                        loop {
                            if let Some(item) = st.pending.take() {
                                break Some(item);
                            }
                            if st.shutdown {
                                break None;
                            }
                            st = cv.wait(st).unwrap_or_else(|e| e.into_inner());
                        }
                    };
                    match work {
                        None => break, // shutdown and no more pending work
                        Some((file, sealed_lsn)) => {
                            let result = file.sync_all();
                            let mut st = mtx.lock().unwrap_or_else(|e| e.into_inner());
                            if let Err(e) = result {
                                st.error =
                                    Some(format!("WAL background sealer sync_all failed: {e}"));
                            } else if st.done_lsn < sealed_lsn {
                                st.done_lsn = sealed_lsn;
                            }
                            cv.notify_all();
                        }
                    }
                }
            })
            .expect("failed to spawn WAL sealer thread");
        let state_for_inner = Arc::clone(&state);
        (
            Self {
                state,
                _thread: handle,
            },
            state_for_inner,
        )
    }

    /// Wait until all segments with LSN ≤ `target` are fsynced by the sealer.
    /// Returns `Ok(())` if already durable or the sealer confirms durability.
    /// Returns `Err` if the sealer's sync_all failed.
    fn wait_until_durable(&self, target: Lsn) -> Result<()> {
        let (mtx, cv) = &*self.state;
        let mut st = mtx.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(ref msg) = st.error {
                return Err(DbError::DurabilityFailure(msg.clone()));
            }
            if st.done_lsn >= target {
                return Ok(());
            }
            // If there's no pending work and done_lsn < target, the sealer hasn't
            // been asked to seal anything covering `target`. This means `target`
            // is in the active (not-yet-rotated) segment — caller handles that.
            if st.pending.is_none() {
                return Ok(()); // not a sealer-covered segment
            }
            st = cv.wait(st).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Signal the sealer thread to exit and wait for it to drain.
    fn shutdown(&self) {
        let (mtx, cv) = &*self.state;
        let mut st = mtx.lock().unwrap_or_else(|e| e.into_inner());
        st.shutdown = true;
        cv.notify_all();
    }
}

/// The mutable WAL state guarded by one mutex (P5.b). LSN allocation and the
/// physical append both happen while this is held, so concurrent appenders
/// never interleave a partial record or hand out a duplicate/out-of-order LSN.
///
/// P6.a: `writer` targets the **active** (highest-numbered) segment in `dir`;
/// `active_seg`/`active_base_lsn`/`active_bytes` track it for rotation.
struct WalInner {
    writer: BufWriter<File>,
    dir: PathBuf,
    /// Index of the segment `writer` is appending to.
    active_seg: u64,
    /// LSN of the first record that segment `active_seg` will hold.
    active_base_lsn: Lsn,
    /// Physical bytes in the active segment (header + records so far), used to
    /// decide when to seal + rotate.
    active_bytes: u64,
    /// Max segment size before sealing + rotating (a record is never split, so
    /// a single oversized record may exceed this).
    segment_size: u64,
    next_lsn: Lsn,
    next_mini_txn: u64,
    /// Framed bytes currently in the WAL file (P1.e) — the WAL size since the
    /// last checkpoint, the signal the `max_wal_size` auto-checkpoint trigger
    /// watches. A running counter (no `stat` syscall on the hot path).
    wal_bytes: u64,
    /// Cumulative framed bytes ever appended to the WAL — never reset by a
    /// checkpoint truncation, unlike `wal_bytes`. Measurement-only (Phase A C1):
    /// lets a benchmark diff WAL volume across a window of operations even when
    /// an auto-checkpoint truncates the log mid-run.
    total_bytes_appended: u64,
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
    /// Shared state of the background segment sealer (item 89). Stored inside
    /// WalInner so the free functions (rotate_segment, etc.) can submit segments
    /// without needing a reference to the Wal struct. The background thread holds
    /// its own Arc clone; the Wal struct holds the Sealer (which contains the
    /// JoinHandle) for lifecycle management.
    sealer_state: Arc<(Mutex<SealerState>, Condvar)>,
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
    /// Durability-cost observability (item 21). Recorded around the actual
    /// `sync_all` in both fsync paths — deliberately **outside** the `inner`
    /// lock (in `group_fsync` the fsync itself runs lock-free), so the
    /// histogram add never lengthens the commit critical section. `fsyncs`
    /// counts only fsyncs that actually reached the platter (fast-path skips
    /// and coalesced followers are not counted), so `commits / fsyncs`
    /// exposes the group-commit amortization ratio.
    fsyncs: AtomicU64,
    fsync_latency: AtomicHistogram,
    /// Background segment sealer (item 89): moves the rotation fsync off the
    /// append-mutex critical path. On rotation we flush + try_clone the current
    /// segment FD, hand it to the sealer, and open N+1 immediately — the
    /// append lock is released before sync_all runs.
    sealer: Sealer,
}

impl Wal {
    /// Open (or create) the segmented WAL rooted at directory `dir`. `start_lsn`
    /// is the last LSN known durable from the control file; the next record is
    /// `start_lsn + 1` (or 1 for a fresh WAL). Segment size comes from
    /// `UNIDB_WAL_SEGMENT_BYTES` or defaults to 16 MiB.
    pub fn open(dir: &Path, start_lsn: Lsn) -> Result<Self> {
        let segment_size = std::env::var("UNIDB_WAL_SEGMENT_BYTES")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v > SEG_HDR)
            .unwrap_or(DEFAULT_SEGMENT_SIZE);
        Self::open_with_segment_size(dir, start_lsn, segment_size)
    }

    /// Like [`Wal::open`] with an explicit segment size — used by tests to force
    /// rotation with a small cap.
    pub fn open_with_segment_size(dir: &Path, start_lsn: Lsn, segment_size: u64) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let segment_size = segment_size.max(SEG_HDR + 1);
        let next_lsn = if start_lsn == INVALID_LSN {
            1
        } else {
            start_lsn + 1
        };

        let segments = list_segments(dir)?;
        // Seed the WAL-size counter from every existing segment (P1.e).
        let mut wal_bytes = 0u64;
        for (_, path) in &segments {
            wal_bytes += std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        }

        let (writer, active_seg, active_base_lsn, active_bytes) = match segments.last() {
            Some((idx, path)) => {
                // Reopen the highest-numbered (active) segment in append mode.
                let file = OpenOptions::new().append(true).open(path)?;
                let bytes = file.metadata().map(|m| m.len()).unwrap_or(SEG_HDR);
                let base = read_segment_base_lsn(path)?;
                (BufWriter::new(file), *idx, base, bytes)
            }
            None => {
                // Brand-new WAL: create segment 1 with base_lsn = next_lsn.
                let writer = create_segment(dir, 1, next_lsn)?;
                wal_bytes = SEG_HDR;
                (writer, 1, next_lsn, SEG_HDR)
            }
        };

        tracing::info!(dir = %dir.display(), next_lsn, active_seg, segment_size, "WAL opened");
        let (sealer, sealer_state_for_inner) = Sealer::start();
        Ok(Self {
            inner: Mutex::new(WalInner {
                writer,
                dir: dir.to_path_buf(),
                active_seg,
                active_base_lsn,
                active_bytes,
                segment_size,
                next_lsn,
                next_mini_txn: 1,
                wal_bytes,
                total_bytes_appended: 0,
                durable_lsn: INVALID_LSN,
                deferred_sync: false,
                poisoned: false,
                fsync_fault_armed: false,
                sealer_state: sealer_state_for_inner,
            }),
            flush_lock: Mutex::new(()),
            fsyncs: AtomicU64::new(0),
            fsync_latency: AtomicHistogram::new(),
            sealer,
        })
    }

    /// Durable-fsync count + latency snapshot (item 21). `count` is fsyncs that
    /// actually hit the platter (not coalesced followers), so a healthy
    /// group-commit workload shows far fewer fsyncs than commits.
    pub fn fsync_stats(&self) -> (u64, HistogramSnapshot) {
        (
            self.fsyncs.load(Ordering::Relaxed),
            self.fsync_latency.snapshot(),
        )
    }

    /// Record one durable fsync's wall-clock cost (item 21). Called only on the
    /// success path of an actual `sync_all`; lock-free.
    #[inline]
    fn note_fsync(&self, elapsed: std::time::Duration) {
        self.fsyncs.fetch_add(1, Ordering::Relaxed);
        self.fsync_latency.record(elapsed.as_micros() as u64);
    }

    /// Number of segment files currently on disk (observability + tests).
    pub fn segment_count(&self) -> Result<usize> {
        let dir = self.lock().dir.clone();
        Ok(list_segments(&dir)?.len())
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

    /// Cumulative framed bytes ever appended, across all segments and surviving
    /// checkpoint truncation (C1, measurement-only). Diff two readings to get
    /// the WAL volume produced by a window of operations.
    pub fn total_bytes_appended(&self) -> u64 {
        self.lock().total_bytes_appended
    }

    /// Number of mini-transactions opened so far (measurement / testing only).
    /// Starts at 0 immediately after `open`; increments by 1 per
    /// `begin_mini_txn` call.  Used by item-98 tests to assert that a multi-row
    /// VALUES INSERT collapses to ≤ one mini-txn per heap page.
    #[cfg(test)]
    pub fn mini_txn_count(&self) -> u64 {
        // `next_mini_txn` begins at 1 and is incremented before use, so the
        // number of txns opened is `next_mini_txn - 1`.
        self.lock().next_mini_txn.saturating_sub(1)
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

    /// Log a logical B-tree leaf insert (item 56, Step 4 — `WAL_INDEX_INSERT`).
    /// Replaces a full `WAL_INDEX` page image on the non-split single-insert
    /// path. `slot` is the insertion position within the leaf's entry array;
    /// `key_bytes` is the B-tree key encoded with [`btree_index::encode_key`];
    /// `rid_page_id`/`rid_slot` are the heap `RowId`. Redo-only — no undo
    /// payload. The caller must have already called `maybe_log_fpi` for
    /// `page_id` within this checkpoint interval.
    #[allow(clippy::too_many_arguments)]
    pub fn log_index_insert(
        &self,
        txn_id: u64,
        prev_lsn: Lsn,
        page_id: PageId,
        slot: u16,
        key_bytes: &[u8],
        rid_page_id: PageId,
        rid_slot: u16,
    ) -> Result<Lsn> {
        let mut redo = Vec::with_capacity(2 + key_bytes.len() + 6);
        redo.extend_from_slice(&u16_to_le(key_bytes.len() as u16));
        redo.extend_from_slice(key_bytes);
        redo.extend_from_slice(&u32_to_le(rid_page_id));
        redo.extend_from_slice(&u16_to_le(rid_slot));
        let mut inner = self.lock();
        let lsn = append_locked(
            &mut inner,
            txn_id,
            prev_lsn,
            WAL_INDEX_INSERT,
            page_id,
            slot,
            &redo,
            &[],
        )?;
        tracing::trace!(mini_txn_id = txn_id, lsn, page_id, slot, "WAL INDEX_INSERT");
        Ok(lsn)
    }

    /// Log a batched xmax-stamp for all `slots` on `page_id` (item 56, Step 3).
    /// Replaces N `log_update` calls with one record, amortising the per-record
    /// WAL header overhead over the whole page group.
    ///
    /// redo: `xid (8 B) || n_slots (2 B) || slot_0 (2 B) || ...`
    /// undo: `n_slots (2 B) || slot_0 (2 B) || ...`  (old xmax provably 0)
    ///
    /// `slot` in the fixed header is set to `u16::MAX` (batch, no single slot).
    pub fn log_xmax_batch(
        &self,
        txn_id: u64,
        prev_lsn: Lsn,
        page_id: PageId,
        xid: Xid,
        slots: &[u16],
    ) -> Result<Lsn> {
        debug_assert!(!slots.is_empty());
        let n = slots.len();
        let mut redo = Vec::with_capacity(10 + 2 * n);
        redo.extend_from_slice(&u64_to_le(xid));
        redo.extend_from_slice(&u16_to_le(n as u16));
        for &s in slots {
            redo.extend_from_slice(&u16_to_le(s));
        }
        let mut undo = Vec::with_capacity(2 + 2 * n);
        undo.extend_from_slice(&u16_to_le(n as u16));
        for &s in slots {
            undo.extend_from_slice(&u16_to_le(s));
        }
        let mut inner = self.lock();
        let lsn = append_locked(
            &mut inner,
            txn_id,
            prev_lsn,
            WAL_XMAX_BATCH,
            page_id,
            u16::MAX,
            &redo,
            &undo,
        )?;
        tracing::trace!(
            mini_txn_id = txn_id,
            lsn,
            page_id,
            n_slots = n,
            "WAL XMAX_BATCH"
        );
        Ok(lsn)
    }

    /// Log an atomic HOT update (item 58 — same-page xmax-stamp + HOT forwarding
    /// pointer + new-version insert). `old_slot` is the chain-head slot (where
    /// the B-tree still points). `new_slot` is the new version's slot on the
    /// SAME page. `insert_redo` is the standard WAL_INSERT redo payload for the
    /// new version (i.e. `encode_insert_redo(xid, prev, payload)`).
    ///
    /// WAL header `slot` = `old_slot` (the chain head).
    ///
    /// redo payload: xid (8 B LE) || old_slot (2 B LE) || new_slot (2 B LE)
    ///               || insert_redo (variable)
    ///
    /// undo payload: old_slot (2 B LE) || new_slot (2 B LE)
    ///   Undo: (1) delete new slot; (2) clear hot_next on old slot; (3) clear xmax on old slot.
    #[allow(clippy::too_many_arguments)]
    pub fn log_hot_update(
        &self,
        txn_id: u64,
        prev_lsn: Lsn,
        page_id: PageId,
        xid: Xid,
        old_slot: u16,
        new_slot: u16,
        insert_redo: &[u8],
    ) -> Result<Lsn> {
        // redo: xid || old_slot || new_slot || insert_redo
        let mut redo = Vec::with_capacity(12 + insert_redo.len());
        redo.extend_from_slice(&u64_to_le(xid));
        redo.extend_from_slice(&u16_to_le(old_slot));
        redo.extend_from_slice(&u16_to_le(new_slot));
        redo.extend_from_slice(insert_redo);
        // undo: old_slot || new_slot
        let mut undo = Vec::with_capacity(4);
        undo.extend_from_slice(&u16_to_le(old_slot));
        undo.extend_from_slice(&u16_to_le(new_slot));
        let mut inner = self.lock();
        let lsn = append_locked(
            &mut inner,
            txn_id,
            prev_lsn,
            WAL_HOT_UPDATE,
            page_id,
            old_slot,
            &redo,
            &undo,
        )?;
        tracing::trace!(
            mini_txn_id = txn_id,
            lsn,
            page_id,
            old_slot,
            new_slot,
            "WAL HOT_UPDATE"
        );
        Ok(lsn)
    }

    /// WAL_HOT_XPAGE_HEAD (item 71): the old-page portion of a cross-page HOT
    /// update — xmax stamp + cross-page forwarding pointer on `old_slot`.
    ///
    /// The new version insert on `new_page_id` is logged separately via
    /// `log_insert` in the same mini-txn.
    ///
    /// redo payload (16 B): xid (8 B LE) || old_slot (2 B LE) ||
    ///   new_page_id (4 B LE) || new_slot (2 B LE)
    /// undo payload (8 B):  old_slot (2 B LE) || saved_prev_page (4 B LE) ||
    ///   saved_prev_slot (2 B LE)
    #[allow(clippy::too_many_arguments)]
    pub fn log_hot_xpage_head(
        &self,
        txn_id: u64,
        prev_lsn: Lsn,
        old_page_id: PageId,
        xid: Xid,
        old_slot: u16,
        new_page_id: PageId,
        new_slot: u16,
        saved_prev_page: PageId,
        saved_prev_slot: u16,
    ) -> Result<Lsn> {
        // redo: xid || old_slot || new_page_id || new_slot
        let mut redo = Vec::with_capacity(16);
        redo.extend_from_slice(&u64_to_le(xid));
        redo.extend_from_slice(&u16_to_le(old_slot));
        redo.extend_from_slice(&u32_to_le(new_page_id));
        redo.extend_from_slice(&u16_to_le(new_slot));
        // undo: old_slot || saved_prev_page || saved_prev_slot
        let mut undo = Vec::with_capacity(8);
        undo.extend_from_slice(&u16_to_le(old_slot));
        undo.extend_from_slice(&u32_to_le(saved_prev_page));
        undo.extend_from_slice(&u16_to_le(saved_prev_slot));
        let mut inner = self.lock();
        let lsn = append_locked(
            &mut inner,
            txn_id,
            prev_lsn,
            WAL_HOT_XPAGE_HEAD,
            old_page_id,
            old_slot,
            &redo,
            &undo,
        )?;
        tracing::trace!(
            mini_txn_id = txn_id,
            lsn,
            old_page_id,
            old_slot,
            new_page_id,
            new_slot,
            "WAL HOT_XPAGE_HEAD"
        );
        Ok(lsn)
    }

    /// Item 79 — Phase B batch insert: one WAL record for N rows on the same
    /// fill page. Reduces WAL mutex acquisitions from O(rows) to O(fill_pages).
    ///
    /// `rows`: `(slot, insert_redo)` pairs in slot order.
    /// `insert_redo` is the standard `encode_insert_redo(xid, prev, payload)` blob.
    pub fn log_insert_batch(
        &self,
        txn_id: u64,
        prev_lsn: Lsn,
        page_id: PageId,
        xid: Xid,
        rows: &[(u16, Vec<u8>)],
    ) -> Result<Lsn> {
        debug_assert!(!rows.is_empty());
        let n = rows.len();
        // redo: xmin (8) || n_rows (2) || for each: slot (2) + redo_data
        let total_redo: usize = 10 + rows.iter().map(|(_, r)| 8 + r.len()).sum::<usize>();
        let mut redo = Vec::with_capacity(total_redo);
        redo.extend_from_slice(&u64_to_le(xid));
        redo.extend_from_slice(&u16_to_le(n as u16));
        for (slot, insert_redo) in rows {
            redo.extend_from_slice(&u16_to_le(*slot));
            redo.extend_from_slice(&u32_to_le(insert_redo.len() as u32));
            redo.extend_from_slice(insert_redo);
        }
        // undo: n_slots (2) || slot... (2 each)
        let mut undo = Vec::with_capacity(2 + 2 * n);
        undo.extend_from_slice(&u16_to_le(n as u16));
        for (slot, _) in rows {
            undo.extend_from_slice(&u16_to_le(*slot));
        }
        let mut inner = self.lock();
        let lsn = append_locked(
            &mut inner,
            txn_id,
            prev_lsn,
            WAL_INSERT_BATCH,
            page_id,
            u16::MAX,
            &redo,
            &undo,
        )?;
        tracing::trace!(
            mini_txn_id = txn_id,
            lsn,
            page_id,
            n_rows = n,
            "WAL INSERT_BATCH"
        );
        Ok(lsn)
    }

    /// Item 80 — Phase A batch cross-page HOT head: one WAL record for N
    /// (old_slot, new_page_id, new_slot) entries on the same old page.
    /// Reduces Phase A WAL mutex acquisitions from O(rows) to O(old_pages).
    ///
    /// `entries`: (old_slot, new_page_id, new_slot, saved_prev_page, saved_prev_slot).
    pub fn log_hot_xpage_batch(
        &self,
        txn_id: u64,
        prev_lsn: Lsn,
        old_page_id: PageId,
        xid: Xid,
        entries: &[(u16, PageId, u16, PageId, u16)],
    ) -> Result<Lsn> {
        debug_assert!(!entries.is_empty());
        let n = entries.len();
        // redo: xid (8) || n_entries (2) || for each: old_slot (2) + new_page_id (4) + new_slot (2)
        let mut redo = Vec::with_capacity(10 + 8 * n);
        redo.extend_from_slice(&u64_to_le(xid));
        redo.extend_from_slice(&u16_to_le(n as u16));
        for &(old_slot, new_page_id, new_slot, _, _) in entries {
            redo.extend_from_slice(&u16_to_le(old_slot));
            redo.extend_from_slice(&u32_to_le(new_page_id));
            redo.extend_from_slice(&u16_to_le(new_slot));
        }
        // undo: n_entries (2) || for each: old_slot (2) + saved_prev_page (4) + saved_prev_slot (2)
        let mut undo = Vec::with_capacity(2 + 8 * n);
        undo.extend_from_slice(&u16_to_le(n as u16));
        for &(old_slot, _, _, saved_prev_page, saved_prev_slot) in entries {
            undo.extend_from_slice(&u16_to_le(old_slot));
            undo.extend_from_slice(&u32_to_le(saved_prev_page));
            undo.extend_from_slice(&u16_to_le(saved_prev_slot));
        }
        let mut inner = self.lock();
        let lsn = append_locked(
            &mut inner,
            txn_id,
            prev_lsn,
            WAL_HOT_XPAGE_BATCH,
            old_page_id,
            u16::MAX,
            &redo,
            &undo,
        )?;
        tracing::trace!(
            mini_txn_id = txn_id,
            lsn,
            old_page_id,
            n_entries = n,
            "WAL HOT_XPAGE_BATCH"
        );
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

    /// Enable/disable group-commit deferral. In deferred mode (the engine
    /// default since C1 — commit-time fsync) statement mini-txn commit/abort
    /// records are appended without a per-call fsync; durability is forced by a
    /// later [`Self::sync`] / [`Self::sync_up_to`] (the transaction's commit
    /// point). When turning it **off**, callers should normally call
    /// [`Self::sync`] first to make anything appended-but-unsynced durable.
    pub fn set_deferred_sync(&self, deferred: bool) {
        self.lock().deferred_sync = deferred;
    }

    /// Force every record appended so far to durable storage and advance the
    /// durable frontier. In group-commit mode the caller invokes this once per
    /// drained batch, amortizing one fsync across every transaction that
    /// committed in that batch — the P5.b/M9 win.
    ///
    /// Item 89: also drain the background sealer before returning — any sealed
    /// segment that was handed off during rotation must be confirmed durable.
    pub fn sync(&self) -> Result<()> {
        // Wait for background sealer to complete any in-progress fsync first.
        // `wait_until_durable(INVALID_LSN)` is a no-op if the sealer has no
        // pending work; otherwise waits for the current segment to finish.
        // We use next_lsn - 1 to mean "everything appended so far".
        let drain_target = self.lock().next_lsn.saturating_sub(1);
        if drain_target > 0 {
            self.sealer.wait_until_durable(drain_target)?;
        }
        let start = Instant::now();
        let mut inner = self.lock();
        fsync_locked(&mut inner)?;
        drop(inner);
        // `fsync_locked` returns `Ok` only after a real `sync_all` reached the
        // platter (poison/fault return `Err`) — item 21 durability-cost panel.
        self.note_fsync(start.elapsed());
        Ok(())
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
    ///
    /// Item 89: after the active-segment fsync, also wait for the background
    /// sealer to finish any sealed (rotated-away) segment whose LSN range covers
    /// `target`. In the common case (no rotation) `wait_until_durable` returns
    /// immediately (no pending work, no-op).
    pub fn sync_up_to(&self, target: Lsn) -> Result<()> {
        if self.durable_lsn() >= target {
            // Also check sealer coverage — if the sealer has confirmed durability
            // for `target` via done_lsn, the fast path above already passes.
            return Ok(());
        }
        let _leader = self.flush_lock.lock().unwrap_or_else(|e| e.into_inner());
        if self.durable_lsn() >= target {
            return Ok(());
        }
        // If the target is in a segment that the sealer is currently fsyncing,
        // wait for that to complete before fsyncing the active segment.
        self.sealer.wait_until_durable(target)?;
        // Re-check: sealer may have made it durable.
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
        let fsync_start = Instant::now();
        if let Err(e) = file.sync_all() {
            let mut inner = self.lock();
            inner.poisoned = true;
            return Err(DbError::DurabilityFailure(format!("WAL fsync failed: {e}")));
        }
        // Item 21: one recorded fsync covers every commit it made durable, so
        // `commits / fsyncs` reads out the group-commit amortization directly.
        self.note_fsync(fsync_start.elapsed());
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

    /// Truncate the WAL up to (but not including) `keep_from_lsn` by deleting
    /// whole consumed segments (P6.a) — no file rewrite. A sealed segment is
    /// removable iff *every* record it holds has an LSN below `keep_from_lsn`,
    /// which holds exactly when the **next** segment's base LSN is
    /// `<= keep_from_lsn`. The active segment is never deleted. This is coarser
    /// than the old record-exact rewrite (a surviving segment may still carry a
    /// few pre-`keep_from_lsn` records), which is harmless: recovery filters by
    /// `lsn >= checkpoint_lsn` anyway, and the retained records are idempotent.
    pub fn truncate_before(&self, keep_from_lsn: Lsn) -> Result<()> {
        // Item 89: drain the background sealer before truncating — a segment
        // being sealed by the background thread must not be removed while its
        // fsync is in-flight.
        let drain_target = self.lock().next_lsn.saturating_sub(1);
        if drain_target > 0 {
            self.sealer.wait_until_durable(drain_target)?;
        }
        let mut inner = self.lock();
        inner.writer.flush()?;
        let dir = inner.dir.clone();
        let active_seg = inner.active_seg;
        let active_base_lsn = inner.active_base_lsn;

        let segments = list_segments(&dir)?;
        let mut removed = 0usize;
        for (i, (idx, path)) in segments.iter().enumerate() {
            if *idx == active_seg {
                continue; // never delete the segment we are appending to
            }
            // Base LSN of the segment *after* this one (or the active segment's
            // base if this is the last sealed segment).
            let next_base = match segments.get(i + 1) {
                Some((_, next_path)) => read_segment_base_lsn(next_path)?,
                None => active_base_lsn,
            };
            if next_base <= keep_from_lsn {
                std::fs::remove_file(path)?;
                removed += 1;
                tracing::info!(segment = *idx, "WAL segment removed (truncation)");
            }
        }

        // P1.e: recompute the WAL-size counter from the surviving segments.
        inner.wal_bytes = list_segments(&dir)?
            .iter()
            .map(|(_, p)| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0))
            .sum();
        tracing::info!(keep_from_lsn, removed_segments = removed, "WAL truncated");
        Ok(())
    }

    /// Scan every record in the WAL directory `dir`, across all segments, in LSN
    /// order (segments are numbered monotonically, so index order = LSN order).
    /// A missing directory or missing segments yield an empty log.
    pub fn scan_file(dir: &Path) -> Result<Vec<WalRecord>> {
        let mut records = Vec::new();
        for (_, path) in list_segments(dir)? {
            records.extend(scan_segment_file(&path)?);
        }
        Ok(records)
    }

    /// WAL shipping (P6.b): every record strictly after `from_lsn` **and at or
    /// below the durable frontier**, in LSN order — what a replica or archiver
    /// needs to catch up from its last-applied LSN.
    ///
    /// The durable-frontier cap is the commit-time-fsync replication guard (C3):
    /// under the group-committed default, records are appended to the segment
    /// file before their fsync, so the file on disk can contain records past
    /// `durable_lsn`. Shipping those would let a replica apply — and, on
    /// failover, a promoted replica *retain* — commits the primary had not yet
    /// made durable; a crash of the primary before its own fsync would then
    /// leave the replica **ahead** of the recovered primary (divergence). Capping
    /// at `durable_lsn` guarantees a replica's state is always a prefix of the
    /// primary's durable state. Records between `durable_lsn` and the WAL tail
    /// are simply shipped in a later batch once they become durable.
    ///
    /// v1 scans all segments then filters; skipping already-consumed segments by
    /// their base LSN is a future optimization.
    pub fn records_from(&self, from_lsn: Lsn) -> Result<Vec<WalRecord>> {
        let (dir, durable) = {
            let g = self.lock();
            (g.dir.clone(), g.durable_lsn)
        };
        let mut recs = Self::scan_file(&dir)?;
        recs.retain(|r| r.lsn > from_lsn && r.lsn <= durable);
        Ok(recs)
    }

    /// Serialize every record after `from_lsn` into a framed byte stream that a
    /// replica applies via [`decode_stream`] + redo (the P6.c consumer side).
    pub fn ship_from(&self, from_lsn: Lsn) -> Result<Vec<u8>> {
        Ok(encode_stream(&self.records_from(from_lsn)?))
    }

    /// Write shipped records **verbatim** (preserving each record's original
    /// LSN) into this WAL, then fsync (P6.c replica apply). Unlike
    /// [`append_locked`] the LSNs are the primary's, not self-allocated, so the
    /// replica's WAL mirrors the primary's and recovery replays it identically.
    /// Records are expected in ascending LSN order (as shipped). Duplicate/old
    /// records (`lsn < next_lsn`) are still written but harmless — recovery's
    /// redo is idempotent and LSN-gated. Advances `next_lsn` past the highest.
    pub fn write_shipped(&self, records: &[WalRecord]) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        let mut inner = self.lock();
        if inner.poisoned {
            return Err(DbError::DurabilityFailure(
                "WAL is poisoned by an earlier fsync failure; session is unrecoverable".into(),
            ));
        }
        for r in records {
            let encoded = encode_record(r);
            write_framed_locked(&mut inner, &encoded, r.lsn)?;
            if r.lsn + 1 > inner.next_lsn {
                inner.next_lsn = r.lsn + 1;
            }
        }
        fsync_locked(&mut inner)?;
        Ok(())
    }
}

impl Drop for Wal {
    /// Signal the background sealer thread to exit cleanly on WAL close.
    fn drop(&mut self) {
        self.sealer.shutdown();
    }
}

/// Frame a list of WAL records into a `[len:u32][record]...` byte stream (the
/// WAL-shipping wire format, P6.b) — the same framing used inside a segment.
pub fn encode_stream(records: &[WalRecord]) -> Vec<u8> {
    let mut out = Vec::new();
    for r in records {
        let encoded = encode_record(r);
        out.extend_from_slice(&u32_to_le(encoded.len() as u32));
        out.extend_from_slice(&encoded);
    }
    out
}

/// Decode a framed WAL-shipping byte stream back into records (P6.c consumer).
pub fn decode_stream(bytes: &[u8]) -> Result<Vec<WalRecord>> {
    let mut records = Vec::new();
    let mut pos = 0usize;
    while pos + 4 <= bytes.len() {
        let len = u32_from_le(bytes[pos..pos + 4].try_into().unwrap()) as usize;
        pos += 4;
        if pos + len > bytes.len() {
            return Err(DbError::WalCorrupt { lsn: 0 });
        }
        records.push(decode_record(&bytes[pos..pos + len])?);
        pos += len;
    }
    Ok(records)
}

/// Read every record from a single segment file, skipping its header. Stops at
/// the first short/corrupt record (a partially-written tail after a crash).
fn scan_segment_file(path: &Path) -> Result<Vec<WalRecord>> {
    let mut records = Vec::new();
    let mut f = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(records),
        Err(e) => return Err(e.into()),
    };
    // Skip and validate the segment header.
    let mut hdr = [0u8; SEG_HDR as usize];
    match f.read_exact(&mut hdr) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(records),
        Err(e) => return Err(e.into()),
    }
    if u32_from_le(hdr[0..4].try_into().unwrap()) != SEG_MAGIC {
        tracing::warn!(?path, "WAL scan: bad segment magic, skipping segment");
        return Ok(records);
    }
    f.seek(SeekFrom::Start(SEG_HDR))?;
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
    write_framed_locked(inner, &encoded, lsn)?;
    inner.next_lsn += 1;
    Ok(lsn)
}

/// Physically append one already-encoded record to the active segment, rotating
/// first if it would overflow (`base_lsn` is the LSN of the record being written
/// — the fresh segment's base). Shared by [`append_locked`] (self-allocated LSN)
/// and [`Wal::write_shipped`] (verbatim replica apply, P6.c).
fn write_framed_locked(inner: &mut WalInner, encoded: &[u8], base_lsn: Lsn) -> Result<()> {
    let framed = 4 + encoded.len() as u64;
    // P6.a: seal + rotate before this record if the active segment already holds
    // at least one record and would overflow. `active_bytes > SEG_HDR` guards
    // against rotating a header-only segment forever when a single record is
    // larger than the whole segment (it then lands whole in its own segment).
    if inner.active_bytes > SEG_HDR && inner.active_bytes + framed > inner.segment_size {
        rotate_segment(inner, base_lsn)?;
    }
    inner.writer.write_all(&u32_to_le(encoded.len() as u32))?;
    inner.writer.write_all(encoded)?;
    inner.active_bytes += framed;
    inner.wal_bytes += framed; // P1.e: track WAL size
    inner.total_bytes_appended += framed; // C1: cumulative, survives truncation
    Ok(())
}

/// Seal the active segment and rotate to a fresh one. The fsync of the sealed
/// segment is handed to the background [`Sealer`] thread (item 89) so the
/// append mutex is released before the slow sync_all runs. The durable frontier
/// is NOT advanced here — it advances when the sealer confirms durability via
/// `wait_until_durable`, or when `sync_up_to` / `sync` fsyncs the active segment.
fn rotate_segment(inner: &mut WalInner, new_base_lsn: Lsn) -> Result<()> {
    if inner.poisoned {
        return Err(DbError::DurabilityFailure(
            "WAL is poisoned by an earlier fsync failure; session is unrecoverable".into(),
        ));
    }
    // Flush the BufWriter to push all records into the OS page cache.
    if let Err(e) = inner.writer.flush() {
        inner.poisoned = true;
        return Err(DbError::DurabilityFailure(format!(
            "WAL segment seal flush failed: {e}"
        )));
    }
    // Clone the FD so the sealer thread can sync_all it independently.
    let sealed_file = match inner.writer.get_ref().try_clone() {
        Ok(f) => f,
        Err(e) => {
            inner.poisoned = true;
            return Err(DbError::DurabilityFailure(format!(
                "WAL segment FD clone for background seal failed: {e}"
            )));
        }
    };
    let sealed_last = inner.next_lsn - 1;
    // Open the next segment BEFORE handing the sealed one to the sealer, so
    // we continue appending immediately after this function returns.
    let next_idx = inner.active_seg + 1;
    inner.writer = create_segment(&inner.dir, next_idx, new_base_lsn)?;
    inner.active_seg = next_idx;
    inner.active_base_lsn = new_base_lsn;
    inner.active_bytes = SEG_HDR;
    // Submit the sealed segment FD to the background thread for fsync (item 89).
    // The append mutex will be released as soon as this function returns and
    // write_framed_locked resumes — the sealer's sync_all runs concurrently.
    // Wait if a prior segment is still being sealed (rare with 16 MiB segments).
    {
        let (mtx, cv) = &*inner.sealer_state;
        let mut st = mtx.lock().unwrap_or_else(|e| e.into_inner());
        while st.pending.is_some() {
            st = cv.wait(st).unwrap_or_else(|e| e.into_inner());
        }
        st.pending = Some((sealed_file, sealed_last));
        cv.notify_all();
    }
    tracing::info!(
        segment = next_idx,
        base_lsn = new_base_lsn,
        sealed_lsn = sealed_last,
        "WAL rotated to new segment (background seal submitted)"
    );
    Ok(())
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
        // corrupt the last bytes of the active segment file
        let seg = segment_path(&p, 1);
        let mut bytes = std::fs::read(&seg).unwrap();
        let n = bytes.len();
        bytes[n - 5] ^= 0xff;
        std::fs::write(&seg, &bytes).unwrap();
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
