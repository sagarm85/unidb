//! Lock-free observability capture (backlog item 21).
//!
//! The one hard constraint this module exists to satisfy: **metric capture on
//! a hot path must never take a lock.** Every counter here is a plain
//! `AtomicU64`, and the latency histogram ([`AtomicHistogram`]) is a fixed
//! array of `AtomicU64` buckets updated with three `Relaxed` `fetch_add`s — no
//! mutex, no allocation, no syscall. Two threads recording concurrently only
//! ever contend on the atomic word for their own bucket, so the commit path
//! and the scan path stay lock-free (the AC the item is measured against).
//!
//! Reading the histogram back ([`AtomicHistogram::snapshot`]) happens only on
//! the **cold** path — `Engine::stats()` / `GET /stats` / a `/metrics` scrape —
//! so it may do the small O(buckets) walk to estimate percentiles.
//!
//! ## Bucketing
//!
//! Buckets are power-of-two ranges over the recorded value (micros for a
//! latency, but the type is unit-agnostic). Bucket `0` holds the value `0`;
//! bucket `k >= 1` holds `[2^(k-1), 2^k)`. The bucket index of a non-zero `v`
//! is therefore `64 - v.leading_zeros()`, clamped to the last bucket. With
//! [`BUCKETS`] = 48 the top bucket saturates around `2^47` micros (~4.4 years),
//! far beyond any real latency, so nothing is ever lost off the high end.
//!
//! Percentiles are reported as the **upper bound** of the bucket the requested
//! rank falls in (the Prometheus `le` convention): a log-scale over-estimate,
//! never an under-estimate, which is the safe direction for a latency SLO
//! panel. This is an estimate by construction — documented as such in the
//! widget-traceability table — not a precise quantile.

use std::sync::atomic::{AtomicU64, Ordering};

/// Number of power-of-two buckets. 48 covers `[0]` plus `[2^0,2^1) .. [2^46,∞)`
/// — the last bucket is open-ended (clamp target), so no value is ever dropped.
pub const BUCKETS: usize = 48;

/// A lock-free, fixed-memory latency histogram over `u64` values (micros, by
/// convention). Recording is three `Relaxed` atomic adds; it never blocks,
/// allocates, or syscalls, so it is safe to call from the commit/scan hot
/// paths. See the module doc for the bucketing and percentile scheme.
#[derive(Debug)]
pub struct AtomicHistogram {
    buckets: [AtomicU64; BUCKETS],
    /// Total number of recorded samples (also the sum of all bucket counts,
    /// tracked separately so `snapshot` needn't sum the array for the count).
    count: AtomicU64,
    /// Sum of every recorded value, for the arithmetic mean.
    sum: AtomicU64,
}

impl Default for AtomicHistogram {
    fn default() -> Self {
        Self {
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            count: AtomicU64::new(0),
            sum: AtomicU64::new(0),
        }
    }
}

impl AtomicHistogram {
    pub fn new() -> Self {
        Self::default()
    }

    /// The bucket index for a value: `0` for `0`, else `64 - leading_zeros`
    /// clamped to the last (open-ended) bucket.
    #[inline]
    fn index_of(v: u64) -> usize {
        if v == 0 {
            0
        } else {
            ((64 - v.leading_zeros()) as usize).min(BUCKETS - 1)
        }
    }

    /// Record one sample. Lock-free: three `Relaxed` `fetch_add`s. Safe on any
    /// hot path (this is the whole point of the type).
    #[inline]
    pub fn record(&self, value: u64) {
        self.buckets[Self::index_of(value)].fetch_add(1, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum.fetch_add(value, Ordering::Relaxed);
    }

    /// The upper bound (exclusive) of bucket `k` — `0` for bucket 0, else
    /// `2^k`. Used as the reported percentile value (`le` convention).
    #[inline]
    fn bucket_upper(k: usize) -> u64 {
        if k == 0 {
            0
        } else {
            1u64 << k
        }
    }

    /// Take a cold-path snapshot: sample count, mean, and estimated p50/p99.
    ///
    /// A concurrent `record` may land between the reads of the individual
    /// atomics, so the snapshot is only *approximately* internally consistent
    /// — acceptable for an observability gauge, and it never blocks a writer.
    pub fn snapshot(&self) -> HistogramSnapshot {
        let count = self.count.load(Ordering::Relaxed);
        let sum = self.sum.load(Ordering::Relaxed);
        if count == 0 {
            return HistogramSnapshot::default();
        }
        let counts: [u64; BUCKETS] =
            std::array::from_fn(|i| self.buckets[i].load(Ordering::Relaxed));
        HistogramSnapshot {
            count,
            mean_us: sum / count,
            p50_us: Self::percentile(&counts, count, 0.50),
            p99_us: Self::percentile(&counts, count, 0.99),
        }
    }

    /// Walk cumulative bucket counts to the bucket holding rank `ceil(p*count)`,
    /// returning that bucket's upper bound.
    fn percentile(counts: &[u64; BUCKETS], total: u64, p: f64) -> u64 {
        // Rank is 1-based; ceil so p99 of a small sample lands on a real sample.
        let target = ((p * total as f64).ceil() as u64).max(1).min(total);
        let mut cum = 0u64;
        for (k, &c) in counts.iter().enumerate() {
            cum += c;
            if cum >= target {
                return Self::bucket_upper(k);
            }
        }
        Self::bucket_upper(BUCKETS - 1)
    }
}

/// A cold-path readout of an [`AtomicHistogram`]: sample count, arithmetic
/// mean, and estimated p50/p99 (bucket upper bounds — see the module doc).
/// Units are whatever was recorded; the engine records micros, so field names
/// carry the `_us` suffix.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct HistogramSnapshot {
    pub count: u64,
    pub p50_us: u64,
    pub p99_us: u64,
    pub mean_us: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_of_covers_powers_of_two() {
        assert_eq!(AtomicHistogram::index_of(0), 0);
        assert_eq!(AtomicHistogram::index_of(1), 1); // [1,2)
        assert_eq!(AtomicHistogram::index_of(2), 2); // [2,4)
        assert_eq!(AtomicHistogram::index_of(3), 2);
        assert_eq!(AtomicHistogram::index_of(4), 3); // [4,8)
                                                     // Anything astronomically large clamps to the open-ended top bucket.
        assert_eq!(AtomicHistogram::index_of(u64::MAX), BUCKETS - 1);
    }

    #[test]
    fn empty_snapshot_is_zero() {
        let h = AtomicHistogram::new();
        assert_eq!(h.snapshot(), HistogramSnapshot::default());
    }

    #[test]
    fn mean_and_percentiles_track_samples() {
        let h = AtomicHistogram::new();
        // 100 samples all near 10us -> bucket [8,16), upper bound 16.
        for _ in 0..100 {
            h.record(10);
        }
        let s = h.snapshot();
        assert_eq!(s.count, 100);
        assert_eq!(s.mean_us, 10);
        assert_eq!(s.p50_us, 16);
        assert_eq!(s.p99_us, 16);
    }

    #[test]
    fn p99_reaches_the_tail_bucket() {
        let h = AtomicHistogram::new();
        for _ in 0..98 {
            h.record(5); // [4,8) -> upper 8
        }
        for _ in 0..2 {
            h.record(5000); // [4096,8192) -> upper 8192
        }
        let s = h.snapshot();
        assert_eq!(s.count, 100);
        // p50 stays in the low bucket; p99 (rank 99) crosses into the tail
        // bucket where the two slow samples live.
        assert_eq!(s.p50_us, 8);
        assert_eq!(s.p99_us, 8192);
    }
}
