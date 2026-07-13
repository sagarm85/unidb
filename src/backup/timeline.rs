// Time-based PITR timeline index (item 28, R1).
//
// Records one (ts_micros, lsn) mark per committed user transaction so that
// `restore_to_time` can resolve a wall-clock target to the highest LSN at or
// before it — without touching the WAL record format (no FORMAT_VERSION bump,
// no §3/D9 sign-off). Time is advisory; LSN is authoritative.
//
// **On-disk format:** a flat binary file (`timeline.bin`) of 16-byte records:
//
//   [0..8]  ts_micros   u64 LE   — Unix epoch microseconds at commit time
//   [8..16] lsn         u64 LE   — WAL LSN of the committed WAL_TXN_COMMIT
//
// Records are appended after each user-transaction commit (after WAL sync).
// A partial last record (< 16 bytes, from a crash mid-append) is silently
// skipped on load — this degrades PITR resolution to the previous valid mark,
// not database consistency (the WAL is still the source of truth).
//
// **Clock-skew handling:** `resolve` picks `max(lsn)` where `mark.ts ≤ target`.
// Since LSN is monotonic, this is correct even when wall-clock timestamps are
// non-monotonic (NTP step-back, VM migration). Marks are written in commit
// order (LSN order), so the common case is a sorted array and the linear scan
// terminates quickly.

use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::Path,
    sync::Mutex,
};

use crate::{error::Result, format::Lsn};

pub const TIMELINE_FILE: &str = "timeline.bin";

/// One (timestamp, LSN) mark in the timeline index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimelineMark {
    /// Unix epoch microseconds at commit time.
    pub ts_micros: u64,
    /// WAL LSN of the WAL_TXN_COMMIT record.
    pub lsn: Lsn,
}

impl TimelineMark {
    pub fn to_bytes(self) -> [u8; 16] {
        let mut b = [0u8; 16];
        b[0..8].copy_from_slice(&self.ts_micros.to_le_bytes());
        b[8..16].copy_from_slice(&self.lsn.to_le_bytes());
        b
    }

    fn from_bytes(b: &[u8; 16]) -> Self {
        Self {
            ts_micros: u64::from_le_bytes(b[0..8].try_into().unwrap()),
            lsn: u64::from_le_bytes(b[8..16].try_into().unwrap()),
        }
    }
}

/// Append-only timeline index. One instance lives on the `Engine` and records
/// a mark after every user-transaction commit. Interior-mutable so `record`
/// takes `&self` (same pattern as `Wal`, `SlotRegistry`, etc.).
pub struct TimelineIndex {
    file: Mutex<File>,
}

impl TimelineIndex {
    /// Open (or create) the timeline file in `dir`. Non-fatal if the existing
    /// file has a partial last record — `load_from` handles that on restore.
    pub fn open(dir: &Path) -> Result<Self> {
        let path = dir.join(TIMELINE_FILE);
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(false)
            .open(&path)?;
        Ok(Self {
            file: Mutex::new(file),
        })
    }

    /// Append a mark. Called from `Engine::commit` after WAL sync, so `lsn`
    /// is already durable. A write failure is logged but never propagates —
    /// timeline marks are advisory; they must not block a commit.
    pub fn record(&self, ts_micros: u64, lsn: Lsn) {
        let bytes = TimelineMark { ts_micros, lsn }.to_bytes();
        if let Ok(mut f) = self.file.lock() {
            if let Err(e) = f.write_all(&bytes) {
                tracing::warn!(error = %e, lsn, "timeline mark write failed (advisory; commit unaffected)");
            }
        }
    }

    /// Load all valid marks from the timeline file at `path`. Partial last
    /// records (torn by a crash) are silently skipped.
    pub fn load_from(path: &Path) -> Vec<TimelineMark> {
        let mut marks = Vec::new();
        let Ok(mut f) = File::open(path) else {
            return marks;
        };
        let mut buf = Vec::new();
        if f.read_to_end(&mut buf).is_err() {
            return marks;
        }
        // chunks_exact skips any sub-16-byte tail automatically.
        for chunk in buf.chunks_exact(16) {
            marks.push(TimelineMark::from_bytes(chunk.try_into().unwrap()));
        }
        marks
    }

    /// Resolve `target_ts_micros` to the highest LSN where `mark.ts ≤ target`.
    /// Returns `None` when no marks exist at or before the target time.
    ///
    /// Does not assume mark timestamps are monotonic (handles NTP skew / VM
    /// migration). Scans all marks and picks `max(lsn)` among eligible ones.
    pub fn resolve(marks: &[TimelineMark], target_ts_micros: u64) -> Option<Lsn> {
        marks
            .iter()
            .filter(|m| m.ts_micros <= target_ts_micros)
            .max_by_key(|m| m.lsn)
            .map(|m| m.lsn)
    }
}

/// Current Unix epoch in microseconds. Used by `Engine::commit` to stamp marks.
pub fn now_micros() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn roundtrip_mark_bytes() {
        let m = TimelineMark {
            ts_micros: 1_700_000_000_000_000,
            lsn: 42,
        };
        assert_eq!(TimelineMark::from_bytes(&m.to_bytes()), m);
    }

    #[test]
    fn resolve_picks_highest_lsn_at_or_before_target() {
        let marks = vec![
            TimelineMark {
                ts_micros: 1000,
                lsn: 10,
            },
            TimelineMark {
                ts_micros: 2000,
                lsn: 20,
            },
            TimelineMark {
                ts_micros: 3000,
                lsn: 30,
            },
        ];
        // Exact match on ts 2000 → lsn 20.
        assert_eq!(TimelineIndex::resolve(&marks, 2000), Some(20));
        // Between marks → most recent eligible lsn.
        assert_eq!(TimelineIndex::resolve(&marks, 2500), Some(20));
        // After all marks → lsn 30.
        assert_eq!(TimelineIndex::resolve(&marks, 9999), Some(30));
        // Before any mark → None.
        assert_eq!(TimelineIndex::resolve(&marks, 500), None);
    }

    #[test]
    fn resolve_handles_non_monotonic_timestamps() {
        // Simulate clock skew: mark 2 has a ts earlier than mark 1.
        let marks = vec![
            TimelineMark {
                ts_micros: 2000,
                lsn: 10,
            },
            TimelineMark {
                ts_micros: 1800,
                lsn: 20,
            }, // clock stepped back
            TimelineMark {
                ts_micros: 3000,
                lsn: 30,
            },
        ];
        // At ts=2000: eligible marks = {lsn:10, lsn:20} → max lsn = 20.
        assert_eq!(TimelineIndex::resolve(&marks, 2000), Some(20));
    }

    #[test]
    fn load_from_skips_torn_last_record() {
        use std::io::Write as _;
        let dir = tempdir().unwrap();
        let path = dir.path().join(TIMELINE_FILE);
        let m1 = TimelineMark {
            ts_micros: 1000,
            lsn: 10,
        };
        let m2 = TimelineMark {
            ts_micros: 2000,
            lsn: 20,
        };
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(&m1.to_bytes()).unwrap();
        f.write_all(&m2.to_bytes()).unwrap();
        f.write_all(&[0u8; 7]).unwrap(); // torn: 7 bytes of a 16-byte mark
        drop(f);

        let marks = TimelineIndex::load_from(&path);
        assert_eq!(marks.len(), 2, "torn mark must be silently skipped");
        assert_eq!(marks[0], m1);
        assert_eq!(marks[1], m2);
    }

    #[test]
    fn load_from_returns_empty_when_no_file() {
        let dir = tempdir().unwrap();
        let marks = TimelineIndex::load_from(&dir.path().join(TIMELINE_FILE));
        assert!(marks.is_empty());
    }

    #[test]
    fn open_and_record_persist_across_reload() {
        let dir = tempdir().unwrap();
        {
            let idx = TimelineIndex::open(dir.path()).unwrap();
            idx.record(1000, 10);
            idx.record(2000, 20);
        }
        let path = dir.path().join(TIMELINE_FILE);
        let marks = TimelineIndex::load_from(&path);
        assert_eq!(marks.len(), 2);
        assert_eq!(marks[0].lsn, 10);
        assert_eq!(marks[1].lsn, 20);
    }
}
