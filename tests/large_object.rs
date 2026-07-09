// Large-object storage (P3.d): store + stream a big value out-of-line, atomic
// with the transaction, streamed without holding the whole blob, and vacuum-
// reclaimable. Crash recovery is covered by the crash harness (P16).

use std::io::Write;

use tempfile::tempdir;
use unidb::Engine;

/// Deterministic blob byte at position `i` — lets a test generate and verify a
/// multi-MB value without ever materializing two copies of it.
fn blob_byte(i: usize) -> u8 {
    ((i * 2654435761) >> 13) as u8
}

/// A `Write` sink that never keeps the bytes — it just folds them into a rolling
/// checksum + length, so verifying a multi-GB read needs O(1) memory (proving
/// the engine streamed, and letting the test avoid holding the blob either).
struct ChecksumSink {
    len: u64,
    hash: u64,
}
impl Write for ChecksumSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        for &b in buf {
            self.hash = self.hash.wrapping_mul(1099511628211) ^ b as u64;
            self.len += 1;
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn expected_checksum(n: usize) -> (u64, u64) {
    let mut hash = 0u64;
    for i in 0..n {
        hash = hash.wrapping_mul(1099511628211) ^ blob_byte(i) as u64;
    }
    (n as u64, hash)
}

/// A `Read` that produces `n` deterministic bytes without allocating them.
struct BlobReader {
    pos: usize,
    n: usize,
}
impl std::io::Read for BlobReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let remaining = self.n - self.pos;
        if remaining == 0 {
            return Ok(0);
        }
        let take = remaining.min(buf.len());
        for (j, slot) in buf[..take].iter_mut().enumerate() {
            *slot = blob_byte(self.pos + j);
        }
        self.pos += take;
        Ok(take)
    }
}

#[test]
fn store_and_stream_large_blob_roundtrips() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    // 5 MiB ≈ 750 chunks across many heap pages — proves chunking + streaming.
    // The mechanism is identical at multi-GB; only one chunk is ever resident.
    let n = 5 * 1024 * 1024usize;

    let xid = engine.begin().unwrap();
    let lob_id = engine
        .put_large_object(xid, BlobReader { pos: 0, n })
        .unwrap();
    engine.commit(xid).unwrap();

    let rx = engine.begin().unwrap();
    let mut sink = ChecksumSink { len: 0, hash: 0 };
    let written = engine.read_large_object(rx, lob_id, &mut sink).unwrap();
    engine.commit(rx).unwrap();

    let (exp_len, exp_hash) = expected_checksum(n);
    assert_eq!(written, exp_len, "byte count must round-trip");
    assert_eq!(sink.len, exp_len);
    assert_eq!(sink.hash, exp_hash, "content must round-trip exactly");
}

#[test]
fn large_object_is_atomic_with_transaction() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    let n = 200 * 1024usize; // a few dozen chunks

    // A blob written in an aborted transaction must be invisible afterward.
    let doomed = engine.begin().unwrap();
    let lob_id = engine
        .put_large_object(doomed, BlobReader { pos: 0, n })
        .unwrap();
    engine.abort(doomed).unwrap();

    let rx = engine.begin().unwrap();
    let mut sink = ChecksumSink { len: 0, hash: 0 };
    let written = engine.read_large_object(rx, lob_id, &mut sink).unwrap();
    assert_eq!(
        written, 0,
        "an aborted blob's chunks must be MVCC-invisible"
    );
    engine.commit(rx).unwrap();

    // A committed blob is fully visible.
    let good = engine.begin().unwrap();
    let lob2 = engine
        .put_large_object(good, BlobReader { pos: 0, n })
        .unwrap();
    engine.commit(good).unwrap();
    let rx2 = engine.begin().unwrap();
    let mut sink2 = ChecksumSink { len: 0, hash: 0 };
    let written2 = engine.read_large_object(rx2, lob2, &mut sink2).unwrap();
    assert_eq!(written2, n as u64);
    let (_, exp_hash) = expected_checksum(n);
    assert_eq!(sink2.hash, exp_hash);
}

#[test]
fn vacuum_reclaims_deleted_large_object_chunks() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    let n = 400 * 1024usize;

    let xid = engine.begin().unwrap();
    let lob_id = engine
        .put_large_object(xid, BlobReader { pos: 0, n })
        .unwrap();
    engine.commit(xid).unwrap();

    let dx = engine.begin().unwrap();
    let deleted = engine.delete_large_object(dx, lob_id).unwrap();
    engine.commit(dx).unwrap();
    assert!(deleted > 1, "a 400 KiB blob is many chunks: {deleted}");

    // After delete + vacuum, the chunk rows are physically reclaimed.
    let report = engine.vacuum().unwrap();
    assert!(
        report.versions_reclaimed >= deleted,
        "vacuum must reclaim the deleted chunk rows: {report:?}"
    );

    // The blob reads back empty now.
    let rx = engine.begin().unwrap();
    let mut sink = ChecksumSink { len: 0, hash: 0 };
    let written = engine.read_large_object(rx, lob_id, &mut sink).unwrap();
    assert_eq!(written, 0, "deleted blob must read empty");
}
