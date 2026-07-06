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
// Tuple header prepended to every stored record:
//   [0..8]  xmin   u64   (reserved for MVCC, zero in M0)
//   [8..16] xmax   u64   (reserved for MVCC, zero in M0)
// Tuple header size: 16 bytes

use crate::{
    error::{DbError, Result},
    format::{u16_from_le, u16_to_le, u32_from_le, u32_to_le, u64_from_le, u64_to_le, Lsn, PageId},
};

pub const PAGE_HEADER_SIZE: usize = 28;
pub const SLOT_SIZE: usize = 4;
pub const TUPLE_HEADER_SIZE: usize = 16;
const CRC_FIELD_OFFSET: usize = 8;

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
    /// Returns the slot index allocated.
    pub fn insert(&mut self, payload: &[u8]) -> Result<u16> {
        let stored_len = TUPLE_HEADER_SIZE + payload.len();
        let needed = SLOT_SIZE + stored_len;
        if needed > self.free_space() {
            return Err(DbError::HeapFull { size: payload.len() });
        }
        // carve space from the top
        let new_fe = self.free_end() as usize - stored_len;
        // write tuple header (xmin/xmax = 0 in M0, D4)
        let th_end = new_fe + TUPLE_HEADER_SIZE;
        self.data[new_fe..th_end].fill(0);
        // write payload
        self.data[th_end..th_end + payload.len()].copy_from_slice(payload);

        let sc = self.slot_count();
        self.write_slot(sc, new_fe as u16, stored_len as u16);
        self.set_slot_count(sc + 1);
        self.set_free_end(new_fe as u16);
        self.set_free_start(self.free_start() + SLOT_SIZE as u16);
        self.write_crc();
        Ok(sc)
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
            return Err(DbError::HeapFull { size: payload.len() });
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

    fn compute_crc(&self) -> u32 {
        let mut buf = self.data.clone();
        buf[CRC_FIELD_OFFSET..CRC_FIELD_OFFSET + 4].fill(0);
        crc32fast::hash(&buf)
    }

    fn write_crc(&mut self) {
        let crc = self.compute_crc();
        self.data[CRC_FIELD_OFFSET..CRC_FIELD_OFFSET + 4].copy_from_slice(&u32_to_le(crc));
    }

    pub fn verify_crc(&self) -> Result<()> {
        let stored = u32_from_le(self.data[CRC_FIELD_OFFSET..CRC_FIELD_OFFSET + 4].try_into().unwrap());
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
}
