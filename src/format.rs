// On-disk format constants, magic numbers, version, and endian helpers.
// All on-disk integers are little-endian (D9).

pub const MAGIC: u32 = 0x556E4442; // "UnDB"
pub const FORMAT_VERSION: u16 = 1;

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

pub const INVALID_LSN: Lsn = 0;
pub const INVALID_PAGE_ID: PageId = u32::MAX;

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
        assert!(DEFAULT_PAGE_SIZE >= MIN_PAGE_SIZE);
        assert!(DEFAULT_PAGE_SIZE <= MAX_PAGE_SIZE);
        assert!(DEFAULT_PAGE_SIZE.is_power_of_two());
    }
}
