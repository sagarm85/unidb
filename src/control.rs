// Control file (D3): single meta-page/file holding magic, version, page_size,
// last-checkpoint LSN, and WAL tail pointer. Recovery starts by reading this.
//
// Layout (all little-endian, D9):
//   [0..4]   magic          u32
//   [4..6]   format_version u16
//   [6..8]   _pad           u16
//   [8..12]  page_size      u32
//   [12..16] _pad           u32
//   [16..24] checkpoint_lsn u64
//   [24..32] wal_tail_lsn   u64
//   [32..36] crc32          u32   (over bytes [0..32])
// Total: 36 bytes

use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
};

use crate::{
    error::{DbError, Result},
    format::{
        u16_from_le, u16_to_le, u32_from_le, u32_to_le, u64_from_le, u64_to_le, DEFAULT_PAGE_SIZE,
        FORMAT_VERSION, INVALID_LSN, MAGIC, MAX_PAGE_SIZE, MIN_PAGE_SIZE,
    },
};

const CONTROL_SIZE: usize = 36;
const CRC_PAYLOAD_LEN: usize = 32;

#[derive(Debug, Clone)]
pub struct ControlData {
    pub page_size: u32,
    pub checkpoint_lsn: u64,
    pub wal_tail_lsn: u64,
}

impl ControlData {
    pub fn new(page_size: u32) -> Self {
        Self {
            page_size,
            checkpoint_lsn: INVALID_LSN,
            wal_tail_lsn: INVALID_LSN,
        }
    }
}

fn encode(cd: &ControlData) -> [u8; CONTROL_SIZE] {
    let mut buf = [0u8; CONTROL_SIZE];
    buf[0..4].copy_from_slice(&u32_to_le(MAGIC));
    buf[4..6].copy_from_slice(&u16_to_le(FORMAT_VERSION));
    // [6..8] pad
    buf[8..12].copy_from_slice(&u32_to_le(cd.page_size));
    // [12..16] pad
    buf[16..24].copy_from_slice(&u64_to_le(cd.checkpoint_lsn));
    buf[24..32].copy_from_slice(&u64_to_le(cd.wal_tail_lsn));
    let crc = crc32fast::hash(&buf[..CRC_PAYLOAD_LEN]);
    buf[32..36].copy_from_slice(&u32_to_le(crc));
    buf
}

fn decode(buf: &[u8; CONTROL_SIZE]) -> Result<ControlData> {
    let stored_crc = u32_from_le(buf[32..36].try_into().unwrap());
    let computed_crc = crc32fast::hash(&buf[..CRC_PAYLOAD_LEN]);
    if stored_crc != computed_crc {
        return Err(DbError::ControlFileCorrupt(format!(
            "CRC mismatch: stored {stored_crc:#010x}, computed {computed_crc:#010x}"
        )));
    }
    let magic = u32_from_le(buf[0..4].try_into().unwrap());
    if magic != MAGIC {
        return Err(DbError::BadMagic {
            expected: MAGIC,
            got: magic,
        });
    }
    let version = u16_from_le(buf[4..6].try_into().unwrap());
    if version != FORMAT_VERSION {
        return Err(DbError::BadVersion(version));
    }
    let page_size = u32_from_le(buf[8..12].try_into().unwrap());
    if !(MIN_PAGE_SIZE..=MAX_PAGE_SIZE).contains(&page_size) || !page_size.is_power_of_two() {
        return Err(DbError::BadPageSize(page_size));
    }
    let checkpoint_lsn = u64_from_le(buf[16..24].try_into().unwrap());
    let wal_tail_lsn = u64_from_le(buf[24..32].try_into().unwrap());
    Ok(ControlData {
        page_size,
        checkpoint_lsn,
        wal_tail_lsn,
    })
}

pub fn create(path: &Path, page_size: u32) -> Result<ControlData> {
    if !(MIN_PAGE_SIZE..=MAX_PAGE_SIZE).contains(&page_size) || !page_size.is_power_of_two() {
        return Err(DbError::BadPageSize(page_size));
    }
    let cd = ControlData::new(page_size);
    let buf = encode(&cd);
    let mut f = File::create(path)?;
    f.write_all(&buf)?;
    f.flush()?;
    tracing::info!(
        path = %path.display(),
        page_size,
        "control file created"
    );
    Ok(cd)
}

pub fn read(path: &Path) -> Result<ControlData> {
    let mut f = File::open(path)?;
    let mut buf = [0u8; CONTROL_SIZE];
    f.read_exact(&mut buf)?;
    let cd = decode(&buf)?;
    tracing::debug!(
        path = %path.display(),
        checkpoint_lsn = cd.checkpoint_lsn,
        wal_tail_lsn = cd.wal_tail_lsn,
        "control file read"
    );
    Ok(cd)
}

pub fn write(path: &Path, cd: &ControlData) -> Result<()> {
    let buf = encode(cd);
    let mut f = OpenOptions::new().write(true).open(path)?;
    f.seek(SeekFrom::Start(0))?;
    f.write_all(&buf)?;
    f.flush()?;
    tracing::debug!(
        checkpoint_lsn = cd.checkpoint_lsn,
        wal_tail_lsn = cd.wal_tail_lsn,
        "control file updated"
    );
    Ok(())
}

pub fn open_or_create(path: &Path, page_size: u32) -> Result<ControlData> {
    if path.exists() {
        read(path)
    } else {
        let ps = if page_size == 0 { DEFAULT_PAGE_SIZE } else { page_size };
        create(path, ps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn create_read_roundtrip() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("control");
        let cd = create(&p, DEFAULT_PAGE_SIZE).unwrap();
        assert_eq!(cd.page_size, DEFAULT_PAGE_SIZE);
        assert_eq!(cd.checkpoint_lsn, INVALID_LSN);
        let cd2 = read(&p).unwrap();
        assert_eq!(cd2.page_size, cd.page_size);
        assert_eq!(cd2.checkpoint_lsn, cd.checkpoint_lsn);
    }

    #[test]
    fn update_and_reread() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("control");
        let mut cd = create(&p, DEFAULT_PAGE_SIZE).unwrap();
        cd.checkpoint_lsn = 42;
        cd.wal_tail_lsn = 99;
        write(&p, &cd).unwrap();
        let cd2 = read(&p).unwrap();
        assert_eq!(cd2.checkpoint_lsn, 42);
        assert_eq!(cd2.wal_tail_lsn, 99);
    }

    #[test]
    fn corrupt_crc_rejected() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("control");
        create(&p, DEFAULT_PAGE_SIZE).unwrap();
        let mut bytes = std::fs::read(&p).unwrap();
        bytes[0] ^= 0xff; // corrupt magic byte
        std::fs::write(&p, &bytes).unwrap();
        assert!(read(&p).is_err());
    }

    #[test]
    fn bad_page_size_rejected() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("control");
        assert!(create(&p, 1234).is_err()); // not power-of-two
        assert!(create(&p, 1024).is_err()); // too small
    }
}
