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
//
// v5 -> v6 (item 56 Step 3): a new WAL record kind `WAL_XMAX_BATCH` (type 14)
// batches multiple xmax-stamp operations on the same heap page into a single
// log record. Recovery for this record type is not present in older builds —
// the recovery `_ => {}` catch-all silently skips unknown types, leaving dead
// rows visible after crash recovery. The version bump causes older builds to
// produce a hard `BadVersion` error rather than silently misrecovering. No
// migration path — no prior version has shipped externally.
//
// v6 -> v7 (item 56 Step 4): WAL_INDEX_INSERT (type 15) replaces full-page
// WAL_INDEX for single-row non-split leaf inserts. Torn-page safety is
// provided by a WAL_FPI for the index leaf before the first logical record in
// each checkpoint interval. Redo re-executes the key+RowId insert at the
// recorded leaf slot (LSN-gated, idempotent). No undo arm — stale index
// entries from aborted/incomplete inserts are filtered by heap visibility and
// scrubbed by vacuum, matching existing behaviour for WAL_INDEX. An older
// binary's recovery `_ => {}` catch-all would silently skip WAL_INDEX_INSERT,
// leaving the index entry missing and the committed row unfindable — incorrect,
// not a safe no-op. The version bump causes older builds to produce BadVersion
// rather than silently misrecovering. No migration path — no prior version has
// shipped externally.
//
// v7 -> v8 (item 58 — HOT-equivalent UPDATE, D4 sign-off 2026-07-17):
// The `_pad u16` field at tuple-header offset [22..24] is repurposed as
// `hot_next: u16`, a forwarding pointer from an old (xmax-stamped) tuple
// version to a newer version on the SAME page. `HOT_TUPLE_FLAG` (bit 0 of
// `flags`, a previously-unused byte inserted between xmax and prev_page) signals
// that the slot is a HOT chain head — callers must follow `hot_next` to find
// the current version rather than updating the B-tree entry. TUPLE_HEADER_SIZE
// (24 bytes) is UNCHANGED: the flag byte and hot_next fit inside the existing
// 24-byte budget (flags byte was implicit zero-padding before). A new WAL record
// type `WAL_HOT_UPDATE` (type 16) carries the atomic (xmax-stamp old slot +
// set HOT_FLAG/hot_next + insert new slot) operation under one mini-txn. An
// older binary's recovery `_ => {}` catch-all would silently skip WAL_HOT_UPDATE,
// leaving the HOT chain broken after crash recovery (B-tree points to xmax'd old
// slot with no forward pointer → row unfindable). The version bump causes older
// builds to produce BadVersion rather than silently misrecovering. No migration
// path — no prior version has shipped externally.
//
// v8 -> v9 (item 71 — cross-page HOT chains, 2026-07-18):
// A new WAL record type `WAL_HOT_XPAGE_HEAD` (type 17) covers the old-page
// portion of a cross-page HOT update: (a) xmax-stamp the old slot and (b) set
// `hot_next = HOT_NEXT_XPAGE (0xFFFE)` plus overwrite the old slot's
// `prev_page`/`prev_slot` fields (which are safe to repurpose on a superseded
// version — those backward-chain fields are never read during forward scans)
// with the cross-page target `(new_page_id, new_slot)`. The new version's
// insert on `new_page_id` is logged via the existing `WAL_INSERT` in the same
// mini-txn. TUPLE_HEADER_SIZE (24 bytes) is UNCHANGED — no field grows; only
// the semantics of prev_page/prev_slot on a superseded-with-XPAGE slot changes.
// An older binary's `_ => {}` catch-all would silently skip WAL_HOT_XPAGE_HEAD,
// leaving the cross-page chain broken after crash recovery (B-tree points to
// xmax'd old slot with hot_next=XPAGE but no restored chain → row unfindable).
// The version bump causes older builds to produce BadVersion. No migration path.
//
// v10 → v11 (item 97): `TableDef` gains `row_count: i64` in the catalog JSON
// blob. No WAL format change. `#[serde(default)]` means old blobs deserialise
// with `row_count = 0` (safe — treated as stale, recalibrated on next DML).
// The version bump rejects v10 opens so a stale `row_count` is never surfaced
// as a correct exact count to a new binary.
pub const FORMAT_VERSION: u16 = 11;

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

// WAL batched xmax-stamp record (item 56, Step 3). One record per (page,
// mini-txn) replaces N individual WAL_UPDATE records when delete_many (or
// update_many Phase A) stamps multiple rows on the same page. `slot` is
// `u16::MAX` (batch record; no single slot). FORMAT_VERSION bumped to 6 so
// older builds reject this WAL with BadVersion rather than silently misrecovering
// (recovery's `_ => {}` catch-all would skip unknown types, leaving dead rows
// visible — incorrect behaviour, not a safe no-op).
//
// redo payload: xid (8 bytes LE) || n_slots (2 bytes LE) || slot_0 (2 bytes LE) || ...
//   Redo: stamp xmax = xid on every listed slot (LSN-gated, idempotent).
//
// undo payload: n_slots (2 bytes LE) || slot_0 (2 bytes LE) || ...
//   Undo: reset xmax = 0 on every listed slot. The old xmax is provably 0 for
//   every row in the batch (conflict check rejects rows with xmax != 0 before
//   any stamp), so the per-slot old-xmax payload of WAL_UPDATE is dead weight.
pub const WAL_XMAX_BATCH: u8 = 14;

// WAL logical B-tree leaf insert (item 56, Step 4). Redo-only, LSN-gated,
// idempotent. Replaces WAL_INDEX (full page image) for single-row non-split
// leaf inserts; splits, insert_many, and patch_many keep WAL_INDEX.
//
// WAL header fields used: `page_id` = leaf page; `slot` = insertion position
// within the leaf's entry array (the index into `entries` after insertion).
//
// redo payload: key_len (2 B LE) || key_bytes (key_len B) || rid_page (4 B LE) || rid_slot (2 B LE)
//   Redo: deserialize the leaf, insert (key, RowId) at position `slot`,
//   re-serialize, stamp LSN. LSN-gated: skip if page.lsn() >= record.lsn.
//   A WAL_FPI for the same leaf page precedes the first WAL_INDEX_INSERT in
//   each checkpoint interval (torn-page safety, same as heap pages).
//
// undo payload: empty — no undo arm. Index entries for aborted/incomplete
//   inserts are tolerated: the heap MVCC visibility check on every index
//   lookup filters them, and vacuum scrubs them (same behaviour as WAL_INDEX).
pub const WAL_INDEX_INSERT: u8 = 15;

// WAL HOT update record (item 58 — HOT-equivalent UPDATE, D4 sign-off).
// One record covers the entire HOT operation on a single page atomically:
// (a) stamp xmax = xid on the old slot, (b) set HOT_TUPLE_FLAG + hot_next
// in the old slot's tuple header, (c) insert the new version at new_slot.
//
// WAL header fields: `page_id` = the shared page; `slot` = old_slot (the
// chain head, where the B-tree still points).
//
// redo payload: xid (8 B LE) || old_slot (2 B LE) || new_slot (2 B LE)
//   || insert_redo (variable — same layout as WAL_INSERT redo, i.e.
//   xmin:8 || prev_page:4 || prev_slot:2 || payload)
//   Redo: LSN-gated. Apply xmax=xid on old_slot; set HOT_TUPLE_FLAG + hot_next
//   on old_slot; insert_versioned the new payload at new_slot (idempotent —
//   slot-count guard prevents double-application).
//
// undo payload: old_slot (2 B LE) || new_slot (2 B LE)
//   Undo: zero the new slot (Page::delete); clear HOT_TUPLE_FLAG and hot_next
//   in old slot; clear xmax = 0 on old slot. Order: new-slot first, then old
//   slot — this is a two-phase undo, intentional (see crash-test P59b).
//
// No B-tree update is emitted when a HOT update succeeds — the B-tree entry
// stays pointing at old_slot (the chain head). Readers following a B-tree
// candidate check HOT_TUPLE_FLAG and follow hot_next if set.
//
// An older binary's `_ => {}` catch-all would skip this record, leaving the
// HOT chain broken after crash recovery. FORMAT_VERSION bump to 8 causes
// older builds to produce BadVersion. No migration path.
pub const WAL_HOT_UPDATE: u8 = 16;

// WAL cross-page HOT head record (item 71 — cross-page HOT chains).
// Covers the OLD-page changes for a cross-page HOT update atomically:
// (a) stamp xmax = xid on old_slot, and (b) overwrite old_slot's prev_page/
// prev_slot with (new_page_id, new_slot) and set hot_next = HOT_NEXT_XPAGE.
//
// The new version's insert on new_page_id is logged via the existing WAL_INSERT
// in the same mini-txn, giving the whole operation mini-txn atomicity (D2).
// No B-tree update is emitted — the B-tree entry still points at old_slot, which
// chains to the new version via the cross-page forwarding pointer. Vacuum patches
// the B-tree lazily from old_slot → new_slot during reclaimable-slot processing.
//
// WAL header fields: `page_id` = old_page_id; `slot` = old_slot (the chain head,
// where the B-tree still points).
//
// redo payload (old_page_id, 16 B):
//   xid (8 B LE) || old_slot (2 B LE) || new_page_id (4 B LE) || new_slot (2 B LE)
//   Redo: LSN-gated. Stamp xmax=xid; set hot_next=HOT_NEXT_XPAGE; overwrite
//   prev_page/prev_slot with (new_page_id, new_slot) on old_slot.
//
// undo payload (old_page_id, 8 B):
//   old_slot (2 B LE) || saved_prev_page (4 B LE) || saved_prev_slot (2 B LE)
//   Undo: restore prev_page/prev_slot from saved values; clear hot_next =
//   HOT_NEXT_NONE; clear xmax = 0 on old_slot.  The new version (on new_page_id,
//   logged separately as WAL_INSERT) is handled by WAL_INSERT's own undo.
//
// An older binary's `_ => {}` catch-all would skip this record, leaving the
// cross-page HOT chain broken after crash recovery. FORMAT_VERSION bump to 9
// causes older builds to produce BadVersion. No migration path.
pub const WAL_HOT_XPAGE_HEAD: u8 = 17;

// WAL batch insert record (item 79 — Phase B of hot_update_many).
// Replaces N individual WAL_INSERT records (one per row) with ONE record per
// fill page, reducing WAL mutex acquisitions from O(rows) to O(fill_pages).
//
// WAL header fields: `page_id` = fill_page_id; `slot` = u16::MAX (batch sentinel).
//
// redo payload:
//   xmin      u64 LE  — same xid for all rows in the batch
//   n_rows    u16 LE
//   for each row (in slot order):
//     slot      u16 LE
//     prev_page u32 LE  — INVALID_PAGE_ID if no prev-version pointer
//     prev_slot u16 LE
//     data_len  u32 LE
//     data      [u8; data_len]
//   Redo: LSN-gated. For each row whose slot >= page.slot_count_pub(), call
//   insert_versioned(data, xmin, 0, prev). Idempotent: rows already present
//   (slot < slot_count) are skipped.
//
// undo payload:
//   n_slots   u16 LE
//   slot_0 .. slot_N  u16 LE each
//   Undo: page.delete(slot) for each listed slot (reverse order, idempotent).
//
// FORMAT_VERSION bumped to 10 — older builds reject with BadVersion rather
// than silently skipping the record and leaving phantom live rows.
pub const WAL_INSERT_BATCH: u8 = 18;

// WAL batch cross-page HOT head record (item 80 — Phase A of hot_update_many).
// Replaces N individual WAL_HOT_XPAGE_HEAD records (one per row) with ONE
// record per old-page group, reducing Phase A WAL mutex acquisitions from
// O(rows) to O(old_pages).
//
// WAL header fields: `page_id` = old_page_id; `slot` = u16::MAX (batch sentinel).
//
// redo payload:
//   xid       u64 LE
//   n_entries u16 LE
//   for each entry:
//     old_slot      u16 LE
//     new_page_id   u32 LE
//     new_slot      u16 LE
//   Redo: LSN-gated. For each entry: stamp xmax=xid on old_slot; set
//   hot_next=HOT_NEXT_XPAGE; overwrite prev_page/prev_slot with
//   (new_page_id, new_slot). Idempotent (TupleDeleted arm ignored).
//
// undo payload:
//   n_entries u16 LE
//   for each entry (in forward order; undo applies in reverse order):
//     old_slot         u16 LE
//     saved_prev_page  u32 LE
//     saved_prev_slot  u16 LE
//   Undo: for each entry: restore_prev_and_hot_next(saved_prev_page,
//   saved_prev_slot); clear xmax=0 on old_slot. Idempotent.
//
// FORMAT_VERSION 10 required — older builds would skip with `_ => {}`,
// leaving the cross-page HOT chain broken after recovery.
pub const WAL_HOT_XPAGE_BATCH: u8 = 19;

/// Bit 0 of the tuple-header `flags` byte: this slot is a HOT chain head.
/// When set, the `hot_next` field (tuple-header offset [22..24]) holds the
/// slot index of the newer version on the same page. `0xFFFF` means "not set"
/// (used for the new version's own header where no further forwarding exists).
///
/// This flag is ONLY set on the OLD (xmax-stamped) slot. The new slot's
/// `hot_next` is `0xFFFF` (no chain continuation from it).
///
/// Readers: when a B-tree candidate resolves to a slot with HOT_TUPLE_FLAG
/// set and xmax != 0, follow `hot_next` to the new version on the same page,
/// then evaluate MVCC visibility on that new slot.
pub const HOT_TUPLE_FLAG: u8 = 0x01;

/// Sentinel value for `hot_next` meaning "no forwarding" (not a HOT chain head
/// or is the tail of the chain). Stored in the `_pad u16` field at tuple-header
/// offset [22..24].
pub const HOT_NEXT_NONE: u16 = 0xFFFF;

/// Sentinel value for `hot_next` indicating a cross-page HOT chain (item 71).
/// When `hot_next == HOT_NEXT_XPAGE`, the forward target is on a different page;
/// its `(page_id, slot)` are stored in the OLD slot's `prev_page`/`prev_slot`
/// fields, which are safe to repurpose because the backward chain of a
/// superseded version is never traversed during normal forward scans.
pub const HOT_NEXT_XPAGE: u16 = 0xFFFE;

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
