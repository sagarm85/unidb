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
//
// v3 -> v4 (P1.a): a new WAL record kind, `WAL_FPI` (full-page image), was
// added for torn-page protection (full_page_writes). An 8 KiB page write is
// not atomic; a crash mid-write leaves a half-old/half-new page that CRC
// detects but cannot repair. On the first modification of a page after each
// checkpoint the buffer pool now logs the whole clean page image to the WAL;
// recovery replays it as the clean base before the incremental redo records
// on top, so a torn on-disk page is fully reconstructed. Old WALs never
// contain the record, so a version bump (D9) keeps a pre-P1.a database from
// being read by a build that would not know to look for it. No migration path
// — no prior version of this database has shipped externally.
//
// v4 -> v5 (P3.a): the B-Tree secondary index became durable — nodes are pages
// in the page store, WAL-logged as full node-page images via a new record kind
// `WAL_INDEX` (redo-only), and crash-recovered instead of rebuilt on open. A
// new page type `PAGE_TYPE_BTREE` tags those node/meta pages. Old WALs never
// contain `WAL_INDEX`, and a pre-P3.a database has no durable index pages nor
// the per-column `index_root` catalog pointer, so a version bump (D9) keeps an
// older build from misreading them. No migration path — no prior version has
// shipped externally.
pub const FORMAT_VERSION: u16 = 5;

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
/// Durable B-Tree node/meta pages (P3.a). Distinguishes index-structure pages
/// from heap pages so a future integrity checker can tell them apart.
pub const PAGE_TYPE_BTREE: u8 = 4;

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

// WAL vacuum record (M10). Redo-only, idempotent: freeing already-dead,
// already-committed space is a no-op if replayed on recovery, so it carries
// no undo payload (unlike WAL_UPDATE/WAL_DELETE). Used two ways, distinguished
// by the record's `slot`:
//   - slot != u16::MAX : mark that one line-pointer DEAD (M10.b — tuple body
//     dropped, pointer retained, not yet reusable). No redo payload.
//   - slot == u16::MAX : the redo payload is a full compacted page image
//     (M10.d — intra-page compaction + DEAD→UNUSED promotion); redo restores
//     the exact page bytes. Idempotent via the page LSN check in recovery.
pub const WAL_VACUUM: u8 = 11;

// WAL full-page image (P1.a — torn-page protection / full_page_writes).
// Redo-only, no undo payload: the redo payload is the entire clean page image
// (`page_size` bytes) captured on the first modification of that page after a
// checkpoint, and `slot` is `u16::MAX` (a whole-page record carries no slot).
// Recovery redo overwrites the on-disk page (which may be torn) with this
// image as the clean base, then replays the interval's subsequent incremental
// redo records (higher LSN) on top. Logged *before* the first incremental
// change record for the page, within the same mini-txn, so it is redone only
// when that mini-txn committed — which is exactly when the page could have
// reached disk torn (D5 forbids flushing a page whose WAL is not yet durable).
pub const WAL_FPI: u8 = 12;

// WAL durable-index record (P3.a — durable B-Tree). Redo-only, idempotent: the
// redo payload is a full B-Tree node/meta page image (`page_size` bytes) and
// `slot` is `u16::MAX` (a whole-page record). Recovery overwrites the on-disk
// page with the image, stamped with this record's LSN, exactly like a `WAL_FPI`
// base image — last-writer-in-LSN-order wins, and index pages never overlap
// heap pages, so no LSN gate is needed. Every index mutation brackets all of
// its node writes (a leaf write, or a split chain + meta-page repoint) in one
// mini-transaction, so recovery redoes all pages of a committed index mutation
// or none. There is no undo: a secondary-index entry is a hint re-validated
// against MVCC visibility, so a stale/extra entry is harmless and a missing one
// is prevented by the index mini-txn fsyncing before the user txn commits.
pub const WAL_INDEX: u8 = 13;

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
