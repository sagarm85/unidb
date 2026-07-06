// On-disk format constants, magic numbers, version, and endian helpers.
// All on-disk integers are little-endian (D9).

pub const MAGIC: u32 = 0x556E4442; // "UnDB"

// v2 -> v3: control file gained `next_xid` (8 bytes), persisted at every
// checkpoint alongside checkpoint_lsn/wal_tail_lsn. Fixes a real bug found
// during M5 manual testing: `TransactionManager::recover_next_xid` derives
// the resumed xid purely from WAL_TXN_BEGIN records still present in the
// WAL, but `checkpoint::run` truncates every record before the checkpoint
// LSN — which, in ordinary use, is *every* prior transaction's begin
// record, since a checkpoint only runs after they've all committed.
// Without this fix, committing transactions, checkpointing, then reopening
// resets the xid counter to 1, silently reissuing already-used xids (MVCC
// visibility corruption). No migration path — no prior version of this
// database has shipped externally (same precedent as v1->v2's tuple-header
// change, M1.a).
pub const FORMAT_VERSION: u16 = 3;

/// Default page size: 8 KiB (D8). Baked into the control file at DB init.
pub const DEFAULT_PAGE_SIZE: u32 = 8192;

/// Maximum supported page size (64 KiB).
pub const MAX_PAGE_SIZE: u32 = 65536;

/// Minimum supported page size (4 KiB).
pub const MIN_PAGE_SIZE: u32 = 4096;

/// Log Sequence Number — monotonically increasing u64, 0 means "no WAL record".
pub type Lsn = u64;

/// Page identifier — 0-based index into the page file.
pub type PageId = u32;

/// Transaction identifier — monotonically increasing u64, 0 means "no transaction" (M1).
pub type Xid = u64;

pub const INVALID_LSN: Lsn = 0;
pub const INVALID_PAGE_ID: PageId = u32::MAX;
pub const INVALID_XID: Xid = 0;

// Page type tags stored in PageHeader.page_type.
pub const PAGE_TYPE_HEAP: u8 = 1;
pub const PAGE_TYPE_FREE: u8 = 2;
pub const PAGE_TYPE_META: u8 = 3;

// WAL record type tags.
pub const WAL_BEGIN: u8 = 1;
pub const WAL_COMMIT: u8 = 2;
pub const WAL_ABORT: u8 = 3;
pub const WAL_INSERT: u8 = 4;
pub const WAL_UPDATE: u8 = 5;
pub const WAL_DELETE: u8 = 6;
pub const WAL_CHECKPOINT: u8 = 7;

// WAL user-transaction record types (M1). Distinct from the mini-txn
// WAL_BEGIN/COMMIT/ABORT tags above: a mini-txn is D2's per-statement atomic
// unit, a user-txn is M1's multi-statement unit. Both share the WalRecord
// wire format — the `mini_txn_id` field doubles as the xid for these tags,
// so no format change is needed, just a second independent ID space.
pub const WAL_TXN_BEGIN: u8 = 8;
pub const WAL_TXN_COMMIT: u8 = 9;
pub const WAL_TXN_ABORT: u8 = 10;

// ── little-endian helpers ────────────────────────────────────────────────────

#[inline]
pub fn u16_to_le(v: u16) -> [u8; 2] {
    v.to_le_bytes()
}

#[inline]
pub fn u32_to_le(v: u32) -> [u8; 4] {
    v.to_le_bytes()
}

#[inline]
pub fn u64_to_le(v: u64) -> [u8; 8] {
    v.to_le_bytes()
}

#[inline]
pub fn u16_from_le(b: [u8; 2]) -> u16 {
    u16::from_le_bytes(b)
}

#[inline]
pub fn u32_from_le(b: [u8; 4]) -> u32 {
    u32::from_le_bytes(b)
}

#[inline]
pub fn u64_from_le(b: [u8; 8]) -> u64 {
    u64::from_le_bytes(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magic_round_trip() {
        let b = u32_to_le(MAGIC);
        assert_eq!(u32_from_le(b), MAGIC);
    }

    #[test]
    fn page_size_valid() {
        const { assert!(DEFAULT_PAGE_SIZE >= MIN_PAGE_SIZE) };
        const { assert!(DEFAULT_PAGE_SIZE <= MAX_PAGE_SIZE) };
        assert!(DEFAULT_PAGE_SIZE.is_power_of_two());
    }
}
