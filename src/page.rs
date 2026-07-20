// Slotted-page layout (D4, D9).
//
// Page header (fixed, little-endian):
//   [0..4]   page_id        u32
//   [4]      page_type      u8
//   [5..7]   _pad           u8 x2
//   [7..8]   _pad           u8
//   [8..12]  crc32          u32   (over entire page with crc field zeroed)
//   [12..20] lsn            u64
//   [20..22] slot_count     u16   (number of slot entries)
//   [22..24] free_start     u16   (first free byte after slot array)
//   [24..26] free_end       u16   (first byte of highest-offset tuple data)
//   [26..28] _pad           u16
// Header total: 28 bytes
//
// After the header: slot array grows up from offset 28.
// Tuple data grows down from the page top.
//
// Each slot entry (4 bytes):
//   [0..2] offset  u16   (byte offset of tuple data from page start; 0 = deleted)
//   [2..4] length  u16   (byte length of the stored record)
//
// Tuple header prepended to every stored record (M1, D4):
//   [0..8]   xmin        u64   (inserting transaction's xid; 0 in M0-era rows)
//   [8..16]  xmax        u64   (deleting/superseding transaction's xid; 0 = live)
//   [16..20] prev_page   u32   (page_id of prior version; INVALID_PAGE_ID = none)
//   [20..22] prev_slot   u16   (slot of prior version)
//   [22..24] hot_next    u16   (item 58 HOT: slot of the new version on the same
//                               page; HOT_NEXT_NONE (0xFFFF) = no forwarding.
//                               When != 0xFFFF this slot is a HOT chain head —
//                               the B-tree still points here but readers must
//                               follow hot_next to find the current visible version.
//                               Previously unused (_pad u16, always 0 in v7 and
//                               earlier). FORMAT_VERSION bumped to 8.)
// Tuple header size: 24 bytes (UNCHANGED — hot_next fits in the _pad bytes)
//
// Item 68 — Hint bits (lazy transaction-state cache):
//   Transaction IDs (xmin/xmax) are u64 values starting at 1 and incrementing
//   sequentially.  The HIGHEST byte of each field (byte [7] for xmin, byte [15]
//   for xmax in the little-endian on-disk layout, i.e. tuple-base + TH_XMIN+7
//   and TH_XMAX+7) is always 0 in practice: it would take 2^56 ≈ 72 quadrillion
//   committed transactions to reach it.  Item 68 repurposes these zero bytes as
//   soft hint-bit caches:
//
//   xmin high byte (TH_XMIN+7):
//     TUPLE_HINT_XMIN_COMMITTED (0x10) — xmin was confirmed committed under a
//         past snapshot; future scans may skip the snapshot.is_committed check.
//     TUPLE_HINT_XMIN_ABORTED   (0x40) — xmin was confirmed aborted; the row
//         is permanently invisible (only set for safety; aborted inserts are
//         physically undone in this engine, so the flag is informational).
//
//   xmax high byte (TH_XMAX+7):
//     TUPLE_HINT_XMAX_COMMITTED (0x20) — xmax was confirmed committed; this
//         version is permanently dead and every future snapshot will see it as
//         deleted.
//
//   All hint-bit reads MUST mask the high byte to recover the clean xid:
//     xid_clean = raw_u64 & XID_MASK
//   The accessor `tuple_header()` applies this mask transparently.
//
//   Hint-bit writes are NOT WAL-logged (they are soft/recomputable derived
//   state — logging them would negate the performance gain and is explicitly
//   forbidden by the item 68 spec).  They are best-effort: if the page cannot
//   be dirtied, the write is silently skipped.  On crash recovery the hint
//   bytes read back as zero, and the engine recomputes them from xid state on
//   the next access, which is correct.

use crate::{
    error::{DbError, Result},
    format::{
        u16_from_le, u16_to_le, u32_from_le, u32_to_le, u64_from_le, u64_to_le, Lsn, PageId, Xid,
        HOT_NEXT_NONE, HOT_NEXT_XPAGE, INVALID_PAGE_ID,
    },
};

pub const PAGE_HEADER_SIZE: usize = 28;
pub const SLOT_SIZE: usize = 4;
pub const TUPLE_HEADER_SIZE: usize = 24;
const CRC_FIELD_OFFSET: usize = 8;

// ── Item 68: Hint-bit constants ───────────────────────────────────────────────
//
// Hint bits live in the HIGH byte of xmin and xmax (the byte at tuple-base +
// TH_XMIN+7 and TH_XMAX+7 in the little-endian encoding).  That byte is
// always zero under normal operation: transaction IDs start at 1 and
// increment sequentially; exhausting the 7-byte range would require 2^56
// (~72 quadrillion) transactions.  All callers that read an xid must apply
// XID_MASK to strip the hint bits and recover the clean value; `tuple_header`
// does this transparently.

/// Mask to strip hint bits from a raw on-disk xid value and recover the
/// clean transaction id.  Applied by `tuple_header()` before returning xmin/xmax.
pub const XID_MASK: u64 = 0x00FF_FFFF_FFFF_FFFF;

/// Hint bit in xmin's high byte: xmin is known committed — this row version
/// was definitively inserted by a committed transaction.  Safe to skip the
/// MVCC snapshot check on future scans.
pub const TUPLE_HINT_XMIN_COMMITTED: u8 = 0x10;

/// Hint bit in xmax's high byte: xmax is known committed — this row version
/// was definitively deleted/superseded by a committed transaction.  The row
/// is permanently dead; safe to skip the xmax snapshot check.
pub const TUPLE_HINT_XMAX_COMMITTED: u8 = 0x20;

/// Hint bit in xmin's high byte: xmin is known aborted — the inserting
/// transaction rolled back.  In this engine aborted inserts are physically
/// undone, so on-disk rows with a committed xmin should not be aborted; this
/// bit exists for completeness and defensive reads.
pub const TUPLE_HINT_XMIN_ABORTED: u8 = 0x40;

/// Line-pointer (slot) lifecycle for MVCC vacuum (M10), encoded in the slot's
/// `(offset, length)` pair without any format change:
///   - `Live`   : `offset != 0` — points at a real tuple body; resolvable.
///   - `Dead`   : `offset == 0, length == SLOT_DEAD_LEN` — tuple body logically
///     dropped, pointer *retained* and **not** reusable yet (a secondary index
///     may still reference this `(page, slot)`; see M10.c). This is the
///     Postgres-style intermediate state that keeps `RowId`s stable across the
///     dangerous window.
///   - `Unused` : `offset == 0, length == 0` — reusable; a new tuple may be
///     handed this slot index (M10.d). Only reached after every secondary
///     index has been proven free of any entry referencing it.
///
/// A real tuple's stored length is always `>= TUPLE_HEADER_SIZE` (24), so
/// `SLOT_DEAD_LEN == 1` can never collide with a live length. The legacy
/// `Page::delete` (offset 0, length 0) produces `Unused`, matching its
/// "reusable, never committed" origin (recovery undo of an incomplete insert).
pub const SLOT_DEAD_LEN: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotState {
    Live,
    Dead,
    Unused,
}

const TH_XMIN: usize = 0;
const TH_XMAX: usize = 8;
const TH_PREV_PAGE: usize = 16;
const TH_PREV_SLOT: usize = 20;
/// Offset of the `hot_next` forwarding-pointer field (item 58).
/// Repurposed from the `_pad u16` at [22..24] (always-zero in v7 and earlier).
const TH_HOT_NEXT: usize = 22;

/// MVCC version-chain metadata read from a tuple header (M1).
/// `hot_next` is `HOT_NEXT_NONE` (0xFFFF) when this slot is not a HOT chain
/// head; otherwise it is the slot index of the newer version on the same page
/// (item 58, v8 format).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TupleHeader {
    pub xmin: Xid,
    pub xmax: Xid,
    pub prev_page: PageId,
    pub prev_slot: u16,
    /// Item 58 HOT: slot of the newer version on the same page, or `HOT_NEXT_NONE`.
    pub hot_next: u16,
}

#[derive(Debug, Clone)]
pub struct SlottedPage {
    data: Vec<u8>,
}

impl SlottedPage {
    pub fn new(page_id: PageId, page_type: u8, page_size: usize) -> Self {
        let mut data = vec![0u8; page_size];
        // write page_id
        data[0..4].copy_from_slice(&u32_to_le(page_id));
        data[4] = page_type;
        // free_start = after header
        let fs = PAGE_HEADER_SIZE as u16;
        data[22..24].copy_from_slice(&u16_to_le(fs));
        // free_end = page_size
        let fe = page_size as u16;
        data[24..26].copy_from_slice(&u16_to_le(fe));
        let mut p = Self { data };
        p.write_crc();
        p
    }

    pub fn from_bytes(raw: Vec<u8>) -> Result<Self> {
        let p = Self { data: raw };
        p.verify_crc()?;
        Ok(p)
    }

    /// Load bytes without CRC check — only for freshly allocated zeroed pages.
    pub fn from_bytes_unchecked(data: Vec<u8>) -> Self {
        Self { data }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }

    pub fn as_bytes_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }

    pub fn page_size(&self) -> usize {
        self.data.len()
    }

    // ── header accessors ────────────────────────────────────────────────────

    pub fn page_id(&self) -> PageId {
        u32_from_le(self.data[0..4].try_into().unwrap())
    }

    pub fn page_type(&self) -> u8 {
        self.data[4]
    }

    pub fn lsn(&self) -> Lsn {
        u64_from_le(self.data[12..20].try_into().unwrap())
    }

    pub fn set_lsn(&mut self, lsn: Lsn) {
        self.data[12..20].copy_from_slice(&u64_to_le(lsn));
        self.write_crc();
    }

    fn slot_count(&self) -> u16 {
        u16_from_le(self.data[20..22].try_into().unwrap())
    }

    fn set_slot_count(&mut self, n: u16) {
        self.data[20..22].copy_from_slice(&u16_to_le(n));
    }

    fn free_start(&self) -> u16 {
        u16_from_le(self.data[22..24].try_into().unwrap())
    }

    fn set_free_start(&mut self, v: u16) {
        self.data[22..24].copy_from_slice(&u16_to_le(v));
    }

    fn free_end(&self) -> u16 {
        u16_from_le(self.data[24..26].try_into().unwrap())
    }

    fn set_free_end(&mut self, v: u16) {
        self.data[24..26].copy_from_slice(&u16_to_le(v));
    }

    pub fn free_space(&self) -> usize {
        let fs = self.free_start() as usize;
        let fe = self.free_end() as usize;
        fe.saturating_sub(fs)
    }

    // ── slot array ──────────────────────────────────────────────────────────

    fn slot_offset_in_page(slot: u16) -> usize {
        PAGE_HEADER_SIZE + slot as usize * SLOT_SIZE
    }

    fn read_slot(&self, slot: u16) -> (u16, u16) {
        let base = Self::slot_offset_in_page(slot);
        let offset = u16_from_le(self.data[base..base + 2].try_into().unwrap());
        let length = u16_from_le(self.data[base + 2..base + 4].try_into().unwrap());
        (offset, length)
    }

    fn write_slot(&mut self, slot: u16, offset: u16, length: u16) {
        let base = Self::slot_offset_in_page(slot);
        self.data[base..base + 2].copy_from_slice(&u16_to_le(offset));
        self.data[base + 2..base + 4].copy_from_slice(&u16_to_le(length));
    }

    // ── insert / read / delete ───────────────────────────────────────────────

    /// Insert `payload` (raw user bytes, no tuple header — we prepend it).
    /// Tuple header is zeroed (xmin/xmax = 0, no prior version) — M0 compat.
    /// Returns the slot index allocated.
    pub fn insert(&mut self, payload: &[u8]) -> Result<u16> {
        self.insert_versioned(payload, 0, 0, None)
    }

    /// Insert `payload` with explicit MVCC tuple-header fields (M1, D4).
    /// `prev` chains to the prior version of this logical row, if any.
    /// Returns the slot index allocated.
    pub fn insert_versioned(
        &mut self,
        payload: &[u8],
        xmin: Xid,
        xmax: Xid,
        prev: Option<(PageId, u16)>,
    ) -> Result<u16> {
        let stored_len = TUPLE_HEADER_SIZE + payload.len();
        // Prefer reusing an UNUSED slot index (M10.d) over appending a new one:
        // a reused slot needs no extra slot-array entry, so it costs only the
        // tuple body's bytes. DEAD slots are deliberately NOT reused — a stale
        // secondary-index entry may still point at them until vacuum's index
        // pass (M10.c) has promoted them to UNUSED.
        let reuse = self.first_unused_slot();
        let needed = if reuse.is_some() {
            stored_len
        } else {
            SLOT_SIZE + stored_len
        };
        if needed > self.free_space() {
            return Err(DbError::HeapFull {
                size: payload.len(),
            });
        }
        // carve space from the top
        let new_fe = self.free_end() as usize - stored_len;
        let th_end = new_fe + TUPLE_HEADER_SIZE;
        self.data[new_fe..th_end].fill(0);
        self.data[new_fe + TH_XMIN..new_fe + TH_XMIN + 8].copy_from_slice(&u64_to_le(xmin));
        self.data[new_fe + TH_XMAX..new_fe + TH_XMAX + 8].copy_from_slice(&u64_to_le(xmax));
        let (prev_page, prev_slot) = prev.unwrap_or((INVALID_PAGE_ID, 0));
        self.data[new_fe + TH_PREV_PAGE..new_fe + TH_PREV_PAGE + 4]
            .copy_from_slice(&u32_to_le(prev_page));
        self.data[new_fe + TH_PREV_SLOT..new_fe + TH_PREV_SLOT + 2]
            .copy_from_slice(&u16_to_le(prev_slot));
        // hot_next: initialise to HOT_NEXT_NONE (no forwarding) for all new inserts.
        self.data[new_fe + TH_HOT_NEXT..new_fe + TH_HOT_NEXT + 2]
            .copy_from_slice(&u16_to_le(HOT_NEXT_NONE));
        // write payload
        self.data[th_end..th_end + payload.len()].copy_from_slice(payload);

        let slot = match reuse {
            Some(s) => {
                self.write_slot(s, new_fe as u16, stored_len as u16);
                self.set_free_end(new_fe as u16);
                s
            }
            None => {
                let sc = self.slot_count();
                self.write_slot(sc, new_fe as u16, stored_len as u16);
                self.set_slot_count(sc + 1);
                self.set_free_end(new_fe as u16);
                self.set_free_start(self.free_start() + SLOT_SIZE as u16);
                sc
            }
        };
        // CRC intentionally NOT written here. Every call site follows the pattern:
        // insert_versioned(...)* → set_lsn(...) → write_page(...), and set_lsn()
        // always calls write_crc() as its final step (same reasoning as set_xmax).
        // Computing CRC per-row (one 8 KiB hash per insert) is pure waste when
        // only the last CRC from set_lsn is ever persisted (item 86).
        Ok(slot)
    }

    /// The lowest UNUSED (reusable) slot index, if any (M10.d slot reuse).
    fn first_unused_slot(&self) -> Option<u16> {
        (0..self.slot_count()).find(|&s| {
            let (offset, length) = self.read_slot(s);
            offset == 0 && length == 0
        })
    }

    /// The MVCC-vacuum lifecycle state of `slot` (M10). See [`SlotState`].
    pub fn slot_state(&self, slot: u16) -> Result<SlotState> {
        if slot >= self.slot_count() {
            return Err(DbError::SlotOutOfRange {
                page_id: self.page_id(),
                slot,
            });
        }
        let (offset, length) = self.read_slot(slot);
        Ok(if offset != 0 {
            SlotState::Live
        } else if length == SLOT_DEAD_LEN {
            SlotState::Dead
        } else {
            SlotState::Unused
        })
    }

    /// Mark a LIVE line pointer DEAD (M10.b): drop the tuple body logically
    /// (the slot stops resolving) but retain the pointer as non-reusable. A
    /// no-op if the slot is already non-live, which keeps redo idempotent
    /// (see recovery.rs's `WAL_VACUUM` handling). The freed body bytes are not
    /// physically reclaimed until [`Self::compact`] (M10.d).
    pub fn mark_dead(&mut self, slot: u16) -> Result<()> {
        if slot >= self.slot_count() {
            return Err(DbError::SlotOutOfRange {
                page_id: self.page_id(),
                slot,
            });
        }
        let (offset, _) = self.read_slot(slot);
        if offset == 0 {
            return Ok(()); // already Dead or Unused — idempotent
        }
        self.write_slot(slot, 0, SLOT_DEAD_LEN);
        self.write_crc();
        Ok(())
    }

    /// Intra-page compaction (M10.d): rebuild the tuple-data region keeping
    /// only LIVE slots — their bodies are copied into a fresh contiguous block
    /// and their slot offsets updated — while every non-live slot (DEAD or
    /// UNUSED) is left as a reusable UNUSED line pointer. `slot_count` is
    /// unchanged so LIVE slot indices stay stable; the DEAD→UNUSED promotion
    /// here is the reuse-gating step, safe only because vacuum has already
    /// cleaned every secondary index of the reclaimed `RowId`s (M10.c).
    /// Returns the number of tuple-body bytes reclaimed.
    pub fn compact(&mut self) -> usize {
        let before = self.free_space();
        let sc = self.slot_count();
        // Snapshot every live slot's body (owned copies, so the rewrite below
        // never reads bytes it has already overwritten).
        let mut live: Vec<(u16, Vec<u8>)> = Vec::new();
        for slot in 0..sc {
            let (offset, length) = self.read_slot(slot);
            if offset != 0 {
                let start = offset as usize;
                live.push((slot, self.data[start..start + length as usize].to_vec()));
            }
        }
        // Reset all slots to UNUSED, then re-lay live bodies down from the top.
        for slot in 0..sc {
            self.write_slot(slot, 0, 0);
        }
        let mut fe = self.data.len();
        for (slot, body) in live {
            fe -= body.len();
            self.data[fe..fe + body.len()].copy_from_slice(&body);
            self.write_slot(slot, fe as u16, body.len() as u16);
        }
        self.set_free_end(fe as u16);
        self.write_crc();
        self.free_space() - before
    }

    /// Read the MVCC tuple-header fields at `slot` (M1).
    ///
    /// Item 68: hint bits are stored in the high bytes of the on-disk xmin/xmax
    /// fields.  This accessor applies `XID_MASK` to both, so callers always
    /// receive clean transaction IDs, never raw bytes with hint flags embedded.
    /// Use `tuple_hint_flags` / `tuple_xmax_hint_flags` to read the hint bits.
    pub fn tuple_header(&self, slot: u16) -> Result<TupleHeader> {
        let sc = self.slot_count();
        if slot >= sc {
            return Err(DbError::SlotOutOfRange {
                page_id: self.page_id(),
                slot,
            });
        }
        let (offset, _) = self.read_slot(slot);
        if offset == 0 {
            return Err(DbError::TupleDeleted {
                page_id: self.page_id(),
                slot,
            });
        }
        let base = offset as usize;
        // XID_MASK strips the hint-bit high byte before returning clean xids.
        let raw_xmin = u64_from_le(
            self.data[base + TH_XMIN..base + TH_XMIN + 8]
                .try_into()
                .unwrap(),
        );
        let raw_xmax = u64_from_le(
            self.data[base + TH_XMAX..base + TH_XMAX + 8]
                .try_into()
                .unwrap(),
        );
        Ok(TupleHeader {
            xmin: raw_xmin & XID_MASK,
            xmax: raw_xmax & XID_MASK,
            prev_page: u32_from_le(
                self.data[base + TH_PREV_PAGE..base + TH_PREV_PAGE + 4]
                    .try_into()
                    .unwrap(),
            ),
            prev_slot: u16_from_le(
                self.data[base + TH_PREV_SLOT..base + TH_PREV_SLOT + 2]
                    .try_into()
                    .unwrap(),
            ),
            hot_next: u16_from_le(
                self.data[base + TH_HOT_NEXT..base + TH_HOT_NEXT + 2]
                    .try_into()
                    .unwrap(),
            ),
        })
    }

    /// Read the hint-bit flags stored in the high byte of `xmin` for `slot`
    /// (item 68).  Returns 0 if the slot is out of range or deleted.  The
    /// caller tests bits against `TUPLE_HINT_XMIN_COMMITTED` /
    /// `TUPLE_HINT_XMIN_ABORTED`.
    pub fn tuple_xmin_hint_flags(&self, slot: u16) -> u8 {
        let sc = self.slot_count();
        if slot >= sc {
            return 0;
        }
        let (offset, _) = self.read_slot(slot);
        if offset == 0 {
            return 0;
        }
        // High byte of the little-endian xmin is at offset TH_XMIN+7.
        self.data[offset as usize + TH_XMIN + 7]
    }

    /// Read the hint-bit flags stored in the high byte of `xmax` for `slot`
    /// (item 68).  Returns 0 if the slot is out of range or deleted.  The
    /// caller tests bits against `TUPLE_HINT_XMAX_COMMITTED`.
    pub fn tuple_xmax_hint_flags(&self, slot: u16) -> u8 {
        let sc = self.slot_count();
        if slot >= sc {
            return 0;
        }
        let (offset, _) = self.read_slot(slot);
        if offset == 0 {
            return 0;
        }
        // High byte of the little-endian xmax is at offset TH_XMAX+7.
        self.data[offset as usize + TH_XMAX + 7]
    }

    /// Set one or more hint bits in the high byte of `xmin` (item 68).
    ///
    /// **No WAL record is emitted.**  Hint bits are soft, recomputable derived
    /// state — logging them would cost a WAL write per scanned page, negating
    /// the performance gain.  They are lost on crash and recomputed on the next
    /// access, which is correct.
    ///
    /// CRC is NOT updated here — the caller pattern is always:
    ///   `set_xmin_hint(...)` → `set_lsn(lsn)` → `write_page(…)`
    /// and `set_lsn` calls `write_crc()` as its last step, covering the hint
    /// bytes.  If called in a standalone context without a following `set_lsn`,
    /// the caller must call `recompute_crc()` before writing the page.
    ///
    /// This is a best-effort operation — callers that cannot hold the page
    /// exclusive latch skip this call silently.
    pub fn set_xmin_hint(&mut self, slot: u16, flag: u8) -> Result<()> {
        let sc = self.slot_count();
        if slot >= sc {
            return Err(DbError::SlotOutOfRange {
                page_id: self.page_id(),
                slot,
            });
        }
        let (offset, _) = self.read_slot(slot);
        if offset == 0 {
            return Err(DbError::TupleDeleted {
                page_id: self.page_id(),
                slot,
            });
        }
        let hint_byte_off = offset as usize + TH_XMIN + 7;
        self.data[hint_byte_off] |= flag;
        // CRC intentionally deferred to set_lsn() — same pattern as set_xmax().
        Ok(())
    }

    /// Set one or more hint bits in the high byte of `xmax` (item 68).
    ///
    /// **No WAL record is emitted** — same rationale as `set_xmin_hint`.
    /// CRC is deferred to `set_lsn()` — same pattern as `set_xmax()`.
    pub fn set_xmax_hint(&mut self, slot: u16, flag: u8) -> Result<()> {
        let sc = self.slot_count();
        if slot >= sc {
            return Err(DbError::SlotOutOfRange {
                page_id: self.page_id(),
                slot,
            });
        }
        let (offset, _) = self.read_slot(slot);
        if offset == 0 {
            return Err(DbError::TupleDeleted {
                page_id: self.page_id(),
                slot,
            });
        }
        let hint_byte_off = offset as usize + TH_XMAX + 7;
        self.data[hint_byte_off] |= flag;
        // CRC intentionally deferred to set_lsn() — same pattern as set_xmax().
        Ok(())
    }

    /// Set the HOT forwarding pointer in the old slot's tuple header (item 58).
    /// `next_slot` is the slot index of the newer version on the same page.
    /// Use `HOT_NEXT_NONE` (0xFFFF) to clear the forwarding pointer.
    pub fn set_hot_next(&mut self, slot: u16, next_slot: u16) -> Result<()> {
        let sc = self.slot_count();
        if slot >= sc {
            return Err(DbError::SlotOutOfRange {
                page_id: self.page_id(),
                slot,
            });
        }
        let (offset, _) = self.read_slot(slot);
        if offset == 0 {
            return Err(DbError::TupleDeleted {
                page_id: self.page_id(),
                slot,
            });
        }
        let base = offset as usize;
        self.data[base + TH_HOT_NEXT..base + TH_HOT_NEXT + 2]
            .copy_from_slice(&u16_to_le(next_slot));
        // CRC deferred to set_lsn() — always called last before write_page().
        Ok(())
    }

    /// Cross-page HOT forwarding pointer (item 71): overwrite the OLD
    /// (xmax-stamped) slot's `prev_page`/`prev_slot` fields with the
    /// cross-page chain target `(xpage_pid, xpage_slot)`, and set
    /// `hot_next = HOT_NEXT_XPAGE (0xFFFE)`.
    ///
    /// **Safety of field reuse**: the old slot is already xmax-stamped
    /// (superseded). Its `prev_page`/`prev_slot` backward-chain fields are
    /// never read during forward scans (only the new version's `prev_ptr`
    /// matters). Repurposing them for the forward chain target avoids any
    /// tuple-header growth (TUPLE_HEADER_SIZE stays 24 B, no version bump
    /// to the on-disk row format).
    ///
    /// Call after `set_xmax`. CRC is deferred to `set_lsn()` like all
    /// other header-field mutators.
    pub fn set_hot_xpage(&mut self, slot: u16, xpage_pid: PageId, xpage_slot: u16) -> Result<()> {
        let sc = self.slot_count();
        if slot >= sc {
            return Err(DbError::SlotOutOfRange {
                page_id: self.page_id(),
                slot,
            });
        }
        let (offset, _) = self.read_slot(slot);
        if offset == 0 {
            return Err(DbError::TupleDeleted {
                page_id: self.page_id(),
                slot,
            });
        }
        let base = offset as usize;
        // Overwrite prev_page with target page id.
        self.data[base + TH_PREV_PAGE..base + TH_PREV_PAGE + 4]
            .copy_from_slice(&u32_to_le(xpage_pid));
        // Overwrite prev_slot with target slot.
        self.data[base + TH_PREV_SLOT..base + TH_PREV_SLOT + 2]
            .copy_from_slice(&u16_to_le(xpage_slot));
        // Set hot_next = HOT_NEXT_XPAGE (cross-page sentinel).
        self.data[base + TH_HOT_NEXT..base + TH_HOT_NEXT + 2]
            .copy_from_slice(&u16_to_le(HOT_NEXT_XPAGE));
        // CRC deferred to set_lsn() (same pattern as set_hot_next).
        Ok(())
    }

    /// Restore `prev_page`/`prev_slot` to their saved values and clear
    /// `hot_next` back to `HOT_NEXT_NONE` — used by `undo_hot_xpage_update`
    /// to reverse a cross-page chain pointer set by `set_hot_xpage` (item 71).
    ///
    /// CRC deferred to `set_lsn()` — always called last before `write_page`.
    pub fn restore_prev_and_hot_next(
        &mut self,
        slot: u16,
        saved_prev_page: PageId,
        saved_prev_slot: u16,
    ) -> Result<()> {
        let sc = self.slot_count();
        if slot >= sc {
            return Err(DbError::SlotOutOfRange {
                page_id: self.page_id(),
                slot,
            });
        }
        let (offset, _) = self.read_slot(slot);
        if offset == 0 {
            return Err(DbError::TupleDeleted {
                page_id: self.page_id(),
                slot,
            });
        }
        let base = offset as usize;
        self.data[base + TH_PREV_PAGE..base + TH_PREV_PAGE + 4]
            .copy_from_slice(&u32_to_le(saved_prev_page));
        self.data[base + TH_PREV_SLOT..base + TH_PREV_SLOT + 2]
            .copy_from_slice(&u16_to_le(saved_prev_slot));
        self.data[base + TH_HOT_NEXT..base + TH_HOT_NEXT + 2]
            .copy_from_slice(&u16_to_le(HOT_NEXT_NONE));
        Ok(())
    }

    /// Stamp `xmax` on the tuple at `slot` in place (M1 DELETE / UPDATE
    /// superseding a version). Always fits — it's a fixed-size header field.
    pub fn set_xmax(&mut self, slot: u16, xmax: Xid) -> Result<()> {
        let sc = self.slot_count();
        if slot >= sc {
            return Err(DbError::SlotOutOfRange {
                page_id: self.page_id(),
                slot,
            });
        }
        let (offset, _) = self.read_slot(slot);
        if offset == 0 {
            return Err(DbError::TupleDeleted {
                page_id: self.page_id(),
                slot,
            });
        }
        let base = offset as usize;
        self.data[base + TH_XMAX..base + TH_XMAX + 8].copy_from_slice(&u64_to_le(xmax));
        // CRC is intentionally NOT updated here. Every call site follows the
        // pattern: set_xmax(...)* → set_lsn(...) → write_page(...), and
        // set_lsn() always calls write_crc() as its final step. Computing CRC
        // after each slot stamp is pure waste — with ~104 rows/page it means
        // 104 × (8 KB clone + hash) per page group in delete_many, where only
        // the last CRC (from set_lsn) is ever persisted.
        Ok(())
    }

    /// Read the payload at `slot` (strips tuple header).
    pub fn get(&self, slot: u16) -> Result<&[u8]> {
        let sc = self.slot_count();
        if slot >= sc {
            return Err(DbError::SlotOutOfRange {
                page_id: self.page_id(),
                slot,
            });
        }
        let (offset, length) = self.read_slot(slot);
        if offset == 0 {
            return Err(DbError::TupleDeleted {
                page_id: self.page_id(),
                slot,
            });
        }
        let start = offset as usize + TUPLE_HEADER_SIZE;
        let end = offset as usize + length as usize;
        Ok(&self.data[start..end])
    }

    /// Mark slot as deleted (zero out offset). Does not compact.
    pub fn delete(&mut self, slot: u16) -> Result<()> {
        let sc = self.slot_count();
        if slot >= sc {
            return Err(DbError::SlotOutOfRange {
                page_id: self.page_id(),
                slot,
            });
        }
        let (offset, _) = self.read_slot(slot);
        if offset == 0 {
            return Err(DbError::TupleDeleted {
                page_id: self.page_id(),
                slot,
            });
        }
        self.write_slot(slot, 0, 0);
        self.write_crc();
        Ok(())
    }

    /// In-place update for M0 (D4 — forward-compatible, same slot).
    /// New payload must fit in the existing stored area.
    pub fn update(&mut self, slot: u16, payload: &[u8]) -> Result<()> {
        let sc = self.slot_count();
        if slot >= sc {
            return Err(DbError::SlotOutOfRange {
                page_id: self.page_id(),
                slot,
            });
        }
        let (offset, length) = self.read_slot(slot);
        if offset == 0 {
            return Err(DbError::TupleDeleted {
                page_id: self.page_id(),
                slot,
            });
        }
        let stored_payload_len = length as usize - TUPLE_HEADER_SIZE;
        if payload.len() > stored_payload_len {
            // Payload grew — delete old and insert new (may change slot).
            // For M0 in-place is fine when size fits; if not, caller must
            // handle by delete+insert on a potentially different slot.
            return Err(DbError::HeapFull {
                size: payload.len(),
            });
        }
        let payload_start = offset as usize + TUPLE_HEADER_SIZE;
        self.data[payload_start..payload_start + payload.len()].copy_from_slice(payload);
        // Update stored length to actual size used.
        self.write_slot(slot, offset, (TUPLE_HEADER_SIZE + payload.len()) as u16);
        self.write_crc();
        Ok(())
    }

    pub fn slot_count_pub(&self) -> u16 {
        self.slot_count()
    }

    // ── CRC ─────────────────────────────────────────────────────────────────

    /// Recompute and write the page CRC. Called explicitly by recovery paths
    /// that mutate a page via `set_xmax` without subsequently calling
    /// `set_lsn` (which owns the CRC update on the normal write path).
    pub fn recompute_crc(&mut self) {
        self.write_crc();
    }

    fn compute_crc(&self) -> u32 {
        // Allocation-free incremental CRC: hash the three page regions that
        // surround the CRC field, treating the field bytes themselves as zero.
        // Avoids the 8 KiB clone+hash that the previous implementation performed
        // on every call (item 86).
        let mut h = crc32fast::Hasher::new();
        h.update(&self.data[..CRC_FIELD_OFFSET]);
        h.update(&[0u8; 4]); // CRC field treated as zero while computing
        h.update(&self.data[CRC_FIELD_OFFSET + 4..]);
        h.finalize()
    }

    fn write_crc(&mut self) {
        let crc = self.compute_crc();
        self.data[CRC_FIELD_OFFSET..CRC_FIELD_OFFSET + 4].copy_from_slice(&u32_to_le(crc));
    }

    pub fn verify_crc(&self) -> Result<()> {
        let stored = u32_from_le(
            self.data[CRC_FIELD_OFFSET..CRC_FIELD_OFFSET + 4]
                .try_into()
                .unwrap(),
        );
        let computed = self.compute_crc();
        if stored != computed {
            Err(DbError::ChecksumMismatch {
                page_id: self.page_id(),
            })
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{DEFAULT_PAGE_SIZE, PAGE_TYPE_HEAP};

    fn make_page() -> SlottedPage {
        SlottedPage::new(1, PAGE_TYPE_HEAP, DEFAULT_PAGE_SIZE as usize)
    }

    #[test]
    fn insert_and_get() {
        let mut p = make_page();
        let slot = p.insert(b"hello").unwrap();
        assert_eq!(p.get(slot).unwrap(), b"hello");
    }

    #[test]
    fn multiple_inserts() {
        let mut p = make_page();
        let s0 = p.insert(b"foo").unwrap();
        let s1 = p.insert(b"bar").unwrap();
        assert_eq!(p.get(s0).unwrap(), b"foo");
        assert_eq!(p.get(s1).unwrap(), b"bar");
    }

    #[test]
    fn delete_then_get_fails() {
        let mut p = make_page();
        let s = p.insert(b"data").unwrap();
        p.delete(s).unwrap();
        assert!(matches!(p.get(s), Err(DbError::TupleDeleted { .. })));
    }

    #[test]
    fn update_in_place() {
        let mut p = make_page();
        let s = p.insert(b"old_data").unwrap();
        p.update(s, b"new").unwrap();
        assert_eq!(p.get(s).unwrap(), b"new");
    }

    #[test]
    fn crc_round_trip() {
        let p = make_page();
        let raw = p.as_bytes().to_vec();
        let p2 = SlottedPage::from_bytes(raw).unwrap();
        assert_eq!(p2.page_id(), 1);
    }

    #[test]
    fn corrupt_crc_rejected() {
        let mut p = make_page();
        p.data[0] ^= 0xff;
        // bypass from_bytes and call verify directly
        assert!(p.verify_crc().is_err());
    }

    #[test]
    fn free_space_decreases() {
        let mut p = make_page();
        let before = p.free_space();
        p.insert(b"payload").unwrap();
        assert!(p.free_space() < before);
    }

    #[test]
    fn insert_zeroes_header_by_default() {
        let mut p = make_page();
        let s = p.insert(b"data").unwrap();
        let th = p.tuple_header(s).unwrap();
        assert_eq!(th.xmin, 0);
        assert_eq!(th.xmax, 0);
        assert_eq!(th.prev_page, crate::format::INVALID_PAGE_ID);
        assert_eq!(th.prev_slot, 0);
    }

    #[test]
    fn insert_versioned_round_trip() {
        let mut p = make_page();
        let s = p.insert_versioned(b"row-v1", 42, 0, None).unwrap();
        let th = p.tuple_header(s).unwrap();
        assert_eq!(th.xmin, 42);
        assert_eq!(th.xmax, 0);
        assert_eq!(th.prev_page, crate::format::INVALID_PAGE_ID);
        assert_eq!(p.get(s).unwrap(), b"row-v1");

        let s2 = p.insert_versioned(b"row-v2", 43, 0, Some((1, s))).unwrap();
        let th2 = p.tuple_header(s2).unwrap();
        assert_eq!(th2.xmin, 43);
        assert_eq!(th2.prev_page, 1);
        assert_eq!(th2.prev_slot, s);
    }

    #[test]
    fn set_xmax_stamps_in_place() {
        let mut p = make_page();
        let s = p.insert_versioned(b"data", 1, 0, None).unwrap();
        p.set_xmax(s, 7).unwrap();
        let th = p.tuple_header(s).unwrap();
        assert_eq!(th.xmin, 1);
        assert_eq!(th.xmax, 7);
        // payload untouched
        assert_eq!(p.get(s).unwrap(), b"data");
    }

    #[test]
    fn tuple_header_size_is_24_bytes() {
        assert_eq!(TUPLE_HEADER_SIZE, 24);
    }

    // ── M10: slot lifecycle (LIVE → DEAD → UNUSED), compaction, reuse ─────────

    #[test]
    fn fresh_slot_is_live_and_mark_dead_transitions() {
        let mut p = make_page();
        let s = p.insert_versioned(b"row", 1, 0, None).unwrap();
        assert_eq!(p.slot_state(s).unwrap(), SlotState::Live);
        p.mark_dead(s).unwrap();
        assert_eq!(p.slot_state(s).unwrap(), SlotState::Dead);
        // A DEAD slot no longer resolves.
        assert!(matches!(p.get(s), Err(DbError::TupleDeleted { .. })));
        // mark_dead is idempotent.
        p.mark_dead(s).unwrap();
        assert_eq!(p.slot_state(s).unwrap(), SlotState::Dead);
    }

    #[test]
    fn legacy_delete_produces_unused_slot() {
        let mut p = make_page();
        let s = p.insert(b"row").unwrap();
        p.delete(s).unwrap();
        assert_eq!(p.slot_state(s).unwrap(), SlotState::Unused);
    }

    #[test]
    fn dead_slots_are_not_reused_but_unused_slots_are() {
        let mut p = make_page();
        let s0 = p.insert_versioned(b"a", 1, 0, None).unwrap();
        let s1 = p.insert_versioned(b"b", 1, 0, None).unwrap();
        assert_eq!((s0, s1), (0, 1));
        // Mark s0 DEAD: a new insert must NOT reuse it (it may still be
        // index-referenced), so it appends a fresh slot instead.
        p.mark_dead(s0).unwrap();
        let s2 = p.insert_versioned(b"c", 1, 0, None).unwrap();
        assert_eq!(s2, 2, "DEAD slot must not be reused");
        // Now compaction promotes DEAD→UNUSED; the next insert reuses slot 0.
        p.compact();
        assert_eq!(p.slot_state(s0).unwrap(), SlotState::Unused);
        let s3 = p.insert_versioned(b"d", 1, 0, None).unwrap();
        assert_eq!(s3, 0, "UNUSED slot must be reused");
        assert_eq!(p.get(s3).unwrap(), b"d");
    }

    #[test]
    fn compact_reclaims_dead_body_space_and_keeps_live_rows() {
        let mut p = make_page();
        let big = vec![b'x'; 500];
        let s0 = p.insert_versioned(&big, 1, 0, None).unwrap();
        let s1 = p.insert_versioned(b"keep", 1, 0, None).unwrap();
        let free_before = p.free_space();
        p.mark_dead(s0).unwrap();
        let reclaimed = p.compact();
        assert!(reclaimed >= 500, "must reclaim the big dead body");
        assert!(p.free_space() > free_before);
        // The surviving row is intact and still at its stable slot index.
        assert_eq!(p.slot_state(s1).unwrap(), SlotState::Live);
        assert_eq!(p.get(s1).unwrap(), b"keep");
        assert_eq!(p.slot_state(s0).unwrap(), SlotState::Unused);
    }

    // ── Item 68: hint-bit unit tests ─────────────────────────────────────────

    /// Hint bits survive a set_xmin_hint + CRC round-trip and do NOT corrupt
    /// the stored xid value (tuple_header applies XID_MASK transparently).
    #[test]
    fn hint_xmin_set_and_cleared_by_mask() {
        let mut p = make_page();
        let s = p.insert_versioned(b"row", 42, 0, None).unwrap();
        // Before setting hint: xmin reads back clean, hint flags are 0.
        let th = p.tuple_header(s).unwrap();
        assert_eq!(th.xmin, 42, "xmin must be clean before hint set");
        assert_eq!(
            p.tuple_xmin_hint_flags(s),
            0,
            "hint flags must be zero on fresh insert"
        );
        // Set HINT_XMIN_COMMITTED — simulates what the delete path does.
        p.set_xmin_hint(s, TUPLE_HINT_XMIN_COMMITTED).unwrap();
        // xmin must still read 42 — XID_MASK strips the hint byte.
        let th2 = p.tuple_header(s).unwrap();
        assert_eq!(th2.xmin, 42, "xmin must read clean after hint set");
        assert_eq!(
            p.tuple_xmin_hint_flags(s),
            TUPLE_HINT_XMIN_COMMITTED,
            "hint flag must be readable via tuple_xmin_hint_flags"
        );
        // Payload is untouched.
        assert_eq!(p.get(s).unwrap(), b"row");
    }

    /// Hint bits survive a CRC round-trip: write the page bytes out and back
    /// in; verify_crc must pass, and the hint byte is preserved.
    #[test]
    fn hint_bit_crc_round_trip() {
        let mut p = make_page();
        let s = p.insert_versioned(b"data", 7, 0, None).unwrap();
        // Simulate set_lsn which is called after set_xmin_hint in the write path.
        p.set_xmin_hint(s, TUPLE_HINT_XMIN_COMMITTED).unwrap();
        // Recompute CRC as the write path would (via set_lsn or recompute_crc).
        p.recompute_crc();
        // Round-trip through from_bytes (verifies CRC).
        let raw = p.as_bytes().to_vec();
        let p2 = SlottedPage::from_bytes(raw).unwrap();
        assert_eq!(p2.tuple_header(s).unwrap().xmin, 7);
        assert_eq!(p2.tuple_xmin_hint_flags(s), TUPLE_HINT_XMIN_COMMITTED);
    }

    /// Setting both xmin and xmax hint bits is independent; neither corrupts
    /// the other.
    #[test]
    fn hint_xmin_and_xmax_independent() {
        let mut p = make_page();
        let s = p.insert_versioned(b"v", 10, 3, None).unwrap();
        p.set_xmin_hint(s, TUPLE_HINT_XMIN_COMMITTED).unwrap();
        p.set_xmax_hint(s, TUPLE_HINT_XMAX_COMMITTED).unwrap();
        p.recompute_crc();
        assert_eq!(p.tuple_header(s).unwrap().xmin, 10);
        assert_eq!(p.tuple_header(s).unwrap().xmax, 3);
        assert_eq!(p.tuple_xmin_hint_flags(s), TUPLE_HINT_XMIN_COMMITTED);
        assert_eq!(p.tuple_xmax_hint_flags(s), TUPLE_HINT_XMAX_COMMITTED);
    }
}
